//! Sensor and fan I/O off the control thread.
//!
//! Every sysfs read or write that can wait on a device runs here, never in the control loop that sends the systemd
//! watchdog keepalive: a T2/SMC read that hangs for half a minute (2026-10-22: the control thread blocked in one
//! read for more than 20 s and the watchdog killed the daemon) or a drive that holds a temperature command through
//! a spin-up or an error recovery only makes its readings stale. Stale readings raise the read alarms (every fan to
//! its cap) exactly as failed reads did before.
//!
//! * [`Platform`] owns the SMC fan registers and the platform temperatures in two threads: the fan thread applies
//!   the control loop's fan commands at once and reads the tachometers and fan registers (re-asserting a fan that
//!   another writer changed) every round; the temperature thread reads the CPU package and the SMC temperature keys.
//! * [`DeviceReader`] polls one peripheral temperature sensor (NVMe, drivetemp, mlx5) at its class's period.
//! * [`Scanner`] re-discovers the peripheral sensors once a minute (driver reloads renumber hwmon; new drivers add
//!   sensors).
//!
//! What stays in the control thread: the override file (tmpfs), the RAPL powercap registers (MSR reads, no device
//! I/O) and the journal.
use crate::{
    config::Config,
    hardware::{number, Fans, Paths},
    inventory::{self, PeriphSensor},
    CAPS, PERIOD,
};
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

/// A platform reading older than this (s) is no reading.
pub const PLATFORM_STALE: f64 = 10.0;
/// The control loop withholds the watchdog keepalive once the platform thread has not finished a round for this
/// long (s): a stall that outlasts it goes to systemd's watchdog restart and failure path; a shorter one is ridden
/// through on the read alarms.
pub const IO_STALL: f64 = 60.0;
/// A peripheral sensor whose driver is gone (device still present) for this long (s) is retired: a driver reload, a
/// device unbound for a VM, or a driver left unloaded. Its zone keeps the device's `UNMONITORED` floor. Shorter than
/// every class's staleness limit less its period, so an unloaded driver retires its sensors before they raise the
/// stale-reading alarm (a driver reload is not a thermal event).
pub const RETIRE_UNBOUND: f64 = 20.0;
/// A peripheral sensor that has read nothing for this long (s) while its driver is present is retired too (a
/// failed drive's temperature command keeps failing; the drive is not a thermal emergency).
pub const RETIRE_FAILING: f64 = 600.0;
/// Peripheral sensor rediscovery period (s).
pub const SCAN_PERIOD: f64 = 60.0;
/// A fan's registers are checked against its setting only this long (s) after the setting was written: the SMC
/// reflects a write in its registers with a delay, and 2.0.1 checked a loop (2 s) after writing. Checking at once
/// made a fan that ramps every loop look changed by another writer (alarm, every fan to its cap; 2026-10-23).
pub const VERIFY_SETTLE: f64 = 1.5;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stop/kick signal for an I/O thread: it sleeps until its next round, a kick (new work) or a stop, with no
/// polling wakeups.
#[derive(Default)]
pub struct Signal {
    state: Mutex<(bool, bool)>,
    cv: Condvar,
}
impl Signal {
    pub fn stop(&self) {
        lock(&self.state).0 = true;
        self.cv.notify_all();
    }
    pub fn kick(&self) {
        lock(&self.state).1 = true;
        self.cv.notify_all();
    }
    pub fn stopped(&self) -> bool {
        lock(&self.state).0
    }
    /// Sleep for `period`, until kicked or until stopped; true if stopped. Consumes the kick.
    pub fn wait(&self, period: Duration) -> bool {
        let g = lock(&self.state);
        let (mut g, _) = self
            .cv
            .wait_timeout_while(g, period, |(stopped, kicked)| !*stopped && !*kicked)
            .unwrap_or_else(|e| e.into_inner());
        g.1 = false;
        g.0
    }
}

/// One temperature round of the platform reader: values are None where a read failed.
#[derive(Clone, Debug)]
pub struct TempSample {
    /// Start of the round's reads.
    pub taken: Instant,
    pub cpu_temp: Option<f64>,
    pub smc: BTreeMap<String, Option<f64>>,
}

