//! Linux sysfs adapters. Paths are injected by offline tests, never by the product CLI.
use crate::{
    config::{Config, SensorSpec},
    control::Filter,
    os::Lock,
    transaction::PowerIo,
    CAPS, RAPL,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};
#[derive(Clone, Debug)]
pub struct Paths {
    pub sys: PathBuf,
    pub run: PathBuf,
}
impl Default for Paths {
    fn default() -> Self {
        Self {
            sys: "/sys".into(),
            run: "/run/macpro-fand".into(),
        }
    }
}
impl Paths {
    pub fn sys(&self, path: &str) -> PathBuf {
        self.sys
            .join(path.trim_start_matches("/sys/").trim_start_matches('/'))
    }
    pub fn resolve(&self, path: &str) -> PathBuf {
        if path.starts_with("/sys/") {
            self.sys(path)
        } else {
            path.into()
        }
    }
    pub fn owner(&self) -> PathBuf {
        self.run.join("owner.lock")
    }
    pub fn smc_lock(&self) -> PathBuf {
        self.run.join("smc.lock")
    }
    pub fn override_file(&self) -> PathBuf {
        self.run.join("override")
    }
}
pub fn text(path: impl AsRef<Path>) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut s = String::new();
    f.take(8193).read_to_string(&mut s).ok()?;
    if s.len() > 8192 {
        None
    } else {
        Some(s.trim().to_string())
    }
}
pub fn number(path: impl AsRef<Path>, scale: f64) -> Option<f64> {
    text(path)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v / scale)
}
pub fn write(path: impl AsRef<Path>, value: impl std::fmt::Display) -> io::Result<()> {
    // A missing sysfs attribute is an error, never a newly created regular file.
    let mut f = OpenOptions::new().write(true).truncate(true).open(path)?;
    write_value(&mut f, value)
}
fn write_value(f: &mut impl Write, value: impl std::fmt::Display) -> io::Result<()> {
    // sysfs consumes a complete value in ONE write. write_fmt can split the
    // number and newline into separate, independently parsed stores. A short
    // write is unconfirmed; never retry its suffix as another power limit.
    let bytes = format!("{value}\n");
    if f.write(bytes.as_bytes())? == bytes.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial sysfs attribute write",
        ))
    }
}
pub fn children(path: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut v: Vec<_> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}
fn named_children(path: impl AsRef<Path>, prefix: &str) -> Vec<PathBuf> {
    children(path)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect()
}
pub fn hwmon_named(paths: &Paths, name: &str) -> Vec<PathBuf> {
    named_children(paths.sys("class/hwmon"), "hwmon")
        .into_iter()
        .filter(|h| text(h.join("name")).as_deref() == Some(name))
        .collect()
}
pub fn temp_labels(h: &Path) -> BTreeMap<String, PathBuf> {
    children(h)
        .into_iter()
        .filter_map(|p| {
            let file = p.file_name()?.to_str()?;
            let prefix = file.strip_prefix("temp")?.strip_suffix("_label")?;
            if !prefix.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some((text(&p)?, h.join(format!("temp{prefix}"))))
        })
        .collect()
}
pub fn pci_chain(path: &Path) -> Vec<String> {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .components()
        .filter_map(|c| {
            let s = c.as_os_str().to_str()?;
            let b = s.as_bytes();
            if b.len() == 12 && b[4] == b':' && b[7] == b':' && b[10] == b'.' {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect()
}
pub fn pci_slot(paths: &Paths, path: &Path) -> Option<String> {
    let slots: BTreeMap<_, _> = children(paths.sys("bus/pci/slots"))
        .into_iter()
        .filter_map(|p| {
            Some((
                text(p.join("address"))?,
                p.file_name()?.to_str()?.to_string(),
            ))
        })
        .collect();
    for addr in pci_chain(path).iter().rev() {
        if let Some(name) = slots.get(&addr[..addr.len() - 2]) {
            let slot = name.split('-').next()?;
            if (1..=8).any(|n| slot == n.to_string()) {
                return Some(slot.into());
            }
        }
    }
    None
}
pub fn die_map(paths: &Paths, c: &Config) -> BTreeMap<String, usize> {
    if !c.dies.is_empty() {
        return c.dies.clone();
    }
    children(paths.sys("bus/pci/devices"))
        .into_iter()
        .filter(|d| {
            text(d.join("vendor")).as_deref() == Some("0x1002")
                && text(d.join("class")).is_some_and(|s| s.starts_with("0x03"))
        })
        .filter_map(|d| {
            let addr = d.file_name()?.to_str()?.get(5..)?.to_string();
            let bay = pci_slot(paths, &d)
                .and_then(|s| c.mpx_bay.get(&s).copied())
                .unwrap_or(2);
            Some((addr, bay))
        })
        .collect()
}
pub fn gpu_device(paths: &Paths, addr: &str) -> Option<PathBuf> {
    children(paths.sys("bus/pci/devices"))
        .into_iter()
        .find(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&format!(":{addr}")))
        })
}
pub fn gpu_model_ok(paths: &Paths, addr: &str) -> bool {
    gpu_device(paths, addr)
        .and_then(|d| text(d.join("device")))
        .is_some_and(|s| {
            s.len() == 6 && s.starts_with("0x66a") && s.as_bytes()[5].is_ascii_hexdigit()
        })
}
pub fn devices(paths: &Paths) -> BTreeMap<String, usize> {
    let mlx: BTreeSet<_> = children(paths.sys("bus/pci/devices"))
        .into_iter()
        .filter(|d| {
            text(d.join("vendor")).as_deref() == Some("0x15b3") && !d.join("physfn").exists()
        })
        .filter_map(|d| {
            let s = d.file_name()?.to_str()?;
            Some(s.get(..s.len().checked_sub(2)?)?.to_string())
        })
        .collect();
    let nvme = named_children(paths.sys("class/nvme"), "nvme")
        .iter()
        .filter(|n| {
            pci_chain(&n.join("device"))
                .last()
                .and_then(|a| text(paths.sys(&format!("bus/pci/devices/{a}/vendor"))))
                .as_deref()
                != Some("0x106b")
        })
        .count();
    let disk = named_children(paths.sys("block"), "sd")
        .iter()
        .filter(|d| text(d.join("queue/rotational")).as_deref() == Some("1"))
        .count();
    [
        ("mellanox".into(), mlx.len()),
        ("nvme".into(), nvme),
        ("disk".into(), disk),
    ]
    .into_iter()
    .collect()
}
pub fn find_smc(paths: &Paths) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for h in hwmon_named(paths, "applesmc") {
        candidates.push(h.clone());
        candidates.push(h.join("device"));
    }
    candidates.extend(named_children(paths.sys("bus/acpi/devices"), "APP0001:"));
    candidates.push(paths.sys("devices/platform/applesmc.768"));
    candidates
        .into_iter()
        .find(|p| p.join("fan1_manual").exists())
}
pub fn sane(v: Option<f64>, default: f64) -> f64 {
    v.filter(|v| (60.0..=130.0).contains(v)).unwrap_or(default)
}
#[derive(Clone, Debug)]
pub struct DieSpec {
    pub addr: String,
    pub bay: usize,
    pub paths: [PathBuf; 3],
    pub cap: PathBuf,
    pub jcrit: f64,
    pub mcrit: f64,
}
impl DieSpec {
    pub fn discover(paths: &Paths, addr: &str, bay: usize) -> Option<Self> {
        let dev = gpu_device(paths, addr)?;
        let hw = named_children(dev.join("hwmon"), "hwmon")
            .into_iter()
            .next()?;
        let labels = temp_labels(&hw);
        let junction = labels.get("junction")?;
        let mem = labels.get("mem")?;
        let with = |p: &Path, suffix: &str| PathBuf::from(format!("{}{suffix}", p.display()));
        let mut power = hw.join("power1_average");
        if !power.exists() {
            power = hw.join("power1_input");
        }
        Some(Self {
            addr: addr.into(),
            bay,
            paths: [with(junction, "_input"), with(mem, "_input"), power],
            cap: hw.join("power1_cap"),
            jcrit: sane(number(with(junction, "_crit"), 1000.0), 105.0),
            mcrit: sane(number(with(mem, "_crit"), 1000.0), 95.0),
        })
    }
    fn read(&self) -> [Option<f64>; 3] {
        [
            number(&self.paths[0], 1000.0),
            number(&self.paths[1], 1000.0),
            number(&self.paths[2], 1e6),
        ]
    }
}
#[derive(Clone, Debug)]
pub struct Die {
    pub spec: DieSpec,
    pub filters: [Filter; 3],
    pub last_seq: Option<u64>,
    pub last_taken: Option<Instant>,
}
impl Die {
    pub fn new(spec: DieSpec) -> Self {
        Self {
            spec,
            filters: [Filter::new(4.0), Filter::new(4.0), Filter::new(10.0)],
            last_seq: None,
            last_taken: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct GpuSample {
    pub values: [Option<f64>; 3],
    pub taken: Instant,
    pub seq: u64,
}
/// Stop signal for a sampling thread: it sleeps until its next round or until stopped, with no polling wakeups.
#[derive(Default)]
pub struct Stopper {
    stopped: Mutex<bool>,
    cv: Condvar,
}
impl Stopper {
    pub fn stop(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
    pub fn stopped(&self) -> bool {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Sleep for `period` or until stopped; true if stopped.
    pub fn wait(&self, period: Duration) -> bool {
        let guard = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, period, |stopped| !*stopped)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

pub struct GpuReader {
    vals: Arc<Mutex<BTreeMap<String, GpuSample>>>,
    stop: Arc<Stopper>,
}
impl GpuReader {
    pub fn start(dies: Vec<DieSpec>, stall_after: Option<Duration>) -> io::Result<Self> {
        let vals = Arc::new(Mutex::new(BTreeMap::new()));
        let store = vals.clone();
        let stop = Arc::new(Stopper::default());
        let done = stop.clone();
        thread::Builder::new()
            .name("gpu-reader".into())
            .spawn(move || {
                let start = Instant::now();
                let mut seq = 0u64;
                while !done.stopped() {
                    if stall_after.is_some_and(|s| start.elapsed() > s) {
                        while !done.wait(Duration::from_secs(60)) {}
                        break;
                    }
                    for d in &dies {
                        if done.stopped() {
                            return;
                        }
                        let taken = Instant::now();
                        let values = d.read();
                        seq = seq.wrapping_add(1);
                        store
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(d.addr.clone(), GpuSample { values, taken, seq });
                    }
                    if done.wait(Duration::from_secs(2)) {
                        return;
                    }
                }
            })?;
        Ok(Self { vals, stop })
    }
    pub fn latest(&self) -> BTreeMap<String, GpuSample> {
        self.vals.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
impl Drop for GpuReader {
    fn drop(&mut self) {
        self.stop.stop();
    }
}
#[derive(Clone, Debug)]
pub struct Sensor {
    pub path: PathBuf,
    pub scale: f64,
    pub filter: Filter,
}
impl Sensor {
    pub fn new(path: PathBuf, scale: f64, tau: f64) -> Self {
        Self {
            path,
            scale,
            filter: Filter::new(tau),
        }
    }
    pub fn update(&mut self, dt: f64) -> bool {
        self.filter.feed(number(&self.path, self.scale), dt)
    }
}
pub struct Sensors {
    pub cpu: Sensor,
    pub cpu_crit: f64,
    pub ambient: BTreeMap<String, (SensorSpec, Sensor)>,
}
impl Sensors {
    pub fn discover(paths: &Paths, smc: &Path, c: &Config) -> Result<Self, String> {
        let core = hwmon_named(paths, "coretemp")
            .into_iter()
            .next()
            .ok_or("coretemp not found")?;
        let pkg = temp_labels(&core)
            .get("Package id 0")
            .cloned()
            .ok_or("coretemp 'Package id 0' not found")?;
        let cpu = Sensor::new(format!("{}_input", pkg.display()).into(), 1000.0, 4.0);
        let cpu_crit = sane(number(format!("{}_crit", pkg.display()), 1000.0), 92.0);
        let labels = temp_labels(smc);
        let mut ambient = BTreeMap::new();
        for (k, spec) in &c.smc {
            let prefix = labels
                .get(k)
                .ok_or_else(|| format!("SMC sensor {k} not found"))?;
            ambient.insert(
                k.clone(),
                (
                    spec.clone(),
                    Sensor::new(format!("{}_input", prefix.display()).into(), 1000.0, 10.0),
                ),
            );
        }
        for (name, (target, band, default_crit)) in &c.periph {
            for h in hwmon_named(paths, name) {
                let input = h.join("temp1_input");
                if !input.exists() {
                    continue;
                }
                let pci = pci_chain(&h.join("device")).last().cloned();
                let crit = number(h.join("temp1_crit"), 1000.0)
                    .filter(|v| (50.0..=130.0).contains(v))
                    .unwrap_or(*default_crit);
                let internal = name == "nvme"
                    && pci
                        .as_ref()
                        .and_then(|a| text(paths.sys(&format!("bus/pci/devices/{a}/vendor"))))
                        .as_deref()
                        == Some("0x106b");
                let (key, spec) = if internal {
                    let mut s = c.nvme.clone();
                    s.emergency = crit - 5.0;
                    ("nvme".into(), s)
                } else {
                    let zone = if name == "drivetemp" {
                        c.disk_zone
                    } else {
                        pci_slot(paths, &h.join("device"))
                            .and_then(|s| c.slot_zone.get(&s).copied())
                            .unwrap_or(3)
                    };
                    let dev = fs::canonicalize(h.join("device")).unwrap_or_else(|_| h.clone());
                    let basename = dev.file_name().unwrap_or_default().to_string_lossy();
                    (
                        format!("{name}:{basename}"),
                        SensorSpec {
                            fan: zone,
                            target: *target,
                            band: *band,
                            emergency: crit - 5.0,
                        },
                    )
                };
                ambient.insert(key, (spec, Sensor::new(input, 1000.0, 10.0)));
            }
        }
        Ok(Self {
            cpu,
            cpu_crit,
            ambient,
        })
    }
}
#[derive(Clone)]
pub struct FsPowerIo {
    pub paths: Paths,
}
impl PowerIo for FsPowerIo {
    fn read(&mut self, path: &str) -> Option<f64> {
        number(self.paths.resolve(path), 1.0)
    }
    fn write(&mut self, path: &str, uw: f64) -> bool {
        uw.is_finite()
            && uw >= crate::LIMIT_MIN_UW
            && uw <= i64::MAX as f64
            && write(self.paths.resolve(path), uw as i64).is_ok()
    }
}
pub fn rapl_state(paths: &Paths) -> (bool, bool) {
    let zone = paths.resolve(RAPL);
    (
        text(zone.join("enabled")).as_deref() == Some("1")
            && text(zone.join("constraint_0_name")).as_deref() == Some("long_term"),
        text(zone.join("constraint_1_name")).as_deref() == Some("short_term"),
    )
}
pub fn cap_paths(paths: &Paths, c: &Config) -> (BTreeMap<String, String>, Vec<String>) {
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    for addr in die_map(paths, c).keys() {
        let cap = gpu_device(paths, addr).and_then(|d| {
            named_children(d.join("hwmon"), "hwmon")
                .into_iter()
                .map(|h| h.join("power1_cap"))
                .find(|p| p.exists())
        });
        if let Some(p) = cap {
            found.insert(addr.clone(), p.to_string_lossy().into_owned());
        } else {
            missing.push(addr.clone());
        }
    }
    (found, missing)
}
#[derive(Clone)]
pub struct Fans {
    pub smc: PathBuf,
}
impl Fans {
    pub fn path(&self, fan: usize, key: &str) -> PathBuf {
        self.smc.join(format!("fan{fan}_{key}"))
    }
    pub fn put(&self, fan: usize, rpm: f64) -> io::Result<()> {
        write(self.path(fan, "manual"), 1)?;
        write(self.path(fan, "output"), rpm as i64)
    }
    pub fn full(&self) -> bool {
        let mut ok = true;
        for (i, cap) in CAPS.iter().enumerate() {
            if let Err(e) = self.put(i + 1, *cap) {
                crate::log(&[("error", format!("fan{} write failed: {e}", i + 1).into())]);
                ok = false;
            }
        }
        ok
    }
    pub fn holds(&self, fan: usize, rpm: f64) -> bool {
        text(self.path(fan, "manual")).as_deref() == Some("1")
            && number(self.path(fan, "output"), 1.0).is_some_and(|r| (r - rpm).abs() <= 5.0)
    }
}

/// DC key sampling is isolated too: a stuck SMC read cannot freeze the fan loop.
/// Publication carries the read's start time; stale/missing DC pauses allocation.
type DcSample = Option<(Option<f64>, Instant)>;
pub struct DcReader {
    sample: Arc<Mutex<DcSample>>,
    stop: Arc<Stopper>,
}
impl DcReader {
    pub fn start(paths: Paths, smc: PathBuf) -> io::Result<Self> {
        let sample = Arc::new(Mutex::new(None));
        let store = sample.clone();
        let stop = Arc::new(Stopper::default());
        let done = stop.clone();
        thread::Builder::new()
            .name("smc-dc-reader".into())
            .spawn(move || {
                let mut index = None;
                let mut next_try = Instant::now();
                while !done.stopped() {
                    if index.is_none() && Instant::now() >= next_try {
                        next_try = Instant::now() + Duration::from_secs(30);
                        index = lookup_key(&paths, &smc, "PZ0G");
                    }
                    let taken = Instant::now();
                    let v = index.and_then(|i| read_key(&paths, &smc, "PZ0G", i));
                    *store.lock().unwrap_or_else(|e| e.into_inner()) = Some((v, taken));
                    if done.wait(Duration::from_secs(2)) {
                        return;
                    }
                }
            })?;
        Ok(Self { sample, stop })
    }
    pub fn latest(&self, now: Instant) -> Option<(Option<f64>, Instant)> {
        self.sample
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .filter(|(_, t)| now.saturating_duration_since(*t) <= Duration::from_secs(10))
    }
}
impl Drop for DcReader {
    fn drop(&mut self) {
        self.stop.stop();
    }
}
fn lookup_key(paths: &Paths, smc: &Path, key: &str) -> Option<usize> {
    let n = number(smc.join("key_count"), 1.0)?;
    if !(0.0..=16384.0).contains(&n) || n.fract() != 0.0 {
        return None;
    }
    let lock = Lock::open(&paths.smc_lock()).ok()?;
    if !lock.acquire(Duration::from_secs(2)).ok()? {
        return None;
    }
    for i in 0..n as usize {
        write(smc.join("key_at_index"), i).ok()?;
        if text(smc.join("key_at_index_name")).as_deref() == Some(key) {
            return Some(i);
        }
    }
    None
}
fn read_key(paths: &Paths, smc: &Path, key: &str, index: usize) -> Option<f64> {
    let lock = Lock::open(&paths.smc_lock()).ok()?;
    if !lock.acquire(Duration::from_millis(200)).ok()? {
        return None;
    }
    write(smc.join("key_at_index"), index).ok()?;
    if text(smc.join("key_at_index_name")).as_deref() != Some(key) {
        return None;
    }
    let mut b = [0u8; 4];
    File::open(smc.join("key_at_index_data"))
        .ok()?
        .read_exact(&mut b)
        .ok()?;
    let v = f32::from_le_bytes(b) as f64;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn put(p: impl AsRef<Path>, text: &str) {
        let p = p.as_ref();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    #[test]
    fn sampler_sleep_has_no_polling_wakeups_and_stops_at_once() {
        let s = Arc::new(Stopper::default());
        let t0 = Instant::now();
        assert!(
            !s.wait(Duration::from_millis(120)),
            "a full period passes when nobody stops it"
        );
        assert!(t0.elapsed() >= Duration::from_millis(120));
        let waiter = {
            let s = s.clone();
            thread::spawn(move || {
                let t = Instant::now();
                (s.wait(Duration::from_secs(30)), t.elapsed())
            })
        };
        thread::sleep(Duration::from_millis(50));
        s.stop();
        let (stopped, waited) = waiter.join().unwrap();
        assert!(
            stopped && waited < Duration::from_secs(2),
            "stop wakes the sampler at once: {waited:?}"
        );
        assert!(
            s.stopped() && s.wait(Duration::from_secs(30)),
            "a stopped sampler never sleeps again"
        );
    }
    #[test]
    fn fixture_inventory_and_rapl_fail_closed() {
        let root = crate::os::tests::temp();
        let p = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        for (a, v, class) in [
            ("0000:0b:00.0", "0x1002", "0x030000"),
            ("0000:0c:00.0", "0x1002", "0x030200"),
            ("0000:06:00.0", "0x15b3", "0x020000"),
            ("0000:06:00.1", "0x15b3", "0x020000"),
        ] {
            put(p.sys(&format!("bus/pci/devices/{a}/vendor")), v);
            put(p.sys(&format!("bus/pci/devices/{a}/class")), class);
        }
        put(p.sys("block/sda/queue/rotational"), "1");
        put(p.sys("block/sdb/queue/rotational"), "0");
        assert_eq!(die_map(&p, &Config::default()).len(), 2);
        assert_eq!(devices(&p)["mellanox"], 1);
        assert_eq!(devices(&p)["disk"], 1);
        assert_eq!(rapl_state(&p), (false, false));
        put(p.resolve(RAPL).join("enabled"), "1");
        put(p.resolve(RAPL).join("constraint_0_name"), "long_term");
        put(p.resolve(RAPL).join("constraint_1_name"), "short_term");
        assert_eq!(rapl_state(&p), (true, true));
        let mut io = FsPowerIo { paths: p.clone() };
        assert!(!io.write(crate::PL1, 0.0));
        assert!(!p.resolve(crate::PL1).exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn sysfs_values_are_one_complete_write_and_partial_stores_fail() {
        struct Store {
            calls: usize,
            value: Option<i64>,
            partial: bool,
        }
        impl Write for Store {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                self.value = std::str::from_utf8(b)
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok());
                if self.value.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid sysfs store",
                    ));
                }
                Ok(if self.partial { b.len() - 1 } else { b.len() })
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for partial in [false, true] {
            let mut s = Store {
                calls: 0,
                value: None,
                partial,
            };
            let result = write_value(&mut s, 125_000_000i64);
            assert_eq!(result.is_ok(), !partial);
            assert_eq!(s.calls, 1);
            assert_eq!(s.value, Some(125_000_000));
        }
    }
    #[test]
    fn shared_key_lock_and_binary_float() {
        let root = crate::os::tests::temp();
        let paths = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        let smc = paths.sys("smc");
        put(smc.join("key_count"), "1");
        put(smc.join("key_at_index"), "0");
        put(smc.join("key_at_index_name"), "PZ0G");
        fs::write(smc.join("key_at_index_data"), 1234.0f32.to_le_bytes()).unwrap();
        assert_eq!(lookup_key(&paths, &smc, "PZ0G"), Some(0));
        assert_eq!(read_key(&paths, &smc, "PZ0G", 0), Some(1234.0));
        let lock = Lock::open(&paths.smc_lock()).unwrap();
        lock.acquire(Duration::ZERO).unwrap();
        assert_eq!(read_key(&paths, &smc, "PZ0G", 0), None);
        drop(lock);
        put(smc.join("key_at_index_name"), "WRONG");
        assert_eq!(read_key(&paths, &smc, "PZ0G", 0), None);
        fs::remove_dir_all(root).unwrap();
    }
}
