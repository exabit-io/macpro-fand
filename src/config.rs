use crate::{
    literal::{self, Value},
    CAPS,
};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
};

#[derive(Clone, Debug)]
pub struct Zone {
    pub name: String,
    pub floor: f64,
    pub init: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct SensorSpec {
    pub fan: usize,
    pub target: f64,
    pub emergency: f64,
    pub band: f64,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub zones: [Zone; 4],
    pub bay_target: f64,
    pub hbm_target: f64,
    pub cpu_target: [(f64, f64); 2],
    pub cpu_band: f64,
    pub cpu_target_tau: f64,
    pub band: f64,
    pub budget_limit: f64,
    pub budget_margin: f64,
    pub base: f64,
    pub k_gpu: f64,
    pub k_cpu: f64,
    pub gpu_floor: f64,
    pub gpu_max: f64,
    pub gpu_init: f64,
    pub cpu_floor: f64,
    pub cpu_max: f64,
    pub static_gpu: f64,
    pub static_pl1: Option<f64>,
    pub cpu_keep: (f64, f64),
    pub t_work: (f64, f64),
    pub hbm_work: f64,
    pub backstop: f64,
    pub smc: BTreeMap<String, SensorSpec>,
    pub nvme: SensorSpec,
    pub periph: BTreeMap<String, (f64, f64, f64)>,
    pub device_w: BTreeMap<String, f64>,
    pub device_pci_w: BTreeMap<String, f64>,
    /// Per device class: a floor, rpm, and optionally a fan whose speed the zone fan must not run below.
    pub unmonitored: BTreeMap<String, (f64, Option<usize>)>,
    pub periph_poll: BTreeMap<String, (f64, f64)>,
    pub temp_command: BTreeMap<String, Vec<String>>,
    pub slot_zone: BTreeMap<String, usize>,
    pub disk_zone: usize,
    pub dies: BTreeMap<String, usize>,
    pub mpx_bay: BTreeMap<String, usize>,
}
fn defaults() -> BTreeMap<String, Value> {
    let source = r#"{
      'ZONES': {1: {'name': 'memory side', 'floor': 600, 'init': 600},
                2: {'name': 'MPX bay 1 + PCH', 'floor': 500, 'init': 1150},
                3: {'name': 'MPX bay 2', 'floor': 500, 'init': 1150},
                4: {'name': 'CPU', 'floor': 500, 'init': 500}},
      'BAY_TARGET': 37.8, 'HBM_TARGET': 80, 'CPU_TARGET': ((133,48),(228,57.5)),
      'CPU_BAND': 1, 'CPU_TARGET_TAU': 30, 'BAND': 0.5,
      'BUDGET_LIMIT': 1228, 'BUDGET_MARGIN': 80, 'BASE': 127, 'K_GPU': 1.205, 'K_CPU': 1.156,
      'GPU_CAP': {'floor': 85, 'max': 200, 'init': 125}, 'CPU_PL1': {'floor':80, 'max':413},
      'STATIC_SAFE': {'gpu':125, 'pl1':None}, 'CPU_KEEP': (1.2,20), 'T_WORK': (65,1),
      'HBM_WORK': 80, 'BACKSTOP':1190, 'SMC': {'TPCD':(2,45.7,90,1)}, 'NVME': (1,55,1),
      'DEVICE_W': {'mellanox':25,'nvme':25,'nvme-switched':9,'pcie-switch':10,'pcie-card':25,
                   'disk':25,'ssd':8,'usb-disk':10},
      'DEVICE_PCI_W': {}, 'UNMONITORED': {'mellanox':(1000,4),'pcie-card':(1000,4)},
      'SLOT_ZONE': {'5':3,'6':3,'7':3},
      'PERIPH': {'nvme':(60,1,80),'mlx5':(80,1,105),'drivetemp':(40,1,60),'mellanox':(80,1,105)},
      'PERIPH_POLL': {'nvme':(5,30),'mlx5':(5,30),'drivetemp':(30,120),'mellanox':(15,60)},
      'TEMP_COMMAND': {},
      'DISK_ZONE':4,'DIES':{},'MPX_BAY':{'1':1,'2':1,'3':2,'4':2}
    }"#;
    let mut values: BTreeMap<_, _> = literal::parse(source)
        .expect("built-in config literal")
        .dict()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.string().unwrap().to_string(), v.clone()))
        .collect();
    let Value::Dict(smc) = values.get_mut("SMC").unwrap() else {
        unreachable!()
    };
    for i in 0..9 {
        smc.push((
            format!("TM{i}P").into(),
            Value::Tuple(vec![1i64.into(), 50i64.into(), 85i64.into(), 1i64.into()]),
        ));
    }
    for i in 0..3 {
        smc.push((
            format!("Tp{i}P").into(),
            Value::Tuple(vec![1i64.into(), 55i64.into(), 80i64.into(), 1i64.into()]),
        ));
    }
    values
}
impl Default for Config {
    fn default() -> Self {
        Self::from_values(&defaults()).expect("valid built-in configuration")
    }
}
fn num(v: &Value, lo: f64, hi: f64, label: &str) -> Result<f64, String> {
    v.number()
        .filter(|n| *n >= lo && *n <= hi)
        .ok_or_else(|| format!("{label} must be a finite number in {lo}..{hi}"))
}
fn whole(v: &Value, lo: f64, hi: f64, label: &str) -> Result<f64, String> {
    let n = num(v, lo, hi, label)?;
    if n.fract() == 0.0 {
        Ok(n)
    } else {
        Err(format!("{label} must be whole watts"))
    }
}
fn fan(v: &Value, label: &str) -> Result<usize, String> {
    match v {
        Value::Int(n) if (1..=4).contains(n) => Ok(*n as usize),
        _ => Err(format!("{label} must be fan 1..4")),
    }
}
fn dict<'a>(v: &'a Value, label: &str) -> Result<&'a [(Value, Value)], String> {
    v.dict().ok_or_else(|| format!("{label} must be a dict"))
}
fn exact(v: &Value, keys: &[&str], label: &str) -> Result<(), String> {
    let d = dict(v, label)?;
    if d.len() == keys.len()
        && d.iter()
            .all(|(k, _)| k.string().is_some_and(|k| keys.contains(&k)))
    {
        Ok(())
    } else {
        Err(format!("{label} must have exactly {keys:?}"))
    }
}
fn tuple<'a>(v: &'a Value, n: usize, label: &str) -> Result<&'a [Value], String> {
    v.tuple(n)
        .ok_or_else(|| format!("{label} must be a {n}-tuple"))
}
fn string(v: &Value, label: &str) -> Result<String, String> {
    v.string()
        .map(str::to_string)
        .ok_or_else(|| format!("{label} must be a string"))
}
impl Config {
    pub fn from_values(v: &BTreeMap<String, Value>) -> Result<Self, String> {
        let mut all = defaults();
        for (k, val) in v {
            if !all.contains_key(k) {
                return Err(format!("unknown setting {k}"));
            }
            all.insert(k.clone(), val.clone());
        }
        let val = |k: &str| &all[k];
        let n = |k: &str, lo, hi| num(val(k), lo, hi, k);
        let lim = n("BUDGET_LIMIT", 100.0, 2000.0)?;
        let z = val("ZONES");
        if dict(z, "ZONES")?.len() != 4 {
            return Err("ZONES must define all four fans".into());
        }
        for (k, _) in dict(z, "ZONES")? {
            fan(k, "ZONES key")?;
        }
        let mut zones = Vec::new();
        for (i, cap) in CAPS.iter().enumerate() {
            let d = z
                .get_int((i + 1) as i64)
                .ok_or("ZONES must define fans 1..4")?;
            dict(d, "zone")?;
            let name = string(
                d.get("name").unwrap_or(&Value::String(String::new())),
                "zone name",
            )?;
            let floor = num(
                d.get("floor").unwrap_or(&Value::None),
                500.0,
                *cap,
                "zone floor",
            )?;
            let init = num(
                d.get("init").unwrap_or(&Value::None),
                500.0,
                *cap,
                "zone init",
            )?;
            zones.push(Zone { name, floor, init });
        }
        let g = val("GPU_CAP");
        exact(g, &["floor", "max", "init"], "GPU_CAP")?;
        let gpu_floor = whole(g.get("floor").unwrap(), 50.0, 200.0, "GPU_CAP floor")?;
        let gpu_max = whole(g.get("max").unwrap(), gpu_floor, 200.0, "GPU_CAP max")?;
        let gpu_init = whole(g.get("init").unwrap(), gpu_floor, 200.0, "GPU_CAP init")?;
        let p = val("CPU_PL1");
        exact(p, &["floor", "max"], "CPU_PL1")?;
        let cpu_floor = whole(p.get("floor").unwrap(), 10.0, 413.0, "CPU_PL1 floor")?;
        let cpu_max = whole(p.get("max").unwrap(), cpu_floor, 413.0, "CPU_PL1 max")?;
        let ss = val("STATIC_SAFE");
        exact(ss, &["gpu", "pl1"], "STATIC_SAFE")?;
        let static_gpu = whole(ss.get("gpu").unwrap(), gpu_floor, 200.0, "STATIC_SAFE gpu")?;
        let static_pl1 = match ss.get("pl1").unwrap() {
            Value::None => None,
            x => Some(whole(x, cpu_floor, 413.0, "STATIC_SAFE pl1")?),
        };
        let t = tuple(val("CPU_TARGET"), 2, "CPU_TARGET")?;
        let point = |p: &Value| -> Result<(f64, f64), String> {
            let p = tuple(p, 2, "CPU_TARGET point")?;
            Ok((
                num(&p[0], 0.0, 500.0, "CPU_TARGET W")?,
                num(&p[1], 20.0, 90.0, "CPU_TARGET C")?,
            ))
        };
        let cpu_target = [point(&t[0])?, point(&t[1])?];
        if cpu_target[0].0 >= cpu_target[1].0 {
            return Err("CPU_TARGET watts must rise".into());
        }
        let t = tuple(val("T_WORK"), 2, "T_WORK")?;
        let t_work = (
            num(&t[0], 40.0, 95.0, "T_WORK C")?,
            num(&t[1], 0.1, 5.0, "T_WORK band")?,
        );
        let k = tuple(val("CPU_KEEP"), 2, "CPU_KEEP")?;
        let cpu_keep = (
            num(&k[0], 1.0, 3.0, "CPU_KEEP factor")?,
            num(&k[1], 0.0, 200.0, "CPU_KEEP W")?,
        );
        let mut smc = BTreeMap::new();
        for (key, v) in dict(val("SMC"), "SMC")? {
            let key = string(key, "SMC key")?;
            let t = tuple(v, 4, "SMC entry")?;
            let target = num(&t[1], 0.0, 120.0, "SMC target")?;
            smc.insert(
                key,
                SensorSpec {
                    fan: fan(&t[0], "SMC fan")?,
                    target,
                    emergency: num(&t[2], target, 130.0, "SMC emergency")?,
                    band: num(&t[3], 0.1, 5.0, "SMC band")?,
                },
            );
        }
        let t = tuple(val("NVME"), 3, "NVME")?;
        let nvme = SensorSpec {
            fan: fan(&t[0], "NVME fan")?,
            target: num(&t[1], 0.0, 100.0, "NVME target")?,
            band: num(&t[2], 0.1, 5.0, "NVME band")?,
            emergency: 0.0,
        };
        let mut periph = BTreeMap::new();
        for (k, v) in dict(val("PERIPH"), "PERIPH")? {
            let t = tuple(v, 3, "PERIPH entry")?;
            periph.insert(
                string(k, "PERIPH key")?,
                (
                    num(&t[0], 0.0, 120.0, "PERIPH target")?,
                    num(&t[1], 0.1, 5.0, "PERIPH band")?,
                    num(&t[2], 30.0, 150.0, "PERIPH critical")?,
                ),
            );
        }
        let mut device_w = BTreeMap::new();
        for (k, v) in dict(val("DEVICE_W"), "DEVICE_W")? {
            device_w.insert(string(k, "DEVICE_W key")?, num(v, 0.0, 500.0, "DEVICE_W")?);
        }
        for k in ["mellanox", "nvme", "disk"] {
            if !device_w.contains_key(k) {
                return Err(format!("DEVICE_W missing {k}"));
            }
        }
        let mut device_pci_w = BTreeMap::new();
        for (k, v) in dict(val("DEVICE_PCI_W"), "DEVICE_PCI_W")? {
            let id = string(k, "DEVICE_PCI_W key")?.to_ascii_lowercase();
            let b = id.as_bytes();
            if b.len() != 9
                || b[4] != b':'
                || !id
                    .chars()
                    .filter(|c| *c != ':')
                    .all(|c| c.is_ascii_hexdigit())
            {
                return Err(format!(
                    "DEVICE_PCI_W key {id:?} must be a PCI id 'vvvv:dddd'"
                ));
            }
            device_pci_w.insert(id, num(v, 0.0, 500.0, "DEVICE_PCI_W")?);
        }
        let mut unmonitored = BTreeMap::new();
        for (k, v) in dict(val("UNMONITORED"), "UNMONITORED")? {
            let entry = match v {
                Value::Tuple(t) if t.len() == 2 => (
                    num(&t[0], 0.0, CAPS[1], "UNMONITORED rpm")?,
                    Some(fan(&t[1], "UNMONITORED fan")?),
                ),
                _ => (num(v, 0.0, CAPS[1], "UNMONITORED rpm")?, None),
            };
            unmonitored.insert(string(k, "UNMONITORED key")?, entry);
        }
        let mut periph_poll = BTreeMap::new();
        for (k, v) in dict(val("PERIPH_POLL"), "PERIPH_POLL")? {
            let t = tuple(v, 2, "PERIPH_POLL entry")?;
            let period = num(&t[0], 1.0, 300.0, "PERIPH_POLL period")?;
            let stale = num(
                &t[1],
                2.0 * period,
                3600.0,
                "PERIPH_POLL stale (at least two periods)",
            )?;
            periph_poll.insert(string(k, "PERIPH_POLL key")?, (period, stale));
        }
        let mut temp_command = BTreeMap::new();
        for (k, v) in dict(val("TEMP_COMMAND"), "TEMP_COMMAND")? {
            let items = match v {
                Value::Tuple(i) | Value::List(i) if !i.is_empty() && i.len() <= 16 => i,
                _ => {
                    return Err("TEMP_COMMAND entry must be a non-empty sequence of strings".into())
                }
            };
            let argv = items
                .iter()
                .map(|a| string(a, "TEMP_COMMAND argument"))
                .collect::<Result<Vec<_>, _>>()?;
            if !argv[0].starts_with('/') {
                return Err("TEMP_COMMAND program must be an absolute path".into());
            }
            temp_command.insert(string(k, "TEMP_COMMAND key")?, argv);
        }
        let mut slot_zone = BTreeMap::new();
        for (k, v) in dict(val("SLOT_ZONE"), "SLOT_ZONE")? {
            slot_zone.insert(string(k, "SLOT_ZONE key")?, fan(v, "SLOT_ZONE fan")?);
        }
        let bays = |name| -> Result<BTreeMap<String, usize>, String> {
            let mut out = BTreeMap::new();
            for (k, v) in dict(val(name), name)? {
                let b = num(v, 1.0, 2.0, name)?;
                if b.fract() != 0.0 {
                    return Err(format!("{name} values must be bay 1 or 2"));
                }
                out.insert(string(k, name)?, b as usize);
            }
            Ok(out)
        };
        Ok(Self {
            zones: zones.try_into().unwrap(),
            gpu_floor,
            gpu_max,
            gpu_init,
            cpu_floor,
            cpu_max,
            static_gpu,
            static_pl1,
            cpu_target,
            cpu_keep,
            t_work,
            smc,
            nvme,
            periph,
            device_w,
            device_pci_w,
            unmonitored,
            periph_poll,
            temp_command,
            slot_zone,
            dies: bays("DIES")?,
            mpx_bay: bays("MPX_BAY")?,
            disk_zone: fan(val("DISK_ZONE"), "DISK_ZONE")?,
            budget_limit: lim,
            budget_margin: n("BUDGET_MARGIN", 0.0, lim / 2.0)?,
            base: n("BASE", 0.0, lim)?,
            k_gpu: n("K_GPU", 0.5, 2.0)?,
            k_cpu: n("K_CPU", 0.5, 2.0)?,
            bay_target: n("BAY_TARGET", 20.0, 90.0)?,
            hbm_target: n("HBM_TARGET", 20.0, 95.0)?,
            cpu_band: n("CPU_BAND", 0.1, 5.0)?,
            cpu_target_tau: n("CPU_TARGET_TAU", 1.0, 600.0)?,
            band: n("BAND", 0.1, 5.0)?,
            hbm_work: n("HBM_WORK", 40.0, 95.0)?,
            backstop: n("BACKSTOP", 100.0, lim)?,
        })
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        // Nonblocking open plus a type check rejects a FIFO/device config before
        // it can prevent ExecStopPost from reaching its built-in recovery.
        let mut f = match OpenOptions::new()
            .read(true)
            .custom_flags(0o4000)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if !f.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("configuration must be a regular file".into());
        }
        let mut data = Vec::new();
        f.by_ref()
            .take(131_073)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() > 131_072 {
            return Err("configuration exceeds 128 KiB".into());
        }
        let text =
            std::str::from_utf8(&data).map_err(|_| "configuration is not UTF-8".to_string())?;
        Self::from_text(text)
    }
    pub fn from_text(text: &str) -> Result<Self, String> {
        if text.len() > 131_072 {
            return Err("configuration exceeds 128 KiB".into());
        }
        let mut values = defaults();
        let mut items: Vec<(usize, String)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let content = line.split('#').next().unwrap_or("").trim_end();
            if content.trim().is_empty() {
                continue;
            }
            if content.starts_with([' ', '\t']) && !items.is_empty() {
                let last = &mut items.last_mut().unwrap().1;
                last.push(' ');
                last.push_str(content.trim());
            } else {
                items.push((i + 1, content.trim().to_string()));
            }
        }
        for (line, item) in items {
            let (name, raw) = item
                .split_once('=')
                .ok_or_else(|| format!("line {line}: expected NAME = literal"))?;
            let name = name.trim();
            let old = values
                .get_mut(name)
                .ok_or_else(|| format!("line {line}: unknown setting {name}"))?;
            let new = literal::parse(raw.trim()).map_err(|e| format!("line {line}: {e}"))?;
            if matches!(old, Value::Dict(_)) && !matches!(new, Value::Dict(_)) {
                return Err(format!("line {line}: {name} must be a dict"));
            }
            old.merge(new);
        }
        Self::from_values(&values)
    }
    /// The measured base plus every peripheral's allowance.
    pub fn fixed_base(&self, devices: &[crate::inventory::Device]) -> f64 {
        self.base + devices.iter().map(|d| d.watts).sum::<f64>()
    }
    /// Poll period and staleness limit (s) for a peripheral sensor class.
    pub fn poll(&self, class: &str) -> (f64, f64) {
        self.periph_poll.get(class).copied().unwrap_or((5.0, 30.0))
    }
    pub fn check_feasible(
        &self,
        discovered_dies: usize,
        devices: &[crate::inventory::Device],
    ) -> Result<(), String> {
        let n = if self.dies.is_empty() {
            discovered_dies
        } else {
            self.dies.len()
        };
        let cost = self.fixed_base(devices)
            + self.k_gpu * n as f64 * self.gpu_floor
            + self.k_cpu * self.cpu_floor;
        if cost <= self.budget_limit - self.budget_margin {
            Ok(())
        } else {
            Err(format!(
                "budget infeasible: floors cost {cost:.1} W > {:.1} W",
                self.budget_limit - self.budget_margin
            ))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum FanMode {
    #[default]
    Auto,
    Max,
    Hold([f64; 4]),
}
#[derive(Clone, Debug, Default)]
pub struct Override {
    pub mode: FanMode,
    pub pin: Option<(Option<f64>, Option<f64>)>,
    pub note: String,
}
impl Override {
    pub fn parse(text: &str, age: f64, c: &Config) -> Self {
        let mut words = Vec::new();
        let mut ttl = None;
        let mut gpu = None;
        let mut cpu = None;
        let finite = |s: &str| s.parse::<f64>().ok().filter(|x| x.is_finite());
        for word in text.split_whitespace() {
            if let Some(v) = word.strip_prefix("ttl=") {
                if ttl.is_none() {
                    ttl = Some(finite(v));
                }
            } else if let Some(v) = word.strip_prefix("gpu=") {
                if gpu.is_none() {
                    gpu = Some(finite(v).map(|v| v.clamp(c.gpu_floor, c.gpu_max)));
                }
            } else if let Some(v) = word.strip_prefix("pl1=") {
                if cpu.is_none() {
                    cpu = Some(finite(v).map(|v| v.clamp(10.0, c.cpu_max)));
                }
            } else {
                words.push(word);
            }
        }
        let bad = || Self {
            note: format!(
                "override ignored (malformed: {})",
                words.join(" ").chars().take(60).collect::<String>()
            ),
            ..Self::default()
        };
        if let Some(t) = ttl {
            let Some(t) = t else { return bad() };
            if age > t {
                return Self {
                    note: "override expired".into(),
                    ..Self::default()
                };
            }
        }
        let gp = gpu.flatten();
        let pp = cpu.flatten();
        let pin = if gp.is_some() || pp.is_some() {
            Some((gp, pp))
        } else {
            None
        };
        let mut suffix = String::new();
        if let Some(g) = gp {
            suffix.push_str(&format!(" gpu={g:.0}"));
        }
        if let Some(p) = pp {
            suffix.push_str(&format!(" pl1={p:.0}"));
        }
        if words.is_empty() || words[0] == "auto" {
            return Self {
                mode: FanMode::Auto,
                pin,
                note: if pin.is_some() {
                    format!("override{suffix}")
                } else {
                    String::new()
                },
            };
        }
        if words == ["max"] {
            return Self {
                mode: FanMode::Max,
                pin,
                note: format!("override max{suffix}"),
            };
        }
        if words.len() == 5 && words[0] == "hold" {
            let mut rpm = [0.0; 4];
            for i in 0..4 {
                let Some(v) = finite(words[i + 1]) else {
                    return bad();
                };
                rpm[i] = v.clamp(500.0, CAPS[i]);
            }
            return Self {
                mode: FanMode::Hold(rpm),
                pin,
                note: format!("override hold{suffix}"),
            };
        }
        bad()
    }
    pub fn load(path: &Path, c: &Config) -> Self {
        let read = || -> std::io::Result<(String, f64)> {
            let f = OpenOptions::new()
                .read(true)
                .custom_flags(0o4000 | 0o400000)
                .open(path)?;
            if !f.metadata()?.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "override must be a regular file",
                ));
            }
            let stamp = f.metadata()?.modified()?;
            let mut text = String::new();
            f.take(8193).read_to_string(&mut text)?;
            if text.len() > 8192 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "override too large",
                ));
            }
            let age = match std::time::SystemTime::now().duration_since(stamp) {
                Ok(t) => t.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            Ok((text, age))
        };
        match read() {
            Ok((text, age)) => Self::parse(&text, age, c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(_) => Self {
                note: "override ignored (unreadable or not text)".into(),
                ..Self::default()
            },
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_merges() {
        let c = Config::from_text("GPU_CAP = {'max': 125}\nZONES = {4: {'floor': 700}}\n").unwrap();
        assert_eq!(c.gpu_max, 125.0);
        assert_eq!(c.zones[3].floor, 700.0);
        assert_eq!(c.zones[3].init, 500.0);
        assert_eq!(c.smc.len(), 13);
        assert_eq!(c.device_w["disk"], 25.0);
        assert!(Config::default().check_feasible(4, &[]).is_ok());
    }
    #[test]
    fn rejects_unsafe_shapes_without_panicking() {
        for text in [
            "GPU_CAP = {'floor':126}",
            "GPU_CAP = {'max':100.5}",
            "CPU_PL1 = {'floor':9}",
            "STATIC_SAFE = {'pl1':85.5}",
            "PERIPH = {1:(50,1,80)}",
            "ZONES = {True:{'floor':500}}",
            "K_CPU = True",
            "BASE = 1e999",
            "SMC = {'x': []}",
            "DIES = {'x':True}",
            "CPU_TARGET = [(133,48),(228,57.5)]",
            "BUDGET_LIMIT = 100",
            "ZONES = []",
            "NVME = ([],55,1)",
            "BAND = (1,2)",
            "BOGUS = 10",
        ] {
            assert!(Config::from_text(text).is_err(), "accepted {text}");
        }
        assert!(Config::default().check_feasible(50, &[]).is_err());
    }
    #[test]
    fn override_nan_ttl_and_pins() {
        let c = Config::default();
        assert_eq!(
            Override::parse("hold 500 1000 1100 1200 gpu=150 pl1=20", 1.0, &c).pin,
            Some((Some(150.0), Some(20.0)))
        );
        for text in ["gpu=NaN", "pl1=inf", "hold NaN 500 500 500", "max ttl=NaN"] {
            assert!(Override::parse(text, 1.0, &c).pin.is_none());
        }
        assert_eq!(
            Override::parse("max gpu=150 ttl=10", 11.0, &c).mode,
            FanMode::Auto
        );
        assert_eq!(
            Override::parse("max gpu=1 pl1=0", 0.0, &c).pin,
            Some((Some(85.0), Some(10.0)))
        );
        assert_eq!(
            Override::parse("hold 0 9999 500 500", 0.0, &c).mode,
            FanMode::Hold([500.0, 2500.0, 500.0, 500.0])
        );
    }
    #[test]
    fn binary_oversized_and_symlink_override_are_ignored() {
        let root = crate::os::tests::temp();
        let path = root.join("override");
        let c = Config::default();
        std::fs::write(&path, [0xff, 0]).unwrap();
        assert_eq!(Override::load(&path, &c).mode, FanMode::Auto);
        assert!(Config::load(&path).is_err());
        std::fs::write(&path, " ".repeat(131_073)).unwrap();
        assert!(Config::load(&path).is_err());
        assert_eq!(Override::load(&path, &c).mode, FanMode::Auto);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(root.join("target"), "max gpu=200").unwrap();
        std::os::unix::fs::symlink(root.join("target"), &path).unwrap();
        assert_eq!(Override::load(&path, &c).mode, FanMode::Auto);
        std::fs::remove_dir_all(root).unwrap();
    }
}