/// One round of the fan thread.
#[derive(Clone, Debug)]
pub struct FanSample {
    pub taken: Instant,
    pub tach: [Option<f64>; 4],
    /// Fans found off their written setting this round (re-asserted).
    pub overwritten: Vec<usize>,
}

/// What the platform threads read: the coretemp package input and the configured SMC temperature inputs; the fans.
#[derive(Clone, Debug)]
pub struct PlatformSpec {
    pub cpu: PathBuf,
    pub smc: BTreeMap<String, PathBuf>,
    pub fans: Fans,
}

/// Fault injection for the hardware tests (`MACPRO_FAND_TEST_STALL_TEMPS` / `_FANS`): the thread parks for good this
/// long after it starts, as on a read that never returns.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stall {
    pub temps: Option<Duration>,
    pub fans: Option<Duration>,
}

#[derive(Default)]
struct FanState {
    /// The control loop's setting per fan.
    command: [Option<f64>; 4],
    /// Cleanup has taken the fans: no further writes from this thread.
    frozen: bool,
    /// The first failed fan write (the control loop ends its session on it).
    error: Option<String>,
}

/// The platform I/O: a fan thread (the control loop's fan commands, the fan registers and tachometers) and a
/// temperature thread (CPU package, SMC keys). They are separate so a temperature read that hangs only makes the
/// temperatures stale: the read alarms follow and the fan thread still writes every fan to its cap.
pub struct Platform {
    temps: Arc<Mutex<Option<TempSample>>>,
    fans: Arc<Mutex<Option<FanSample>>>,
    fan: Arc<Mutex<FanState>>,
    /// Held for every fan write, so `freeze` can wait out a write in flight.
    write: Arc<Mutex<()>>,
    fan_signal: Arc<Signal>,
    temp_signal: Arc<Signal>,
    round: Arc<Mutex<Instant>>,
}
fn park(stall: Option<Duration>, start: Instant, signal: &Signal) -> bool {
    if stall.is_some_and(|s| start.elapsed() > s) {
        while !signal.wait(Duration::from_secs(60)) {}
        return true;
    }
    false
}
impl Platform {
    /// Start with the fans already holding `written` (the start-up handover); None = read only (dry run): no
    /// fan is ever written or re-asserted.
    pub fn start(spec: PlatformSpec, written: Option<[f64; 4]>, stall: Stall) -> io::Result<Self> {
        let initial = written.map(|w| w.map(Some)).unwrap_or([None; 4]);
        let p = Self {
            temps: Arc::new(Mutex::new(None)),
            fans: Arc::new(Mutex::new(None)),
            fan: Arc::new(Mutex::new(FanState {
                command: initial,
                ..Default::default()
            })),
            write: Arc::new(Mutex::new(())),
            fan_signal: Arc::new(Signal::default()),
            temp_signal: Arc::new(Signal::default()),
            round: Arc::new(Mutex::new(Instant::now())),
        };
        let (temps, signal) = (p.temps.clone(), p.temp_signal.clone());
        let tspec = spec.clone();
        thread::Builder::new()
            .name("platform-temps".into())
            .spawn(move || {
                let start = Instant::now();
                while !signal.stopped() && !park(stall.temps, start, &signal) {
                    let taken = Instant::now();
                    let cpu_temp = number(&tspec.cpu, 1000.0);
                    let smc = tspec
                        .smc
                        .iter()
                        .map(|(k, p)| (k.clone(), number(p, 1000.0)))
                        .collect();
                    *lock(&temps) = Some(TempSample {
                        taken,
                        cpu_temp,
                        smc,
                    });
                    if signal.wait(Duration::from_secs_f64(PERIOD)) {
                        return;
                    }
                }
            })?;
        let (fans, fan, write, signal, round) = (
            p.fans.clone(),
            p.fan.clone(),
            p.write.clone(),
            p.fan_signal.clone(),
            p.round.clone(),
        );
        let f = spec.fans;
        let spawned = thread::Builder::new().name("fan-io".into()).spawn(move || {
            let start = Instant::now();
            let mut last = initial;
            // The start-up handover verified these settings already.
            let mut written: [Option<Instant>; 4] = [None; 4];
            let settle = Duration::from_secs_f64(VERIFY_SETTLE);
            while !signal.stopped() && !park(stall.fans, start, &signal) {
                // Check first, then write: a setting is checked a round after it was written.
                let taken = Instant::now();
                let tach = std::array::from_fn(|i| number(f.path(i + 1, "input"), 1.0));
                let mut overwritten = Vec::new();
                for (i, w) in last.iter().enumerate() {
                    let Some(w) = *w else { continue };
                    if written[i].is_some_and(|t| t.elapsed() < settle) {
                        continue;
                    }
                    if !f.holds(i + 1, w) {
                        overwritten.push(i + 1);
                        let _g = lock(&write);
                        let mut st = lock(&fan);
                        if !st.frozen {
                            if let Err(e) = f.put(i + 1, w) {
                                st.error
                                    .get_or_insert(format!("reassert fan{}: {e}", i + 1));
                            }
                            written[i] = Some(Instant::now());
                        }
                    }
                }
                *lock(&fans) = Some(FanSample {
                    taken,
                    tach,
                    overwritten,
                });
                *lock(&round) = Instant::now();
                apply(&f, &fan, &write, &mut last, &mut written);
                if signal.wait(Duration::from_secs_f64(PERIOD)) {
                    return;
                }
            }
        });
        if let Err(e) = spawned {
            p.temp_signal.stop();
            return Err(e);
        }
        Ok(p)
    }
    /// New settings for the fans given (None = unchanged); written at once.
    pub fn command(&self, rpm: [Option<f64>; 4]) {
        {
            let mut st = lock(&self.fan);
            for (c, r) in st.command.iter_mut().zip(rpm) {
                if r.is_some() {
                    *c = r;
                }
            }
        }
        self.fan_signal.kick();
    }
    pub fn temps(&self) -> Option<TempSample> {
        lock(&self.temps).clone()
    }
    pub fn fans(&self) -> Option<FanSample> {
        lock(&self.fans).clone()
    }
    pub fn error(&self) -> Option<String> {
        lock(&self.fan).error.clone()
    }
    /// Seconds since the fan thread last finished a round (or started).
    pub fn round_age(&self, now: Instant) -> f64 {
        now.saturating_duration_since(*lock(&self.round))
            .as_secs_f64()
    }
    /// Take the fans away from the fan thread: no write starts after this returns. Waits at most `wait` for a write
    /// in flight; false if one is still stuck (the caller writes the caps regardless).
    pub fn freeze(&self, wait: Duration) -> bool {
        lock(&self.fan).frozen = true;
        self.fan_signal.stop();
        self.temp_signal.stop();
        let end = Instant::now() + wait;
        loop {
            if self.write.try_lock().is_ok() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Platform {
    fn drop(&mut self) {
        self.fan_signal.stop();
        self.temp_signal.stop();
    }
}

/// Write every fan whose command differs from what was last written. The frozen check and the write happen under
/// the write lock, so a write never lands after `freeze` returned.
fn apply(
    fans: &Fans,
    fan: &Mutex<FanState>,
    write: &Mutex<()>,
    last: &mut [Option<f64>; 4],
    written: &mut [Option<Instant>; 4],
) {
    let _g = lock(write);
    let (command, frozen) = {
        let st = lock(fan);
        (st.command, st.frozen)
    };
    if frozen {
        return;
    }
    for i in 0..4 {
        let Some(rpm) = command[i] else { continue };
        if last[i] == Some(rpm) {
            continue;
        }
        let r = fans.put(i + 1, rpm.clamp(0.0, CAPS[i]));
        written[i] = Some(Instant::now());
        match r {
            Ok(()) => last[i] = Some(rpm),
            Err(e) => {
                lock(fan)
                    .error
                    .get_or_insert(format!("write fan{}: {e}", i + 1));
                return;
            }
        }
    }
}

/// The state of one peripheral sensor as its reader last saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeviceStatus {
    /// A reading no older than the class's staleness limit.
    Fresh(f64),
    /// No reading within the staleness limit; `for_s` since the last good one (or since the reader started).
    Stale { for_s: f64, unbound: bool },
    /// The device itself is gone from sysfs.
    Gone,
}

#[derive(Default)]
struct DeviceShared {
    last: Option<(f64, Instant)>,
    unbound_since: Option<Instant>,
    gone: bool,
}

pub struct DeviceReader {
    pub sensor: PeriphSensor,
    pub period: f64,
    pub stale: f64,
    started: Instant,
    shared: Arc<Mutex<DeviceShared>>,
    signal: Arc<Signal>,
}
impl DeviceReader {
    pub fn start(sensor: PeriphSensor, c: &Config) -> io::Result<Self> {
        let (period, stale) = c.poll(&sensor.class);
        let r = Self {
            sensor,
            period,
            stale,
            started: Instant::now(),
            shared: Arc::new(Mutex::new(DeviceShared::default())),
            signal: Arc::new(Signal::default()),
        };
        let (dev, class, command, shared, signal) = (
            r.sensor.dev.clone(),
            r.sensor.class.clone(),
            r.sensor.command.clone(),
            r.shared.clone(),
            r.signal.clone(),
        );
        // A command gets at most one period (and at most 10 s) before it is killed.
        let deadline = Duration::from_secs_f64(period.min(10.0));
        thread::Builder::new()
            .name(format!("sensor-{}", r.sensor.key))
            .stack_size(256 * 1024)
            .spawn(move || {
                while !signal.stopped() {
                    let taken = Instant::now();
                    if !dev.exists() {
                        lock(&shared).gone = true;
                    } else if let Some(argv) = &command {
                        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
                        let v = crate::os::command(&argv[0], &args, deadline)
                            .ok()
                            .filter(|r| r.success)
                            .and_then(|r| r.stdout.split_whitespace().next()?.parse::<f64>().ok())
                            .filter(|v| (-40.0..=150.0).contains(v));
                        let mut s = lock(&shared);
                        s.gone = false;
                        if let Some(v) = v {
                            s.last = Some((v, taken));
                        }
                    } else {
                        // Re-resolved every round: a driver reload renumbers the hwmon directory.
                        match inventory::hwmon_input(&dev, &class) {
                            None => {
                                lock(&shared).unbound_since.get_or_insert(taken);
                            }
                            Some(input) => {
                                let v =
                                    number(&input, 1000.0).filter(|v| (-40.0..=150.0).contains(v));
                                let mut s = lock(&shared);
                                s.unbound_since = None;
                                s.gone = false;
                                if let Some(v) = v {
                                    s.last = Some((v, taken));
                                }
                            }
                        }
                    }
                    if signal.wait(Duration::from_secs_f64(period)) {
                        return;
                    }
                }
            })?;
        Ok(r)
    }
    pub fn status(&self, now: Instant) -> DeviceStatus {
        let s = lock(&self.shared);
        if s.gone {
            return DeviceStatus::Gone;
        }
        let age = |t: Instant| now.saturating_duration_since(t).as_secs_f64();
        match s.last {
            Some((v, t)) if age(t) <= self.stale => DeviceStatus::Fresh(v),
            last => DeviceStatus::Stale {
                for_s: age(last.map(|(_, t)| t).unwrap_or(self.started)),
                unbound: s.unbound_since.is_some_and(|t| age(t) >= RETIRE_UNBOUND),
            },
        }
    }
    /// Seconds since this reader started.
    pub fn running_for(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }
    /// The reading for the control loop: Some only while fresh.
    pub fn value(&self, now: Instant) -> Option<f64> {
        match self.status(now) {
            DeviceStatus::Fresh(v) => Some(v),
            _ => None,
        }
    }
    /// Retire this sensor (its zone falls back to the device's UNMONITORED floor)?
    pub fn retire(&self, now: Instant) -> Option<String> {
        match self.status(now) {
            DeviceStatus::Gone => Some("device gone".into()),
            DeviceStatus::Stale { unbound: true, .. } => {
                Some(format!("no driver for {RETIRE_UNBOUND:.0} s"))
            }
            DeviceStatus::Stale { for_s, .. } if for_s >= RETIRE_FAILING => {
                Some(format!("no reading for {for_s:.0} s"))
            }
            _ => None,
        }
    }
}
impl Drop for DeviceReader {
    fn drop(&mut self) {
        self.signal.stop();
    }
}

/// A rediscovery result: its sequence number and the sensors found.
type Scan = Option<(u64, Vec<PeriphSensor>)>;
/// Periodic peripheral sensor rediscovery: publishes the current sensor list once a minute.
pub struct Scanner {
    found: Arc<Mutex<Scan>>,
    signal: Arc<Signal>,
}
impl Scanner {
    pub fn start(paths: Paths, c: Config) -> io::Result<Self> {
        let s = Self {
            found: Arc::new(Mutex::new(None)),
            signal: Arc::new(Signal::default()),
        };
        let (found, signal) = (s.found.clone(), s.signal.clone());
        thread::Builder::new()
            .name("sensor-scan".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut seq = 0u64;
                while !signal.wait(Duration::from_secs_f64(SCAN_PERIOD)) {
                    let v = inventory::sensors(&paths, &c);
                    seq += 1;
                    *lock(&found) = Some((seq, v));
                }
            })?;
        Ok(s)
    }
    /// The latest scan, with its sequence number.
    pub fn latest(&self) -> Scan {
        lock(&self.found).clone()
    }
}
impl Drop for Scanner {
    fn drop(&mut self) {
        self.signal.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::SensorSpec, hardware::tests::put};
    use std::fs;

