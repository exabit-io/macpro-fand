//! Command-line entry point, control loop, and ownership-preserving recovery.
use crate::{
    budget::{self, Budget, Telemetry},
    config::{Config, FanMode, Override},
    control::{self, FanHealth, Filter, Governor},
    hardware::{
        self, DcReader, Die, DieSpec, Fans, FsPowerIo, GpuReader, GpuSample, Paths, Sensors,
    },
    literal::Value,
    os::{self, Lock, Services, Systemd},
    power::{call_deadline, DeadlineIo, OwnedIo, PowerWorker, Status},
    transaction, CAPS, GPU_STALE, PERIOD, PL2, RAPL,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::PathBuf,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
const NOHW: i32 = 78;
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Control,
    DryRun,
    Release,
    Stopped,
    CheckConfig,
    Version,
    Help,
}
#[derive(Clone, Debug)]
pub struct Options {
    pub mode: Mode,
    pub config: PathBuf,
}
impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut opt = Self {
            mode: Mode::Control,
            config: "/etc/macpro-fand.conf".into(),
        };
        let mut set = false;
        let mut args = args.into_iter();
        while let Some(a) = args.next() {
            let mode = match a.as_str() {
                "--dry-run" => Some(Mode::DryRun),
                "--release" => Some(Mode::Release),
                "--stopped" => Some(Mode::Stopped),
                "--check-config" => Some(Mode::CheckConfig),
                "--version" => {
                    return Ok(Self {
                        mode: Mode::Version,
                        ..opt
                    })
                }
                "--help" | "-h" => {
                    return Ok(Self {
                        mode: Mode::Help,
                        ..opt
                    })
                }
                "--config" => {
                    opt.config = args.next().ok_or("--config needs a path")?.into();
                    None
                }
                _ if a.starts_with("--config=") => {
                    opt.config = a[9..].into();
                    None
                }
                _ => return Err(format!("unknown option {a}")),
            };
            if let Some(mode) = mode {
                if set {
                    return Err(
                        "select only one of --dry-run, --release, --stopped, --check-config".into(),
                    );
                }
                opt.mode = mode;
                set = true;
            }
        }
        Ok(opt)
    }
}
const HELP:&str="macpro-fand: Mac Pro 7,1 zone fan control and power budget\n\nUsage: macpro-fand [--config PATH] [--dry-run | --release | --stopped | --check-config]\n  --dry-run       Compute and log; write no fan or power limit\n  --release       Confirm static-safe limits and hand fans to t2fanrd\n  --stopped       systemd ExecStopPost recovery hook\n  --check-config  Validate configuration and hardware budget, then exit\n  --version       Print version\n";
fn error(message: impl Into<String>) {
    crate::log(&[("error", message.into().into())]);
}
fn note(message: impl Into<String>) {
    crate::log(&[("budget_note", message.into().into())]);
}
fn r1(v: Option<f64>) -> Value {
    v.map(|v| control::round_even(v * 10.0) / 10.0).into()
}
fn numbers(values: impl IntoIterator<Item = f64>) -> Value {
    Value::array(values.into_iter().map(|v| control::round_even(v).into()))
}
fn strings(values: impl IntoIterator<Item = String>) -> Value {
    Value::array(values.into_iter().map(Value::from))
}
fn notify(message: &str) {
    if let Err(e) = os::notify(message) {
        error(format!("systemd notification failed: {e}"));
    }
}

/// Complete, deadline-bounded static recovery. Caller already owns the owner lock
/// and has quiesced the only power worker. No other writer is started on timeout.
fn recover(paths: &Paths, c: &Config, owner: Option<Arc<Lock>>) -> bool {
    recover_with_fallback(paths, c, false, owner)
}
fn recover_with_fallback(
    paths: &Paths,
    c: &Config,
    fallback_invalid: bool,
    owner: Option<Arc<Lock>>,
) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    let (p, config) = (paths.clone(), c.clone());
    let Some((mut found, mut missing, devices)) = call_deadline(end, move || {
        let (found, missing) = hardware::cap_paths(&p, &config);
        (found, missing, hardware::devices(&p))
    }) else {
        error("static-safe recovery: inventory read stuck");
        return false;
    };
    let defaults;
    let c = if fallback_invalid
        && c.check_feasible(found.len() + missing.len(), &devices)
            .is_err()
    {
        error("stop hook: configuration floors are infeasible; recovering with built-in defaults");
        defaults = Config::default();
        let (p, config) = (paths.clone(), defaults.clone());
        let Some((f, m)) = call_deadline(end, move || hardware::cap_paths(&p, &config)) else {
            error("static-safe default recovery: inventory read stuck");
            return false;
        };
        found = f;
        missing = m;
        &defaults
    } else {
        c
    };
    let base = c.fixed_base(&devices);
    let p = paths.clone();
    let Some((enforcing, pl2_present)) = call_deadline(end, move || {
        let (e, s) = hardware::rapl_state(&p);
        (e, s && p.resolve(PL2).exists())
    }) else {
        error("static-safe recovery: RAPL state read stuck");
        return false;
    };
    let mut cpu_min = if enforcing && pl2_present { 0.0 } else { 413.0 };
    let mut ss = budget::static_safe(c, base, found.len(), missing.len(), cpu_min);
    let mut io = DeadlineIo::new(
        OwnedIo::new(
            FsPowerIo {
                paths: paths.clone(),
            },
            owner,
        ),
        end,
    );
    let mut result = transaction::transact(
        &budget::static_want(found.values().cloned(), &ss, pl2_present),
        &mut io,
        ss.fits,
    );
    if !result.ok && result.failed.as_deref() == Some(PL2) && io.unresolved.is_empty() {
        let observed = result.seen.get(PL2).copied().flatten();
        cpu_min = cpu_min.max(observed.map(|v| 413.0f64.min(v / 1e6)).unwrap_or(413.0));
        ss = budget::static_safe(c, base, found.len(), missing.len(), cpu_min);
        let next = transaction::transact(
            &budget::static_want(found.values().cloned(), &ss, false),
            &mut io,
            ss.fits,
        );
        result.seen.extend(next.seen);
        result.ok = next.ok;
        result.failed = next.failed;
    }
    crate::log(&[(
        "static_safe",
        Value::object([
            ("gpu".into(), ss.gpu.into()),
            ("pl1".into(), ss.pl1.into()),
            ("cpu_charged_w".into(), ss.pl1.max(cpu_min).into()),
            ("fits".into(), ss.fits.into()),
            ("ok".into(), result.ok.into()),
            (
                "failed".into(),
                result
                    .failed
                    .clone()
                    .map(Value::from)
                    .unwrap_or(Value::None),
            ),
            ("unmanaged_dies".into(), strings(missing)),
            ("unresolved".into(), strings(io.unresolved)),
            (
                "read_back_w".into(),
                Value::object(
                    result
                        .seen
                        .iter()
                        .map(|(p, v)| (p.clone(), r1(v.map(|v| v / 1e6)))),
                ),
            ),
        ]),
    )]);
    result.ok && ss.fits
}
fn handback(services: &mut impl Services) -> io::Result<()> {
    if services.exists("t2fanrd.service")?
        && !services.restarting()?
        && !services.start("t2fanrd.service")?
    {
        return Err(io::Error::other("t2fanrd start was refused"));
    }
    Ok(())
}
pub fn stop_hook(
    paths: &Paths,
    c: &Config,
    services: &mut impl Services,
    service_success: bool,
) -> i32 {
    let lock = match Lock::open(&paths.owner()) {
        Ok(l) => l,
        Err(e) => {
            error(format!(
                "stop hook: cannot establish ownership: {e}; hardware left alone"
            ));
            return 1;
        }
    };
    match lock.acquire(Duration::from_secs(3)) {
        Ok(true) => {}
        Ok(false) => {
            crate::log(&[(
                "note",
                "stop hook: another macpro-fand owns the fans and power limits: left alone".into(),
            )]);
            return 0;
        }
        Err(e) => {
            error(format!(
                "stop hook: owner lock failed: {e}; hardware left alone"
            ));
            return 1;
        }
    }
    let owner = Arc::new(lock);
    let fans = hardware::find_smc(paths).map(|smc| Fans { smc });
    let cooling = fans.as_ref().map(Fans::full).unwrap_or(true);
    let ok = recover_with_fallback(paths, c, true, Some(owner.clone()));
    if !ok || !cooling {
        error("stop hook: recovery incomplete; fans left at their caps");
        return 1;
    }
    if fans.is_some() && service_success {
        if let Err(e) = handback(services) {
            error(format!("stop hook: handover failed: {e}"));
            return 1;
        }
    }
    0
}

