//! Test-only protocol for comparing pure policy with the pinned Python daemon.
//! This executable is never installed in the daemon package.
use macpro_fand::{
    budget::{self, Allocation, Budget, Telemetry},
    config::{Config, Override},
    control::{Filter, Governor},
    literal::{self, Value},
    transaction::{self, PowerIo},
};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
};
fn get<'a>(v: &'a Value, k: &str) -> Result<&'a Value, String> {
    v.get(k).ok_or_else(|| format!("missing {k}"))
}
fn n(v: &Value) -> Result<f64, String> {
    v.number().ok_or("expected number".into())
}
fn b(v: &Value) -> Result<bool, String> {
    if let Value::Bool(b) = v {
        Ok(*b)
    } else {
        Err("expected bool".into())
    }
}
fn s(v: &Value) -> Result<&str, String> {
    v.string().ok_or("expected string".into())
}
fn seq(v: &Value) -> Result<&[Value], String> {
    match v {
        Value::List(v) | Value::Tuple(v) => Ok(v),
        _ => Err("expected sequence".into()),
    }
}
fn opt(v: &Value) -> Result<Option<f64>, String> {
    if *v == Value::None {
        Ok(None)
    } else {
        Ok(Some(n(v)?))
    }
}
fn map(v: &Value) -> Result<Allocation, String> {
    v.dict()
        .ok_or("expected dictionary")?
        .iter()
        .map(|(k, v)| Ok((s(k)?.to_string(), n(v)?)))
        .collect()
}
fn omap(v: &Value) -> Result<BTreeMap<String, Option<f64>>, String> {
    v.dict()
        .ok_or("expected dictionary")?
        .iter()
        .map(|(k, v)| Ok((s(k)?.to_string(), opt(v)?)))
        .collect()
}
fn obj(items: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::object(items)
}
fn amap(v: &Allocation) -> Value {
    obj(v.iter().map(|(k, v)| (k.clone(), (*v).into())))
}
fn snapshot(b: &Budget) -> Value {
    obj([
        ("gpu".into(), amap(&b.gpu)),
        ("pl1".into(), b.pl1.into()),
        ("tcap".into(), amap(&b.tcap)),
        ("cpu_min".into(), b.cpu_min.into()),
        ("cpu_w".into(), b.cpu_w().into()),
        ("cost".into(), b.cost().into()),
        ("feasible".into(), b.feasible().into()),
        ("floor_until".into(), b.floor_until.into()),
        ("reset_pending".into(), b.reset_pending.into()),
        ("want".into(), amap(&b.want())),
    ])
}
struct Fake {
    v: BTreeMap<String, Option<f64>>,
    first: std::collections::BTreeSet<String>,
    fail: Option<String>,
    read_fail: Option<String>,
    round: f64,
    writes: Vec<Value>,
    states: Vec<Value>,
}
impl PowerIo for Fake {
    fn read(&mut self, p: &str) -> Option<f64> {
        if self.first.remove(p) || self.read_fail.as_deref() == Some(p) {
            None
        } else {
            self.v.get(p).copied().flatten()
        }
    }
    fn write(&mut self, p: &str, v: f64) -> bool {
        self.writes.push(Value::array([p.into(), v.into()]));
        if self.fail.as_deref() == Some(p) {
            return false;
        }
        self.v.insert(p.into(), Some(v + self.round));
        self.states
            .push(obj(self.v.iter().map(|(k, v)| (k.clone(), (*v).into()))));
        true
    }
}
fn run(v: &Value) -> Result<Value, String> {
    let config = Config::from_text(v.get("config").map(s).transpose()?.unwrap_or(""))?;
    match s(get(v, "op")?)? {
        "config" => Ok(obj([
            ("gpu_floor".into(), config.gpu_floor.into()),
            ("gpu_max".into(), config.gpu_max.into()),
            ("gpu_init".into(), config.gpu_init.into()),
            ("cpu_floor".into(), config.cpu_floor.into()),
            (
                "limit".into(),
                (config.budget_limit - config.budget_margin).into(),
            ),
        ])),
        "override" => {
            let o = Override::parse(s(get(v, "text")?)?, n(get(v, "age")?)?, &config);
            Ok(obj([
                (
                    "mode".into(),
                    match o.mode {
                        macpro_fand::config::FanMode::Auto => "auto",
                        macpro_fand::config::FanMode::Max => "max",
                        macpro_fand::config::FanMode::Hold(_) => "hold",
                    }
                    .into(),
                ),
                (
                    "hold".into(),
                    if let macpro_fand::config::FanMode::Hold(h) = o.mode {
                        Value::array(h.into_iter().map(Value::from))
                    } else {
                        Value::None
                    },
                ),
                (
                    "pin".into(),
                    o.pin
                        .map(|(g, p)| Value::array([g.into(), p.into()]))
                        .unwrap_or(Value::None),
                ),
            ]))
        }
        "governor" => {
            let mut g = Governor::new(n(get(v, "fan")?)? as usize, &config);
            let mut out = Vec::new();
            for a in seq(get(v, "actions")?)? {
                let errs = seq(get(a, "errs")?)?
                    .iter()
                    .map(|v| {
                        let t = seq(v)?;
                        Ok((n(&t[0])?, n(&t[1])?))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                g.step(&errs, n(get(a, "dt")?)?);
                out.push(obj([
                    ("rpm".into(), g.rpm.into()),
                    ("rate".into(), g.rate.into()),
                    ("err".into(), g.err.into()),
                ]));
            }
            Ok(Value::array(out))
        }
        "filter" => {
            let mut f = Filter::new(n(get(v, "tau")?)?);
            let mut out = Vec::new();
            for a in seq(get(v, "actions")?)? {
                let t = seq(a)?;
                let healthy = f.feed(opt(&t[0])?, n(&t[1])?);
                out.push(Value::array([
                    f.raw.into(),
                    f.val.into(),
                    f.fails.into(),
                    healthy.into(),
                ]));
            }
            Ok(Value::array(out))
        }
        "static" => {
            let ss = budget::static_safe(
                &config,
                n(get(v, "base")?)?,
                n(get(v, "n")?)? as usize,
                n(get(v, "unmanaged")?)? as usize,
                n(get(v, "cpu_min")?)?,
            );
            Ok(obj([
                ("gpu".into(), ss.gpu.into()),
                ("pl1".into(), ss.pl1.into()),
                ("fits".into(), ss.fits.into()),
            ]))
        }
        "transaction" => {
            let values = omap(get(v, "start")?)?;
            let mut f = Fake {
                v: values,
                first: v
                    .get("unread")
                    .map(seq)
                    .transpose()?
                    .unwrap_or(&[])
                    .iter()
                    .map(|v| s(v).map(str::to_string))
                    .collect::<Result<_, _>>()?,
                fail: v.get("fail").map(s).transpose()?.map(str::to_string),
                read_fail: v.get("read_fail").map(s).transpose()?.map(str::to_string),
                round: v.get("round").map(n).transpose()?.unwrap_or(0.0),
                writes: vec![],
                states: vec![],
            };
            let t = transaction::transact(
                &map(get(v, "want")?)?,
                &mut f,
                v.get("raise_ok").map(b).transpose()?.unwrap_or(true),
            );
            Ok(obj([
                ("ok".into(), t.ok.into()),
                (
                    "failed".into(),
                    t.failed.map(Value::from).unwrap_or(Value::None),
                ),
                (
                    "seen".into(),
                    obj(t.seen.iter().map(|(p, v)| (p.clone(), (*v).into()))),
                ),
                ("writes".into(), Value::List(f.writes)),
                ("states".into(), Value::List(f.states)),
            ]))
        }
        "budget" => {
            let count = n(get(v, "n")?)? as usize;
            let paths = (0..count)
                .map(|i| (format!("g{i}"), format!("/gpu/{i}")))
                .collect();
            let mut budget = Budget::new(
                config,
                paths,
                v.get("unmanaged").map(n).transpose()?.unwrap_or(0.0) as usize,
                v.get("base").map(n).transpose()?.unwrap_or(127.0),
            );
            if let Some(l) = v.get("limit") {
                budget.limit = n(l)?;
                budget.fit(None);
            }
            let mut out = vec![snapshot(&budget)];
            for a in seq(get(v, "actions")?)? {
                match s(get(a, "kind")?)? {
                    "charge" => {
                        budget.cpu_charge(opt(get(a, "pl2")?)?, b(get(a, "enforcing")?)?);
                    }
                    "held" => budget.pl2_held = b(get(a, "value")?)?,
                    "degraded" => budget.degraded = b(get(a, "value")?)?,
                    "pin" => {
                        budget.pin = match get(a, "value")? {
                            Value::None => None,
                            v => {
                                let t = seq(v)?;
                                Some((opt(&t[0])?, opt(&t[1])?))
                            }
                        }
                    }
                    "rebalance" => {
                        budget.rebalance(
                            n(get(a, "now")?)?,
                            &omap(get(a, "watts")?)?,
                            opt(get(a, "cpu")?)?,
                        );
                    }
                    "backstop" => {
                        budget.backstop_check(opt(get(a, "dc")?)?, n(get(a, "now")?)?);
                    }
                    "end_pin" => budget.end_pin(n(get(a, "now")?)?),
                    "reset" => budget.reset(),
                    "thermal" => {
                        let mut dies: Telemetry = BTreeMap::new();
                        for (k, v) in get(a, "dies")?.dict().ok_or("dies dict")? {
                            let t = seq(v)?;
                            dies.insert(
                                s(k)?.into(),
                                (opt(&t[0])?, opt(&t[1])?, opt(&t[2])?, n(&t[3])? as usize),
                            );
                        }
                        let full = get(a, "full")?
                            .dict()
                            .ok_or("full dict")?
                            .iter()
                            .map(|(k, v)| Ok((n(k)? as usize, b(v)?)))
                            .collect::<Result<_, String>>()?;
                        budget.thermal(&dies, &full, n(get(a, "dt")?)?);
                    }
                    _ => return Err("unknown budget action".into()),
                }
                out.push(snapshot(&budget));
            }
            Ok(Value::List(out))
        }
        _ => Err("unknown operation".into()),
    }
}
fn main() {
    for line in io::stdin().lock().lines() {
        let answer = line
            .map_err(|e| e.to_string())
            .and_then(|l| literal::parse(&l))
            .and_then(|v| run(&v));
        println!(
            "{}",
            match answer {
                Ok(v) => v,
                Err(e) => obj([("error".into(), e.into())]),
            }
            .json()
        );
    }
}