    /// A FIFO with no writer: open() for reading waits, like a read stuck in a driver. (coreutils mkfifo keeps the
    /// crate's only unsafe code in the os module.)
    fn fifo(p: &std::path::Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(p)
            .status()
            .unwrap()
            .success());
    }
    fn sensor(dev: PathBuf) -> PeriphSensor {
        PeriphSensor {
            key: "drivetemp:3:0:0:0".into(),
            class: "drivetemp".into(),
            dev,
            device: Some("sdb".into()),
            spec: SensorSpec {
                fan: 4,
                target: 40.0,
                band: 1.0,
                emergency: 55.0,
            },
            command: None,
        }
    }

    /// A TEMP_COMMAND sensor: the first number of the program's output; a hung program is killed at the deadline
    /// and its sensor goes stale; a failing one never produces a reading.
    #[test]
    fn command_sensor_reads_kills_hung_and_ignores_failures() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::os::tests::temp();
        let prog = root.join("temp.sh");
        put(&prog, "#!/bin/sh\ncase \"$1\" in ok) echo ' 43 ';; hang) sleep 30;; *) echo 99; exit 1;; esac\n");
        fs::set_permissions(&prog, fs::Permissions::from_mode(0o755)).unwrap();
        let mut c = Config::default();
        c.periph_poll.insert("mellanox".into(), (1.0, 2.0));
        let mk = |arg: &str| {
            let mut s = sensor(root.clone());
            s.class = "mellanox".into();
            s.command = Some(vec![prog.to_string_lossy().into_owned(), arg.into()]);
            DeviceReader::start(s, &c).unwrap()
        };
        let (ok, hang, bad) = (mk("ok"), mk("hang"), mk("fail"));
        let end = Instant::now() + Duration::from_secs(5);
        while ok.value(Instant::now()).is_none() && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(ok.value(Instant::now()), Some(43.0));
        thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            hang.value(Instant::now()),
            None,
            "killed at the 1 s deadline, never a reading"
        );
        assert_eq!(
            bad.value(Instant::now()),
            None,
            "a failed command's output is not a reading"
        );
        assert!(matches!(
            bad.status(Instant::now()),
            DeviceStatus::Stale { unbound: false, .. }
        ));
        drop((ok, hang, bad));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signal_kick_wakes_and_stop_ends() {
        let s = Arc::new(Signal::default());
        let t0 = Instant::now();
        assert!(!s.wait(Duration::from_millis(80)));
        assert!(t0.elapsed() >= Duration::from_millis(80));
        s.kick();
        let t0 = Instant::now();
        assert!(!s.wait(Duration::from_secs(30)), "a kick is not a stop");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "a pending kick returns at once"
        );
        let w = {
            let s = s.clone();
            thread::spawn(move || s.wait(Duration::from_secs(30)))
        };
        thread::sleep(Duration::from_millis(30));
        s.stop();
        assert!(w.join().unwrap());
        assert!(s.stopped() && s.wait(Duration::from_secs(30)));
    }

    /// A temperature read that blocks (a FIFO with no writer: open() waits like a drive in error recovery) never
    /// blocks the caller: the reading goes stale, the thread stays parked, status keeps answering.
    #[test]
    fn blocked_device_read_goes_stale_without_blocking_the_caller() {
        let root = crate::os::tests::temp();
        let dev = root.join("3:0:0:0");
        put(dev.join("hwmon/hwmon14/name"), "drivetemp");
        put(dev.join("hwmon/hwmon14/temp1_input"), "35000");
        let mut c = Config::default();
        c.periph_poll.insert("drivetemp".into(), (1.0, 2.0));
        let r = DeviceReader::start(sensor(dev.clone()), &c).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while r.value(Instant::now()).is_none() && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(r.value(Instant::now()), Some(35.0));
        fs::remove_file(dev.join("hwmon/hwmon14/temp1_input")).unwrap();
        fifo(&dev.join("hwmon/hwmon14/temp1_input"));
        thread::sleep(Duration::from_millis(3500));
        let t0 = Instant::now();
        let st = r.status(Instant::now());
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "status never waits on the device"
        );
        assert!(
            matches!(st, DeviceStatus::Stale { unbound: false, .. }),
            "{st:?}"
        );
        assert_eq!(r.value(Instant::now()), None);
        assert_eq!(r.retire(Instant::now()), None, "stale, but not yet retired");
        assert!(
            r.retire(Instant::now() + Duration::from_secs_f64(RETIRE_FAILING))
                .is_some(),
            "a sensor that never reads again is retired after RETIRE_FAILING"
        );
        // unblock the parked thread so the fixture can go
        let _ = fs::OpenOptions::new()
            .write(true)
            .custom_flags_nonblock()
            .open(dev.join("hwmon/hwmon14/temp1_input"));
        drop(r);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renumbered_hwmon_unbound_driver_and_removed_device() {
        let root = crate::os::tests::temp();
        let dev = root.join("3:0:0:0");
        put(dev.join("hwmon/hwmon14/name"), "drivetemp");
        put(dev.join("hwmon/hwmon14/temp1_input"), "35000");
        let mut c = Config::default();
        c.periph_poll.insert("drivetemp".into(), (1.0, 2.0));
        let r = DeviceReader::start(sensor(dev.clone()), &c).unwrap();
        let wait_for = |want: &dyn Fn(DeviceStatus) -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !want(r.status(Instant::now())) && Instant::now() < end {
                thread::sleep(Duration::from_millis(20));
            }
            r.status(Instant::now())
        };
        assert_eq!(
            wait_for(&|s| s == DeviceStatus::Fresh(35.0)),
            DeviceStatus::Fresh(35.0)
        );
        // driver reload: the hwmon comes back under a new number with a new value
        fs::rename(dev.join("hwmon/hwmon14"), dev.join("hwmon/hwmon21")).unwrap();
        put(dev.join("hwmon/hwmon21/temp1_input"), "36000");
        assert_eq!(
            wait_for(&|s| s == DeviceStatus::Fresh(36.0)),
            DeviceStatus::Fresh(36.0)
        );
        // driver unbound: stale, and retired once unbound for RETIRE_UNBOUND
        fs::remove_dir_all(dev.join("hwmon")).unwrap();
        let st = wait_for(&|s| matches!(s, DeviceStatus::Stale { .. }));
        assert!(
            matches!(st, DeviceStatus::Stale { unbound: false, .. }),
            "{st:?}"
        );
        assert!(r
            .retire(Instant::now() + Duration::from_secs_f64(RETIRE_UNBOUND + 1.0))
            .is_some_and(|m| m.contains("no driver")));
        // device removed
        fs::remove_dir_all(&dev).unwrap();
        assert_eq!(wait_for(&|s| s == DeviceStatus::Gone), DeviceStatus::Gone);
        assert_eq!(r.retire(Instant::now()).as_deref(), Some("device gone"));
        drop(r);
        fs::remove_dir_all(root).unwrap();
    }

    fn fans(root: &std::path::Path) -> Fans {
        let smc = root.join("smc");
        for i in 1..=4 {
            put(smc.join(format!("fan{i}_manual")), "0");
            put(smc.join(format!("fan{i}_output")), "0");
            put(smc.join(format!("fan{i}_input")), "1000");
        }
        Fans { smc }
    }

    #[test]
    fn platform_writes_commands_reasserts_and_freezes() {
        let root = crate::os::tests::temp();
        let f = fans(&root);
        put(root.join("cpu"), "40000");
        put(root.join("tpcd"), "45000");
        let spec = PlatformSpec {
            cpu: root.join("cpu"),
            smc: [("TPCD".to_string(), root.join("tpcd"))]
                .into_iter()
                .collect(),
            fans: f.clone(),
        };
        let start = [600.0, 1150.0, 1150.0, 500.0];
        for (i, r) in start.iter().enumerate() {
            f.put(i + 1, *r).unwrap();
        }
        let p = Platform::start(spec, Some(start), Stall::default()).unwrap();
        let until = |cond: &dyn Fn() -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !cond() && Instant::now() < end {
                thread::sleep(Duration::from_millis(10));
            }
            cond()
        };
        assert!(until(&|| p.temps().is_some() && p.fans().is_some()));
        let t = p.temps().unwrap();
        assert_eq!(t.cpu_temp, Some(40.0));
        assert_eq!(t.smc["TPCD"], Some(45.0));
        let s = p.fans().unwrap();
        assert_eq!(s.tach, [Some(1000.0); 4]);
        assert!(s.overwritten.is_empty());
        // a command is written at once, not at the next period
        let t0 = Instant::now();
        p.command([None, Some(1300.0), None, None]);
        assert!(until(&|| f.holds(2, 1300.0)));
        assert!(t0.elapsed() < Duration::from_millis(PERIOD as u64 * 500));
        // another writer: re-asserted and reported
        put(f.path(3, "output"), "2000");
        assert!(until(&|| p
            .fans()
            .is_some_and(|s| s.overwritten == vec![3])));
        assert!(until(&|| f.holds(3, 1150.0)));
        // freeze: nothing is written after it returns
        assert!(p.freeze(Duration::from_secs(2)));
        p.command([Some(700.0), None, None, None]);
        thread::sleep(Duration::from_millis(300));
        assert!(f.holds(1, 600.0), "a frozen thread writes nothing");
        assert!(p.error().is_none());
        drop(p);
        fs::remove_dir_all(root).unwrap();
    }

    /// The SMC reflects a fan write in its registers with a delay: a setting read back at once can still show the
    /// previous value. A fan that ramps every loop must not look changed by another writer.
    #[test]
    fn lagging_register_read_back_is_not_another_writer() {
        let root = crate::os::tests::temp();
        let f = fans(&root);
        put(root.join("cpu"), "40000");
        let spec = PlatformSpec {
            cpu: root.join("cpu"),
            smc: BTreeMap::new(),
            fans: f.clone(),
        };
        let start = [600.0, 1150.0, 1150.0, 500.0];
        for (i, r) in start.iter().enumerate() {
            f.put(i + 1, *r).unwrap();
        }
        let p = Platform::start(spec, Some(start), Stall::default()).unwrap();
        thread::sleep(Duration::from_millis(300));
        let mut seen = vec![];
        for (step, rpm) in [1300.0, 1450.0, 1600.0].into_iter().enumerate() {
            p.command([None, Some(rpm), None, None]);
            let end = Instant::now() + Duration::from_secs(2);
            while !f.holds(2, rpm) && Instant::now() < end {
                thread::sleep(Duration::from_millis(1));
            }
            // the register still shows the previous value, and a command for another fan forces a round at once
            let old = if step == 0 { 1150.0 } else { rpm - 150.0 };
            put(f.path(2, "output"), &format!("{old}"));
            p.command([Some(610.0 + step as f64 * 10.0), None, None, None]);
            let end = Instant::now() + Duration::from_millis(700);
            while Instant::now() < end {
                if let Some(s) = p.fans() {
                    seen.extend(s.overwritten);
                }
                thread::sleep(Duration::from_millis(10));
            }
            put(f.path(2, "output"), &format!("{rpm}"));
            let end = Instant::now() + Duration::from_millis(2500);
            while Instant::now() < end {
                if let Some(s) = p.fans() {
                    seen.extend(s.overwritten);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            seen.is_empty(),
            "a lagging read-back counted as another writer: {seen:?}"
        );
        drop(p);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn platform_write_failure_is_reported() {
        let root = crate::os::tests::temp();
        let f = fans(&root);
        put(root.join("cpu"), "40000");
        let spec = PlatformSpec {
            cpu: root.join("cpu"),
            smc: BTreeMap::new(),
            fans: f.clone(),
        };
        let start = [600.0, 1150.0, 1150.0, 500.0];
        for (i, r) in start.iter().enumerate() {
            f.put(i + 1, *r).unwrap();
        }
        let p = Platform::start(spec, Some(start), Stall::default()).unwrap();
        fs::remove_file(f.path(4, "manual")).unwrap();
        p.command([None, None, None, Some(900.0)]);
        let end = Instant::now() + Duration::from_secs(5);
        while p.error().is_none() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        // the check of fan 4 (re-assert) or the new setting (write) fails first; either ends the session
        assert!(
            p.error().is_some_and(|e| e.contains("fan4")),
            "{:?}",
            p.error()
        );
        drop(p);
        fs::remove_dir_all(root).unwrap();
    }

    /// A temperature read that blocks leaves the temperatures stale and nothing else: the fan thread keeps its
    /// rounds and still writes a command at once (the alarm's caps reach the fans).
    #[test]
    fn blocked_temperature_read_leaves_the_fans_working() {
        let root = crate::os::tests::temp();
        let f = fans(&root);
        fifo(&root.join("cpu"));
        let spec = PlatformSpec {
            cpu: root.join("cpu"),
            smc: BTreeMap::new(),
            fans: f.clone(),
        };
        let p =
            Platform::start(spec, Some([600.0, 1150.0, 1150.0, 500.0]), Stall::default()).unwrap();
        thread::sleep(Duration::from_millis(1200));
        let t0 = Instant::now();
        assert!(p.temps().is_none(), "no temperature round completes");
        assert!(
            p.fans().is_some() && p.round_age(Instant::now()) < PERIOD + 0.5,
            "the fan thread keeps going"
        );
        p.command([Some(1200.0), Some(2500.0), Some(2500.0), Some(2500.0)]);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "commands never wait on a read"
        );
        let end = Instant::now() + Duration::from_secs(5);
        while !f.holds(4, 2500.0) && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            f.holds(1, 1200.0) && f.holds(4, 2500.0),
            "the caps reach the fans"
        );
        assert!(
            p.freeze(Duration::from_millis(200)),
            "no write is in flight"
        );
        let _ = fs::OpenOptions::new()
            .write(true)
            .custom_flags_nonblock()
            .open(root.join("cpu"));
        drop(p);
        fs::remove_dir_all(root).unwrap();
    }

    /// A stalled fan thread ages its rounds (the control loop withholds the watchdog keepalive after IO_STALL);
    /// a stalled temperature thread does not.
    #[test]
    fn stall_hooks_park_one_thread_each() {
        let root = crate::os::tests::temp();
        let f = fans(&root);
        put(root.join("cpu"), "40000");
        let spec = PlatformSpec {
            cpu: root.join("cpu"),
            smc: BTreeMap::new(),
            fans: f.clone(),
        };
        let stall = Stall {
            fans: Some(Duration::from_millis(100)),
            temps: None,
        };
        let p = Platform::start(spec.clone(), Some([600.0, 1150.0, 1150.0, 500.0]), stall).unwrap();
        thread::sleep(Duration::from_millis(2600));
        assert!(
            p.round_age(Instant::now()) >= 2.0,
            "parked fan thread: rounds age"
        );
        assert!(p
            .temps()
            .is_some_and(|t| t.taken.elapsed() < Duration::from_millis(2100)));
        drop(p);
        let stall = Stall {
            fans: None,
            temps: Some(Duration::from_millis(100)),
        };
        let p = Platform::start(spec, Some([600.0, 1150.0, 1150.0, 500.0]), stall).unwrap();
        thread::sleep(Duration::from_millis(2600));
        assert!(
            p.round_age(Instant::now()) < 2.1,
            "the fan thread keeps its rounds"
        );
        assert!(p.temps().is_none_or_old(Duration::from_secs(2)));
        drop(p);
        fs::remove_dir_all(root).unwrap();
    }
    trait OldOrNone {
        fn is_none_or_old(&self, age: Duration) -> bool;
    }
    impl OldOrNone for Option<TempSample> {
        fn is_none_or_old(&self, age: Duration) -> bool {
            self.as_ref().map_or(true, |t| t.taken.elapsed() >= age)
        }
    }

    trait NonBlock {
        fn custom_flags_nonblock(&mut self) -> &mut Self;
    }
    impl NonBlock for fs::OpenOptions {
        fn custom_flags_nonblock(&mut self) -> &mut Self {
            use std::os::unix::fs::OpenOptionsExt;
            self.custom_flags(0o4000)
        }
    }
}
