//! Native implementation of the macpro-fand 1.0.6 control policy.
pub mod budget;
pub mod config;
pub mod control;
pub mod hardware;
pub mod inventory;
pub mod literal;
pub mod os;
pub mod power;
pub mod runtime;
pub mod sampling;
pub mod transaction;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PL1: &str = "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw";
pub const PL2: &str = "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw";
pub const RAPL: &str = "/sys/class/powercap/intel-rapl:0";
pub const CAPS: [f64; 4] = [1200.0, 2500.0, 2500.0, 2500.0];
pub const PERIOD: f64 = 2.0;
pub const GPU_STALE: f64 = 10.0;
pub const LIMIT_MIN_UW: f64 = 10_000_000.0;

pub fn log(fields: &[(&str, literal::Value)]) {
    use std::io::Write;
    let value = literal::Value::object(fields.iter().map(|(k, v)| ((*k).into(), v.clone())));
    let mut out = std::io::stdout().lock();
    // A closed journal/stdout must not prevent the safety cleanup.
    let _ = writeln!(out, "{}", value.json());
    let _ = out.flush();
}