/// Inputs to one policy iteration. Unit tests can exercise the complete policy
/// without opening sysfs or calling systemctl.
pub struct Frame {
    pub now: f64,
    pub dt: f64,
    pub instant: Instant,
    pub gpu: BTreeMap<String, GpuSample>,
    pub cpu_temp: Option<f64>,
    pub ambient: BTreeMap<String, Option<f64>>,
    pub energy: Option<f64>,
    pub energy_range: Option<f64>,
    pub dc: Option<f64>,
    pub enforcing: bool,
    pub pl2: Option<f64>,
    pub status: Option<Status>,
    pub tach: [Option<f64>; 4],
    pub overwritten: Vec<usize>,
    pub override_: Override,
}
pub struct Engine {
    pub config: Config,
    pub budget: Budget,
    pub dies: Vec<Die>,
    pub missing: Vec<String>,
    pub cpu: Filter,
    pub cpu_crit: f64,
    pub ambient: BTreeMap<String, (crate::config::SensorSpec, Filter)>,
    pub governors: [Governor; 4],
    pub fan_filters: [Filter; 4],
    pub fan_health: [FanHealth; 4],
    pub written: [Option<f64>; 4],
    pub pkg_w: Option<f64>,
    pub cpu_w: Option<f64>,
    pub bay_t: [Option<f64>; 2],
    pub gpu_age: Option<f64>,
    pub mode: FanMode,
    pub alarms: Vec<String>,
    pub notes: Vec<String>,
    pub override_note: String,
    energy_prev: Option<(f64, f64)>,
    last_budget: f64,
    dc_missing_since: Option<f64>,
    was_feasible: bool,
    owner_fails: usize,
}
impl Engine {
    pub fn new(
        config: Config,
        budget: Budget,
        dies: Vec<Die>,
        missing: Vec<String>,
        sensors: &Sensors,
    ) -> Self {
        Self {
            governors: std::array::from_fn(|i| Governor::new(i + 1, &config)),
            fan_filters: std::array::from_fn(|_| Filter::new(0.0)),
            fan_health: Default::default(),
            written: [None; 4],
            ambient: sensors
                .ambient
                .iter()
                .map(|(k, (spec, s))| (k.clone(), (spec.clone(), s.filter.clone())))
                .collect(),
            cpu: sensors.cpu.filter.clone(),
            cpu_crit: sensors.cpu_crit,
            config,
            budget,
            dies,
            missing,
            pkg_w: None,
            cpu_w: None,
            bay_t: [None; 2],
            gpu_age: None,
            mode: FanMode::Auto,
            alarms: vec![],
            notes: vec![],
            override_note: String::new(),
            energy_prev: None,
            last_budget: 0.0,
            dc_missing_since: None,
            was_feasible: true,
            owner_fails: 0,
        }
    }
    pub fn step(&mut self, f: Frame) -> [f64; 4] {
        self.alarms.clear();
        self.notes.clear();
        let mut errs: [Vec<(f64, f64)>; 4] = Default::default();
        self.mode = f.override_.mode;
        self.override_note = f.override_.note;
        self.alarms
            .extend(self.missing.iter().map(|a| format!("die {a} missing")));
        let mut tj: [Vec<f64>; 2] = Default::default();
        let mut tm: [Vec<f64>; 2] = Default::default();
        let mut ages = Vec::new();
        let mut gpu_ok = !self.dies.is_empty();
        for d in &mut self.dies {
            let ent = f.gpu.get(&d.spec.addr);
            let age = ent.map(|e| f.instant.saturating_duration_since(e.taken).as_secs_f64());
            let Some(ent) = ent.filter(|_| age.unwrap() <= GPU_STALE) else {
                self.alarms.push(format!(
                    "gpu {} {}",
                    if ent.is_some() { "stale" } else { "no data" },
                    d.spec.addr
                ));
                gpu_ok = false;
                continue;
            };
            ages.push(age.unwrap());
            if d.last_seq != Some(ent.seq) {
                let gap = d
                    .last_taken
                    .map(|t| {
                        ent.taken
                            .saturating_duration_since(t)
                            .as_secs_f64()
                            .min(GPU_STALE)
                    })
                    .unwrap_or(PERIOD);
                d.last_seq = Some(ent.seq);
                d.last_taken = Some(ent.taken);
                for (filter, v) in d.filters.iter_mut().zip(ent.values) {
                    filter.feed(v, gap);
                }
            }
            if d.filters.iter().any(|s| !s.healthy()) {
                self.alarms.push(format!("read {}", d.spec.addr));
            }
            if d.filters[0].raw.is_some_and(|t| t >= d.spec.jcrit - 5.0)
                || d.filters[1].raw.is_some_and(|t| t >= d.spec.mcrit - 5.0)
            {
                self.alarms.push(format!("hot {}", d.spec.addr));
            }
            if let Some(t) = d.filters[0].val {
                tj[d.spec.bay - 1].push(t);
            }
            if let Some(t) = d.filters[1].val {
                tm[d.spec.bay - 1].push(t);
            }
        }
        self.gpu_age = ages.into_iter().reduce(f64::max);
        self.bay_t = std::array::from_fn(|i| {
            if tj[i].is_empty() {
                None
            } else {
                Some(tj[i].iter().sum::<f64>() / tj[i].len() as f64)
            }
        });
        if gpu_ok {
            let telemetry: Telemetry = self
                .dies
                .iter()
                .map(|d| {
                    (
                        d.spec.addr.clone(),
                        (
                            d.filters[0].val,
                            d.filters[1].val,
                            d.filters[2].val,
                            d.spec.bay,
                        ),
                    )
                })
                .collect();
            let full = [
                (1, self.governors[1].rpm >= CAPS[1] - 25.0),
                (2, self.governors[2].rpm >= CAPS[2] - 25.0),
            ]
            .into_iter()
            .collect();
            self.budget.thermal(&telemetry, &full, f.dt);
        }
        for b in 0..2 {
            if let Some(t) = self.bay_t[b] {
                errs[b + 1].push((t - self.config.bay_target, self.config.band));
            }
            if let Some(t) = tm[b].iter().copied().reduce(f64::max) {
                errs[b + 1].push((t - self.config.hbm_target, self.config.band));
            }
        }
        self.cpu_w = control::energy_rate(self.energy_prev, f.now, f.energy, f.energy_range);
        if let Some(w) = self.cpu_w {
            self.pkg_w = Some(control::ema(
                self.pkg_w,
                w,
                f.dt,
                self.config.cpu_target_tau,
            ));
        }
        self.energy_prev = f.energy.map(|e| (f.now, e));
        if !self.cpu.feed(f.cpu_temp, f.dt) {
            self.alarms.push("read cpu".into());
        }
        if let Some(t) = self.cpu.raw {
            if t >= self.cpu_crit - 3.0 {
                self.alarms.push("hot cpu".into());
            }
            errs[3].push((
                self.cpu.val.unwrap() - control::cpu_target(self.pkg_w, &self.config),
                self.config.cpu_band,
            ));
        }
        for (k, (spec, s)) in &mut self.ambient {
            if !s.feed(f.ambient.get(k).copied().flatten(), f.dt) {
                self.alarms.push(format!("read {k}"));
            }
            if let Some(t) = s.raw {
                if t >= spec.emergency {
                    self.alarms.push(format!("hot {k}"));
                }
                errs[spec.fan - 1].push((s.val.unwrap() - spec.target, spec.band));
            }
        }
        self.dc_missing_since = if f.dc.is_some() {
            None
        } else {
            Some(self.dc_missing_since.unwrap_or(f.now))
        };
        let degraded = self.dc_missing_since.is_some_and(|t| f.now - t > 15.0);
        if degraded != self.budget.degraded {
            self.budget.degraded = degraded;
            self.notes.push(if degraded{"no measured DC: demand-driven reallocation paused; pins and CPU charge still apply"}else{"measured DC back: reallocation resumes"}.into());
        }
        if self.budget.backstop_check(f.dc, f.now) {
            self.alarms.push(format!(
                "power backstop: DC {:.0} W > {:.0} W, every limit to its floor",
                f.dc.unwrap(),
                self.budget.backstop
            ));
        }
        if self.budget.pin.is_some() && f.override_.pin.is_none() {
            self.budget.end_pin(f.now);
            self.notes.push(
                "override pin ended: default allocation restored after any backstop hold".into(),
            );
        }
        self.budget.pin = f.override_.pin;
        if let Some(st) = &f.status {
            self.notes.extend(st.notes.clone());
            if let Some(p) = &st.pending {
                if f.instant.saturating_duration_since(p.since) > Duration::from_secs(15) {
                    self.alarms
                        .push(format!("power limit {} stuck: {}", p.kind, p.path));
                }
            }
            if st
                .fail_since
                .is_some_and(|t| f.instant.saturating_duration_since(t) > Duration::from_secs(30))
            {
                self.alarms
                    .push(format!("power limits not confirmed: {:?}", st.failed));
            }
            if st.panicked {
                self.alarms.push("power worker panicked".into());
            }
            if self.budget.pl2_held && st.pl2_bad >= 3 {
                self.budget.pl2_held = false;
                self.notes
                    .push("PL2 cannot be held at PL1: CPU charged at PL2 as read back".into());
            }
        }
        // Recheck the actual CPU charge every iteration, including pins and a
        // loss of RAPL enforcement between normal reallocation intervals.
        if self.budget.cpu_charge(f.pl2, f.enforcing) {
            self.notes.push(format!(
                "CPU charged at {:.0} W (PL1 {:.0} W)",
                self.budget.cpu_w(),
                self.budget.pl1
            ));
        }
        if f.now - self.last_budget >= 4.0 {
            self.last_budget = f.now;
            if gpu_ok {
                let watts = self
                    .dies
                    .iter()
                    .map(|d| (d.spec.addr.clone(), d.filters[2].raw))
                    .collect();
                if let Some(msg) = self.budget.rebalance(f.now, &watts, self.cpu_w) {
                    self.notes.push(msg);
                }
            }
        }
        let feasible = self.budget.feasible();
        if !feasible {
            self.alarms.push(format!(
                "budget infeasible: {:.0} W > {:.0} W even at floors (CPU charged {:.0} W)",
                self.budget.cost(),
                self.budget.limit,
                self.budget.cpu_w()
            ));
        }
        if feasible != self.was_feasible {
            self.notes.push(
                if feasible {
                    "budget feasible again"
                } else {
                    "budget infeasible: only lowering limits, every fan at its cap"
                }
                .into(),
            );
            self.was_feasible = feasible;
        }
        for i in 0..4 {
            if !self.fan_filters[i].feed(f.tach[i], f.dt) {
                self.alarms.push(format!("read fan{}", i + 1));
            }
            if let Some(written) = self.written[i] {
                if self.fan_health[i].sample(written, self.fan_filters[i].raw, f.now) {
                    self.alarms.push(format!(
                        "fan{} at {} rpm, set {written:.0}",
                        i + 1,
                        self.fan_filters[i]
                            .raw
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "None".into())
                    ));
                }
            }
        }
        self.owner_fails = if f.overwritten.is_empty() {
            0
        } else {
            self.owner_fails.saturating_add(1)
        };
        if self.owner_fails >= 5 {
            self.alarms.push(format!(
                "fans {:?} changed by another writer",
                f.overwritten
            ));
        }
        std::array::from_fn(|i| {
            let g = &mut self.governors[i];
            g.step(&errs[i], f.dt);
            if !self.alarms.is_empty() || self.mode == FanMode::Max {
                g.rpm = CAPS[i];
            } else if let FanMode::Hold(rpm) = self.mode {
                g.rpm = rpm[i];
            }
            control::round_even(g.rpm)
        })
    }
    pub fn status_log(&self, st: Option<&Status>) {
        crate::log(&[
            ("alarms", strings(self.alarms.clone())),
            (
                "mode",
                match self.mode {
                    FanMode::Auto => "auto",
                    FanMode::Max => "max",
                    FanMode::Hold(_) => "hold",
                }
                .into(),
            ),
            (
                "note",
                if self.override_note.is_empty() {
                    Value::None
                } else {
                    self.override_note.clone().into()
                },
            ),
            (
                "dies",
                Value::object(self.dies.iter().map(|d| {
                    (
                        d.spec.addr.clone(),
                        Value::array(d.filters.iter().map(|s| r1(s.val))),
                    )
                })),
            ),
            ("gpu_age", r1(self.gpu_age)),
            (
                "cpu",
                Value::array([
                    r1(self.cpu.val),
                    r1(Some(control::cpu_target(self.pkg_w, &self.config))),
                    r1(self.pkg_w),
                ]),
            ),
            (
                "bays",
                Value::object(
                    self.bay_t
                        .iter()
                        .enumerate()
                        .map(|(i, t)| ((i + 1).to_string(), r1(*t))),
                ),
            ),
            (
                "smc",
                Value::object(
                    self.ambient
                        .iter()
                        .map(|(k, (_, s))| (k.clone(), r1(s.val))),
                ),
            ),
            (
                "zones",
                Value::object(self.governors.iter().enumerate().map(|(i, g)| {
                    (
                        (i + 1).to_string(),
                        Value::array([
                            r1(g.err),
                            (control::round_even(g.rate * 100.0) / 100.0).into(),
                            (g.rpm as i64).into(),
                        ]),
                    )
                })),
            ),
            (
                "rpm",
                Value::object(
                    self.written
                        .iter()
                        .enumerate()
                        .map(|(i, w)| ((i + 1).to_string(), (*w).into())),
                ),
            ),
            (
                "tach",
                Value::object(
                    self.fan_filters
                        .iter()
                        .enumerate()
                        .map(|(i, s)| ((i + 1).to_string(), s.raw.into())),
                ),
            ),
            (
                "budget",
                Value::object([
                    ("gpu".into(), numbers(self.budget.gpu.values().copied())),
                    ("pl1".into(), control::round_even(self.budget.pl1).into()),
                    (
                        "cost".into(),
                        control::round_even(self.budget.cost()).into(),
                    ),
                    ("dc".into(), r1(self.budget.dc)),
                    ("tcap".into(), numbers(self.budget.tcap.values().copied())),
                    (
                        "cpu_w".into(),
                        control::round_even(self.budget.cpu_w()).into(),
                    ),
                    ("degraded".into(), self.budget.degraded.into()),
                    ("feasible".into(), self.budget.feasible().into()),
                    (
                        "confirmed".into(),
                        st.map(|s| Value::from(s.confirmed()))
                            .unwrap_or(Value::None),
                    ),
                ]),
            ),
        ]);
    }
}

struct Session {
    paths: Paths,
    config: Config,
    fans: Fans,
    worker: Option<PowerWorker>,
    owner: Option<Arc<Lock>>,
    dry: bool,
    under_systemd: bool,
    finished: bool,
}
impl Session {
    fn cleanup(
        &mut self,
        clean: bool,
        services: &mut impl Services,
        stop_timeout: Duration,
    ) -> io::Result<()> {
        self.finished = true;
        if self.dry {
            return Ok(());
        }
        // Cooling precedes every wait on the power worker.
        let cooling = self.fans.full();
        let idle = self
            .worker
            .as_ref()
            .map(|w| w.stop(stop_timeout))
            .unwrap_or(true);
        if !idle {
            return Err(io::Error::other("power recovery incomplete: driver operation stuck; fans at caps; no competing transaction"));
        }
        if !self.under_systemd {
            if !recover(&self.paths, &self.config, self.owner.clone()) {
                return Err(io::Error::other(
                    "static-safe limits not confirmed; fans at caps",
                ));
            }
            if clean && cooling {
                handback(services)?;
            }
        }
        if !cooling {
            return Err(io::Error::other(
                "one or more fans could not be set to full speed",
            ));
        }
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.finished && !self.dry {
            self.fans.full();
            let idle = self
                .worker
                .as_ref()
                .map(|w| w.stop(Duration::from_secs(2)))
                .unwrap_or(true);
            if idle && !self.under_systemd {
                let _ = recover(&self.paths, &self.config, self.owner.clone());
            }
        }
    }
}

/// Discovery keeps at most one unfinished probe per die. A driver read stuck in
/// an earlier attempt never causes an unbounded collection of probe threads.
fn discover_dies(paths: &Paths, c: &Config) -> (Vec<DieSpec>, Vec<String>) {
    let map = hardware::die_map(paths, c);
    let end = Instant::now() + Duration::from_secs(240);
    let mut found = BTreeMap::new();
    let mut running: BTreeMap<String, mpsc::Receiver<Option<DieSpec>>> = BTreeMap::new();
    loop {
        for (a, b) in &map {
            if !found.contains_key(a) && !running.contains_key(a) {
                let (tx, rx) = mpsc::sync_channel(1);
                let (p, addr, bay) = (paths.clone(), a.clone(), *b);
                let task = thread::Builder::new()
                    .name("die-discovery".into())
                    .spawn(move || {
                        let _ = tx.send(DieSpec::discover(&p, &addr, bay));
                    });
                if task.is_ok() {
                    running.insert(a.clone(), rx);
                }
            }
        }
        let wait_end = (Instant::now() + Duration::from_secs(10)).min(end);
        while Instant::now() < wait_end && !os::stopped() {
            let ready: Vec<_> = running
                .iter()
                .filter_map(|(a, r)| match r.try_recv() {
                    Ok(d) => Some((a.clone(), d)),
                    Err(mpsc::TryRecvError::Disconnected) => Some((a.clone(), None)),
                    Err(mpsc::TryRecvError::Empty) => None,
                })
                .collect();
            for (a, d) in ready {
                running.remove(&a);
                if let Some(d) = d {
                    found.insert(a, d);
                }
            }
            if running.is_empty() {
                break;
            }
            os::sleep_interruptible(Duration::from_millis(20));
        }
        let missing: Vec<_> = map
            .keys()
            .filter(|a| !found.contains_key(*a))
            .cloned()
            .collect();
        if missing.is_empty() || Instant::now() >= end || os::stopped() {
            return (found.into_values().collect(), missing);
        }
        notify(&format!("STATUS=waiting for amdgpu: {}", missing.join(" ")));
        os::sleep_interruptible(Duration::from_secs(2));
    }
}
fn hook_number(name: &str) -> Result<Option<f64>, String> {
    match std::env::var(name) {
        Ok(s) if s.is_empty() => Ok(None),
        Ok(s) => {
            let v = s
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| format!("{name} must be finite and nonnegative"))?;
            Ok(if v == 0.0 { None } else { Some(v) })
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(format!("{name} is not UTF-8")),
    }
}

pub fn entry() -> i32 {
    let opt = match Options::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("macpro-fand: {e}");
            return 2;
        }
    };
    if opt.mode == Mode::Version {
        println!("macpro-fand {}", crate::VERSION);
        return 0;
    }
    if opt.mode == Mode::Help {
        print!("{HELP}");
        return 0;
    }
    let mut config = match Config::load(&opt.config) {
        Ok(c) => c,
        Err(e) => {
            error(format!("configuration: {e}"));
            if opt.mode != Mode::Stopped {
                return NOHW;
            }
            Config::default()
        }
    };
    let paths = Paths::default();
    if opt.mode == Mode::CheckConfig {
        return match config.check_feasible(
            hardware::die_map(&paths, &config).len(),
            &hardware::devices(&paths),
        ) {
            Ok(()) => {
                println!("configuration OK");
                0
            }
            Err(e) => {
                error(format!("configuration: {e}"));
                println!("configuration error; nothing applied");
                NOHW
            }
        };
    }
    if !matches!(opt.mode, Mode::DryRun) && os::uid() != 0 {
        error("hardware control requires root");
        return 1;
    }
    let mut services = Systemd;
    if opt.mode == Mode::Stopped {
        return stop_hook(
            &paths,
            &config,
            &mut services,
            std::env::var("SERVICE_RESULT").as_deref() == Ok("success"),
        );
    }
    if !hardware::text(paths.sys("class/dmi/id/product_name"))
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("macpro7,1")
    {
        error("not a MacPro7,1: exiting");
        return NOHW;
    }
    if let Err(e) = config.check_feasible(
        hardware::die_map(&paths, &config).len(),
        &hardware::devices(&paths),
    ) {
        error(e);
        return NOHW;
    }
    if let Err(e) = os::install_signals() {
        error(format!("cannot install signal handlers: {e}"));
        return 1;
    }
    let end = Instant::now() + Duration::from_secs(if opt.mode == Mode::Release { 0 } else { 240 });
    let mut smc = hardware::find_smc(&paths);
    while smc.is_none() && Instant::now() < end && !os::stopped() {
        os::sleep_interruptible(Duration::from_secs(2));
        smc = hardware::find_smc(&paths);
    }
    let Some(smc) = smc else {
        error("applesmc fan attributes not found (applesmc-t2 driver loaded?)");
        return NOHW;
    };
    let dry = opt.mode == Mode::DryRun;
    let _owner = if dry {
        None
    } else {
        let lock = match Lock::open(&paths.owner()) {
            Ok(l) => l,
            Err(e) => {
                error(format!("owner lock: {e}"));
                return 1;
            }
        };
        match lock.acquire(Duration::ZERO) {
            Ok(true) => Some(Arc::new(lock)),
            Ok(false) => {
                error("another macpro-fand owns the fans and power limits (stop it first)");
                return 1;
            }
            Err(e) => {
                error(format!("owner lock: {e}"));
                return 1;
            }
        }
    };
    let fans = Fans { smc: smc.clone() };
    if opt.mode == Mode::Release {
        if !fans.full() || !recover(&paths, &config, _owner.clone()) {
            error("--release: recovery incomplete; fans at caps");
            return 1;
        }
        return match handback(&mut services) {
            Ok(()) => 0,
            Err(e) => {
                error(e.to_string());
                1
            }
        };
    }
    let hooks = (|| {
        Ok::<_, String>((
            hook_number("MACPRO_FAND_TEST_BUDGET")?,
            hook_number("MACPRO_FAND_TEST_BACKSTOP")?,
            hook_number("MACPRO_FAND_TEST_TWORK")?,
            hook_number("MACPRO_FAND_TEST_STALL_GPU")?,
            hook_number("MACPRO_FAND_TEST_DEAD_FAN")?,
        ))
    })();
    let (limit, backstop, twork, stall, dead_fan) = match hooks {
        Ok(v) => v,
        Err(e) => {
            error(e);
            return NOHW;
        }
    };
    // These hooks only reduce the envelope, or inject bounded faults. A bad
    // environment cannot silently enlarge the approved power budget.
    if limit.is_some_and(|v| v > config.budget_limit - config.budget_margin)
        || backstop.is_some_and(|v| v > config.budget_limit)
        || twork.is_some_and(|v| !(40.0..=95.0).contains(&v))
        || stall.is_some_and(|v| v > 86400.0)
        || dead_fan.is_some_and(|v| !(1.0..=4.0).contains(&v) || v.fract() != 0.0)
    {
        error("test hook outside its safe range");
        return NOHW;
    }
    if let Some(t) = twork {
        config.t_work.0 = t;
    }
    let (specs, missing) = discover_dies(&paths, &config);
    if os::stopped() {
        return 0;
    }
    if !missing.is_empty() {
        error(format!(
            "dies missing: {missing:?}; charged 200 W each, every fan at its cap"
        ));
    }
    let odd: Vec<_> = hardware::die_map(&paths, &config)
        .into_keys()
        .filter(|a| !hardware::gpu_model_ok(&paths, a))
        .collect();
    if !odd.is_empty() {
        error(format!("GPU dies that are not Vega 20: {odd:?}; unsupported: budget bounds and calibration are Vega 20's"));
    }
    if !dry
        && hardware::hwmon_named(&paths, "drivetemp").is_empty()
        && hardware::devices(&paths).get("disk").copied().unwrap_or(0) > 0
    {
        let _ = os::command(
            "/usr/sbin/modprobe",
            &["drivetemp"],
            Duration::from_secs(10),
        );
    }
    let sensors = match Sensors::discover(&paths, &smc, &config) {
        Ok(s) => s,
        Err(e) => {
            error(e);
            return 1;
        }
    };
    if !dry {
        let end = Instant::now() + Duration::from_secs(if os::under_systemd() { 15 } else { 0 });
        loop {
            match services.active("t2fanrd.service") {
                Ok(false) => break,
                Ok(true) if Instant::now() < end => {
                    os::sleep_interruptible(Duration::from_millis(500))
                }
                Ok(true) => {
                    error("t2fanrd is running: stop it first");
                    return 1;
                }
                Err(e) => {
                    error(format!("cannot confirm exclusive fan ownership: {e}"));
                    return 1;
                }
            }
            if os::stopped() {
                return 0;
            }
        }
    }
    let inventory = hardware::devices(&paths);
    let base = config.fixed_base(&inventory);
    let caps = specs
        .iter()
        .map(|d| (d.addr.clone(), d.cap.to_string_lossy().into_owned()))
        .collect();
    let mut budget = Budget::new(config.clone(), caps, missing.len(), base);
    if let Some(l) = limit {
        budget.limit = l;
        budget.fit(None);
    }
    if let Some(v) = backstop {
        budget.backstop = v;
    }
    let (enforcing, pl2_present) = hardware::rapl_state(&paths);
    budget.pl2_held = pl2_present && paths.resolve(PL2).exists();
    budget.cpu_charge(hardware::number(paths.resolve(PL2), 1.0), enforcing);
    budget.fit(None);
    let mut session = Session {
        paths: paths.clone(),
        config: config.clone(),
        fans,
        worker: None,
        owner: _owner.clone(),
        dry,
        under_systemd: os::under_systemd(),
        finished: false,
    };
    let result = (|| -> Result<(), String> {
        if !dry {
            let worker = PowerWorker::start(OwnedIo::new(
                FsPowerIo {
                    paths: paths.clone(),
                },
                session.owner.clone(),
            ))
            .map_err(|e| e.to_string())?;
            worker.set(budget.want(), BTreeSet::new(), budget.feasible());
            session.worker = Some(worker);
        }
        let dc = DcReader::start(paths.clone(), smc.clone()).map_err(|e| e.to_string())?;
        let gpu = GpuReader::start(specs.clone(), stall.map(Duration::from_secs_f64))
            .map_err(|e| e.to_string())?;
        let mut engine = Engine::new(
            config.clone(),
            budget,
            specs.into_iter().map(Die::new).collect(),
            missing,
            &sensors,
        );
        if !engine.missing.is_empty() || !engine.budget.feasible() {
            for (g, cap) in engine.governors.iter_mut().zip(CAPS) {
                g.rpm = cap;
            }
        }
        crate::log(&[
            ("version", crate::VERSION.into()),
            ("dry_run", dry.into()),
            (
                "zones",
                Value::object(config.zones.iter().enumerate().map(|(i, z)| {
                    (
                        (i + 1).to_string(),
                        Value::object([
                            ("name".into(), z.name.clone().into()),
                            ("floor".into(), z.floor.into()),
                            ("init".into(), z.init.into()),
                            ("cap".into(), CAPS[i].into()),
                        ]),
                    )
                })),
            ),
            (
                "budget",
                Value::object([
                    ("limit".into(), engine.budget.limit.into()),
                    ("backstop".into(), engine.budget.backstop.into()),
                    ("gpu".into(), numbers(engine.budget.gpu.values().copied())),
                    ("pl1".into(), control::round_even(engine.budget.pl1).into()),
                    (
                        "cost".into(),
                        control::round_even(engine.budget.cost()).into(),
                    ),
                    ("base".into(), base.into()),
                    (
                        "devices".into(),
                        Value::object(inventory.iter().map(|(k, v)| (k.clone(), (*v).into()))),
                    ),
                    ("unmanaged_dies".into(), strings(engine.missing.clone())),
                    ("rapl_enforcing".into(), enforcing.into()),
                    ("pl2_held".into(), engine.budget.pl2_held.into()),
                    ("fits".into(), engine.budget.feasible().into()),
                ]),
            ),
        ]);
        if !dry {
            let end = Instant::now() + Duration::from_secs(10);
            let mut held = false;
            while Instant::now() < end && !os::stopped() {
                for i in 0..4 {
                    session
                        .fans
                        .put(i + 1, engine.governors[i].rpm)
                        .map_err(|e| e.to_string())?;
                }
                os::sleep_interruptible(Duration::from_secs(1));
                if (0..4).all(|i| session.fans.holds(i + 1, engine.governors[i].rpm)) {
                    for i in 0..4 {
                        engine.written[i] = Some(engine.governors[i].rpm as i64 as f64);
                    }
                    held = true;
                    break;
                }
            }
            if !held && !os::stopped() {
                return Err("fans do not hold manual settings after handover".into());
            }
        }
        if os::stopped() {
            return Ok(());
        }
        notify("READY=1");
        let origin = Instant::now();
        let mut last = origin;
        let mut last_log = -30.0;
        let mut last_note = String::new();
        let range = hardware::number(paths.resolve(RAPL).join("max_energy_range_uj"), 1.0);
        while !os::stopped() {
            os::sleep_interruptible(Duration::from_secs_f64(PERIOD));
            if os::stopped() {
                break;
            }
            let instant = Instant::now();
            let now = origin.elapsed().as_secs_f64();
            let dt = instant.duration_since(last).as_secs_f64().min(5.0 * PERIOD);
            last = instant;
            let st = session.worker.as_ref().map(PowerWorker::status);
            let overwritten = if dry {
                vec![]
            } else {
                engine
                    .written
                    .iter()
                    .enumerate()
                    .filter_map(|(i, w)| {
                        w.filter(|w| !session.fans.holds(i + 1, *w)).map(|_| i + 1)
                    })
                    .collect()
            };
            for &f in &overwritten {
                if let Err(e) = session.fans.put(f, engine.written[f - 1].unwrap()) {
                    return Err(format!("reassert fan{f}: {e}"));
                }
            }
            let frame = Frame {
                now,
                dt,
                instant,
                gpu: gpu.latest(),
                cpu_temp: hardware::number(&sensors.cpu.path, sensors.cpu.scale),
                ambient: sensors
                    .ambient
                    .iter()
                    .map(|(k, (_, s))| (k.clone(), hardware::number(&s.path, s.scale)))
                    .collect(),
                energy: hardware::number(paths.resolve(RAPL).join("energy_uj"), 1.0),
                energy_range: range,
                dc: dc.latest(instant).and_then(|(v, _)| v),
                enforcing: hardware::rapl_state(&paths).0,
                pl2: if let Some(st) = &st {
                    st.pl2_observed(instant)
                } else {
                    hardware::number(paths.resolve(PL2), 1.0)
                },
                status: st.clone(),
                tach: std::array::from_fn(|i| {
                    if dead_fan == Some((i + 1) as f64) {
                        Some(0.0)
                    } else {
                        hardware::number(session.fans.path(i + 1, "input"), 1.0)
                    }
                }),
                overwritten,
                override_: Override::load(&paths.override_file(), &config),
            };
            let cmd = engine.step(frame);
            for n in &engine.notes {
                note(n.clone());
            }
            if let Some(w) = &session.worker {
                w.set(
                    engine.budget.want(),
                    BTreeSet::new(),
                    engine.budget.feasible(),
                );
            }
            for (i, rpm) in cmd.iter().copied().enumerate() {
                let should = engine.written[i]
                    .map(|v| {
                        (v - rpm).abs() >= 25.0 || (engine.governors[i].rate == 0.0 && v != rpm)
                    })
                    .unwrap_or(true);
                if should {
                    if !dry {
                        session
                            .fans
                            .put(i + 1, rpm)
                            .map_err(|e| format!("write fan{}: {e}", i + 1))?;
                    }
                    engine.written[i] = Some(rpm);
                }
            }
            notify("WATCHDOG=1");
            if !engine.alarms.is_empty()
                || engine.override_note != last_note
                || now - last_log >= 30.0
            {
                last_log = now;
                last_note = engine.override_note.clone();
                engine.status_log(st.as_ref());
            }
        }
        Ok(())
    })();
    let clean = result.is_ok();
    if let Err(e) = result {
        error(e);
    }
    if !dry {
        notify("STOPPING=1");
    }
    let stop_timeout = Duration::from_secs(if session.under_systemd { 2 } else { 10 });
    match session.cleanup(clean, &mut services, stop_timeout) {
        Ok(()) if clean => 0,
        Ok(()) => 1,
        Err(e) => {
            error(e.to_string());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PL1;
    use std::path::Path;
    struct FakeServices {
        calls: Vec<String>,
    }
    impl Services for FakeServices {
        fn active(&mut self, _: &str) -> io::Result<bool> {
            panic!("unexpected service call")
        }
        fn exists(&mut self, u: &str) -> io::Result<bool> {
            self.calls.push(format!("exists {u}"));
            Ok(true)
        }
        fn restarting(&mut self) -> io::Result<bool> {
            self.calls.push("restarting".into());
            Ok(false)
        }
        fn start(&mut self, u: &str) -> io::Result<bool> {
            self.calls.push(format!("start {u}"));
            Ok(true)
        }
    }
    fn engine() -> Engine {
        let c = Config::default();
        let specs = (0..4)
            .map(|i| DieSpec {
                addr: format!("g{i}"),
                bay: if i < 2 { 1 } else { 2 },
                paths: Default::default(),
                cap: format!("/gpu/{i}").into(),
                jcrit: 105.0,
                mcrit: 95.0,
            })
            .collect::<Vec<_>>();
        let paths = specs
            .iter()
            .map(|d| (d.addr.clone(), d.cap.to_string_lossy().into_owned()))
            .collect();
        let b = Budget::new(c.clone(), paths, 0, 127.0);
        let sensors = Sensors {
            cpu: hardware::Sensor::new(PathBuf::new(), 1000.0, 4.0),
            cpu_crit: 92.0,
            ambient: BTreeMap::new(),
        };
        Engine::new(
            c,
            b,
            specs.into_iter().map(Die::new).collect(),
            vec![],
            &sensors,
        )
    }
    fn frame(now: f64, seq: u64) -> Frame {
        let instant = Instant::now();
        Frame {
            now,
            dt: 2.0,
            instant,
            gpu: (0..4)
                .map(|i| {
                    (
                        format!("g{i}"),
                        GpuSample {
                            values: [Some(38.0), Some(40.0), Some(50.0)],
                            taken: instant,
                            seq,
                        },
                    )
                })
                .collect(),
            cpu_temp: Some(40.0),
            ambient: BTreeMap::new(),
            energy: Some(now * 100e6),
            energy_range: Some(1e12),
            dc: Some(600.0),
            enforcing: true,
            pl2: Some(300e6),
            status: None,
            tach: [Some(600.0), Some(1150.0), Some(1150.0), Some(500.0)],
            overwritten: vec![],
            override_: Override::default(),
        }
    }
    #[test]
    fn persistent_gpu_read_alarm_and_stale_full_speed() {
        let mut e = engine();
        for i in 0..3 {
            let mut f = frame(i as f64 * 2.0, i + 1);
            for s in f.gpu.values_mut() {
                s.values[2] = None;
            }
            e.step(f);
        }
        assert!(e.alarms.iter().any(|a| a == "read g0"));
        let mut f = frame(6.0, 3);
        for s in f.gpu.values_mut() {
            s.values[2] = None;
        }
        assert_eq!(e.step(f), CAPS);
        e.step(frame(8.0, 4));
        assert!(!e.alarms.iter().any(|a| a == "read g0"));
        let mut f = frame(10.0, 4);
        for s in f.gpu.values_mut() {
            s.taken -= Duration::from_secs(11);
        }
        assert_eq!(e.step(f), CAPS);
        assert!(e.alarms.iter().any(|a| a.starts_with("gpu stale")));
    }
    #[test]
    fn rapl_loss_and_recovery_checked_each_loop() {
        let mut e = engine();
        e.budget.fixed_base = 327.0;
        e.config.gpu_floor = 125.0;
        e.budget.config.gpu_floor = 125.0;
        let mut f = frame(4.0, 1);
        f.enforcing = false;
        assert_eq!(e.step(f), CAPS);
        assert!(!e.budget.feasible());
        let mut f = frame(6.0, 2);
        f.enforcing = false;
        assert_eq!(e.step(f), CAPS);
        assert!(e.alarms.iter().any(|a| a.starts_with("budget infeasible")));
        e.step(frame(8.0, 3));
        assert!(e.budget.feasible());
        assert!(e.notes.iter().any(|n| n == "budget feasible again"));
    }
    #[test]
    fn alarms_out_rank_hold_and_missing_dc_degrades() {
        let mut e = engine();
        let mut f = frame(2.0, 1);
        f.override_ = Override::parse("hold 500 500 500 500", 0.0, &e.config);
        assert_eq!(e.step(f), [500.0; 4]);
        let mut f = frame(4.0, 2);
        f.override_ = Override::parse("hold 500 500 500 500", 0.0, &e.config);
        f.cpu_temp = Some(90.0);
        assert_eq!(e.step(f), CAPS);
        for n in [6.0, 24.0] {
            let mut f = frame(n, n as u64);
            f.dc = None;
            e.step(f);
        }
        assert!(e.budget.degraded);
        e.step(frame(26.0, 30));
        assert!(!e.budget.degraded);
    }
    #[test]
    fn owner_stop_hook_leaves_fixture_and_services_alone() {
        let root = crate::os::tests::temp();
        let paths = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        let lock = Lock::open(&paths.owner()).unwrap();
        assert!(lock.acquire(Duration::ZERO).unwrap());
        let smc = paths.sys("devices/platform/applesmc.768");
        hardware::tests::put(smc.join("fan1_manual"), "0");
        hardware::tests::put(smc.join("fan1_output"), "600");
        let mut s = FakeServices { calls: vec![] };
        assert_eq!(stop_hook(&paths, &Config::default(), &mut s, true), 0);
        assert!(s.calls.is_empty());
        assert_eq!(
            hardware::text(smc.join("fan1_manual")).as_deref(),
            Some("0")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn recovery_and_hook_use_fixture_limits() {
        let root = crate::os::tests::temp();
        let paths = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        let smc = paths.sys("devices/platform/applesmc.768");
        for i in 1..=4 {
            hardware::tests::put(smc.join(format!("fan{i}_manual")), "0");
            hardware::tests::put(smc.join(format!("fan{i}_output")), "500");
        }
        for (suffix, value) in [
            ("enabled", "1"),
            ("constraint_0_name", "long_term"),
            ("constraint_1_name", "short_term"),
            ("constraint_0_power_limit_uw", "10000000"),
            ("constraint_1_power_limit_uw", "10000000"),
        ] {
            hardware::tests::put(paths.resolve(RAPL).join(suffix), value);
        }
        let mut s = FakeServices { calls: vec![] };
        assert_eq!(stop_hook(&paths, &Config::default(), &mut s, true), 0);
        assert_eq!(s.calls.len(), 3);
        for (i, cap) in CAPS.iter().enumerate() {
            assert!(Fans { smc: smc.clone() }.holds(i + 1, *cap));
        }
        assert_eq!(hardware::number(paths.resolve(PL1), 1.0), Some(413e6));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cli_exclusion_and_config_arguments() {
        assert!(Options::parse(["--dry-run", "--release"].map(str::to_string)).is_err());
        assert_eq!(
            Options::parse(["--config=/tmp/x", "--check-config"].map(str::to_string))
                .unwrap()
                .config,
            Path::new("/tmp/x")
        );
    }
    fn fixture_session() -> (PathBuf, Session) {
        let root = crate::os::tests::temp();
        let paths = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        let smc = paths.sys("devices/platform/applesmc.768");
        for i in 1..=4 {
            hardware::tests::put(smc.join(format!("fan{i}_manual")), "0");
            hardware::tests::put(smc.join(format!("fan{i}_output")), "500");
        }
        for (suffix, value) in [
            ("enabled", "1"),
            ("constraint_0_name", "long_term"),
            ("constraint_1_name", "short_term"),
            ("constraint_0_power_limit_uw", "10000000"),
            ("constraint_1_power_limit_uw", "10000000"),
        ] {
            hardware::tests::put(paths.resolve(RAPL).join(suffix), value);
        }
        (
            root,
            Session {
                paths,
                config: Config::default(),
                fans: Fans { smc },
                worker: None,
                owner: None,
                dry: false,
                under_systemd: false,
                finished: false,
            },
        )
    }
    #[test]
    fn failure_and_unwind_leave_full_cooling_without_handover() {
        let (root, mut session) = fixture_session();
        let mut services = FakeServices { calls: vec![] };
        assert!(session
            .cleanup(false, &mut services, Duration::ZERO)
            .is_ok());
        assert!(services.calls.is_empty());
        for (i, cap) in CAPS.iter().enumerate() {
            assert!(session.fans.holds(i + 1, *cap));
        }
        std::fs::remove_dir_all(root).unwrap();
        let (root, session) = fixture_session();
        let smc = session.fans.smc.clone();
        assert!(std::panic::catch_unwind(|| {
            let _session = session;
            panic!("injected main-loop panic");
        })
        .is_err());
        for (i, cap) in CAPS.iter().enumerate() {
            assert!(Fans { smc: smc.clone() }.holds(i + 1, *cap));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    struct StuckWrite {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        writes: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
    }
    impl transaction::PowerIo for StuckWrite {
        fn read(&mut self, _: &str) -> Option<f64> {
            Some(200e6)
        }
        fn write(&mut self, _: &str, v: f64) -> bool {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            self.writes.lock().unwrap().push(v);
            true
        }
    }
    #[test]
    fn cooling_precedes_worker_wait_and_stuck_worker_prevents_recovery() {
        let (root, mut session) = fixture_session();
        let (tx, rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writes = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let worker = PowerWorker::start(StuckWrite {
            entered: tx,
            release: release_rx,
            writes: writes.clone(),
        })
        .unwrap();
        worker.set(
            [("gpu".into(), 125e6)].into_iter().collect(),
            Default::default(),
            true,
        );
        session.worker = Some(worker);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut services = FakeServices { calls: vec![] };
        assert!(session
            .cleanup(true, &mut services, Duration::from_millis(20))
            .is_err());
        for (i, cap) in CAPS.iter().enumerate() {
            assert!(session.fans.holds(i + 1, *cap));
        }
        assert_eq!(
            hardware::number(session.paths.resolve(PL1), 1.0),
            Some(10e6)
        );
        assert!(services.calls.is_empty());
        release_tx.send(()).unwrap();
        assert!(session
            .worker
            .as_ref()
            .unwrap()
            .stop(Duration::from_secs(1)));
        assert_eq!(*writes.lock().unwrap(), vec![125e6]);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stop_hook_refuses_unverifiable_owner_and_failure_never_starts_service() {
        let (root, session) = fixture_session();
        std::fs::create_dir(&session.paths.run).unwrap();
        std::os::unix::fs::symlink(root.join("target"), session.paths.owner()).unwrap();
        let mut services = FakeServices { calls: vec![] };
        assert_eq!(
            stop_hook(&session.paths, &session.config, &mut services, true),
            1
        );
        assert_eq!(
            hardware::text(session.fans.path(1, "manual")).as_deref(),
            Some("0")
        );
        assert!(services.calls.is_empty());
        std::fs::remove_file(session.paths.owner()).unwrap();
        assert_eq!(
            stop_hook(&session.paths, &session.config, &mut services, false),
            0
        );
        assert!(services.calls.is_empty());
        let mut session = session;
        session.finished = true;
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unheld_stale_pl2_and_degraded_pins_remain_conservative() {
        let mut e = engine();
        e.budget.pl2_held = false;
        let mut f = frame(4.0, 1);
        f.pl2 = None;
        f.override_ = Override::parse("gpu=200 pl1=10", 0.0, &e.config);
        e.step(f);
        assert_eq!(e.budget.cpu_min, 413.0);
        assert!(e.budget.feasible());
        assert_eq!(e.budget.pl1, 10.0);
        e.budget.degraded = true;
        e.dc_missing_since = Some(0.0);
        let mut f = frame(24.0, 2);
        f.pl2 = None;
        f.dc = None;
        f.override_ = Override::parse("gpu=200 pl1=10", 0.0, &e.config);
        e.step(f);
        assert!(e.budget.degraded);
        assert!(e.budget.feasible());
        assert_eq!(e.budget.cpu_w(), 413.0);
    }
    #[test]
    fn stop_hook_uses_defaults_for_infeasible_config() {
        let (root, mut session) = fixture_session();
        let bad = Config::from_text("BASE=1100").unwrap();
        assert!(bad.check_feasible(0, &BTreeMap::new()).is_err());
        let mut services = FakeServices { calls: vec![] };
        assert_eq!(stop_hook(&session.paths, &bad, &mut services, true), 0);
        assert_eq!(
            hardware::number(session.paths.resolve(PL1), 1.0),
            Some(413e6)
        );
        assert_eq!(services.calls.len(), 3);
        session.finished = true;
        std::fs::remove_dir_all(root).unwrap();
    }
}
