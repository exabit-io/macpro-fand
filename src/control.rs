use crate::{config::Config, CAPS};
pub fn ema(old: Option<f64>, new: f64, dt: f64, tau: f64) -> f64 {
    old.map(|v| v + (new - v) * dt / (tau + dt)).unwrap_or(new)
}
pub fn cpu_target(w: Option<f64>, c: &Config) -> f64 {
    let [(w0, t0), (w1, t1)] = c.cpu_target;
    match w {
        None => t0,
        Some(w) if w <= w0 => t0,
        Some(w) if w >= w1 => t1,
        Some(w) => t0 + (t1 - t0) * (w - w0) / (w1 - w0),
    }
}
pub fn energy_rate(
    prev: Option<(f64, f64)>,
    now: f64,
    energy: Option<f64>,
    range: Option<f64>,
) -> Option<f64> {
    let (t, e) = prev?;
    let energy = energy?;
    if now <= t {
        return None;
    }
    let mut delta = energy - e;
    if delta < 0.0 {
        delta += range?;
    }
    if delta >= 0.0 {
        Some(delta / 1e6 / (now - t))
    } else {
        None
    }
}
/// Python round(), needed for the v1 fan write thresholds at half-way values.
pub fn round_even(v: f64) -> f64 {
    let floor = v.floor();
    if v - floor < 0.5 {
        floor
    } else if v - floor > 0.5 || floor % 2.0 != 0.0 {
        floor + 1.0
    } else {
        floor
    }
}
#[derive(Clone, Debug)]
pub struct Filter {
    pub tau: f64,
    pub raw: Option<f64>,
    pub val: Option<f64>,
    pub fails: usize,
}
impl Filter {
    pub fn new(tau: f64) -> Self {
        Self {
            tau,
            raw: None,
            val: None,
            fails: 0,
        }
    }
    pub fn feed(&mut self, v: Option<f64>, dt: f64) -> bool {
        if let Some(v) = v.filter(|n| n.is_finite()) {
            self.fails = 0;
            self.raw = Some(v);
            self.val = Some(ema(self.val, v, dt, self.tau));
            true
        } else {
            self.fails = self.fails.saturating_add(1);
            self.fails < 3
        }
    }
    pub fn healthy(&self) -> bool {
        self.fails < 3
    }
}
#[derive(Clone, Debug)]
pub struct Governor {
    pub rpm: f64,
    pub err: Option<f64>,
    pub rate: f64,
    floor: f64,
    cap: f64,
}
impl Governor {
    pub fn new(fan: usize, c: &Config) -> Self {
        let z = &c.zones[fan - 1];
        Self {
            rpm: z.init,
            err: None,
            rate: 0.0,
            floor: z.floor,
            cap: CAPS[fan - 1],
        }
    }
    pub fn step(&mut self, errs: &[(f64, f64)], dt: f64) -> f64 {
        self.err = errs.iter().map(|(e, _)| *e).reduce(f64::max);
        self.rate = if errs.is_empty() {
            0.0
        } else if errs.iter().any(|(e, b)| e > b) {
            errs.iter()
                .filter(|(e, b)| e > b)
                .map(|(e, _)| *e)
                .fold(f64::NEG_INFINITY, f64::max)
                .min(150.0)
        } else if errs.iter().all(|(e, b)| *e < -*b) {
            -errs
                .iter()
                .map(|(e, _)| -*e)
                .fold(f64::INFINITY, f64::min)
                .mul_add(0.5, 0.0)
                .clamp(0.25, 2.0)
        } else {
            0.0
        };
        self.rpm = (self.rpm + self.rate * dt).clamp(self.floor, self.cap);
        self.rpm
    }
}
#[derive(Clone, Debug, Default)]
pub struct FanHealth {
    pub failed: bool,
    pub reference: Option<f64>,
    pub reference_t: f64,
    pub bad_since: Option<f64>,
    pub good_since: Option<f64>,
}
impl FanHealth {
    pub fn sample(&mut self, written: f64, tach: Option<f64>, now: f64) -> bool {
        if self.reference.none_or(|r| (written - r).abs() > 100.0) {
            self.reference = Some(written);
            self.reference_t = now;
        }
        let off = tach.none_or(|t| (t - written).abs() > 150.0f64.max(0.25 * written));
        let stopped = tach.none_or(|t| t < 250.0);
        if off && (self.failed || stopped || now - self.reference_t > 20.0) {
            let t = *self.bad_since.get_or_insert(now);
            self.good_since = None;
            if now - t >= 30.0 {
                self.failed = true;
            }
        } else if !off {
            self.bad_since = None;
            let t = *self.good_since.get_or_insert(now);
            if now - t >= 30.0 {
                self.failed = false;
            }
        }
        self.failed
    }
}
// Option::is_none_or was stabilized after our minimum Rust version.
pub trait OptionNoneOr<T> {
    fn none_or(self, predicate: impl FnOnce(T) -> bool) -> bool;
}
impl<T> OptionNoneOr<T> for Option<T> {
    fn none_or(self, p: impl FnOnce(T) -> bool) -> bool {
        self.map(p).unwrap_or(true)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_use_real_gap_and_failure_latches() {
        let mut f = Filter::new(4.0);
        f.feed(Some(0.0), 2.0);
        f.feed(Some(20.0), 8.0);
        assert!((f.val.unwrap() - 13.333333333).abs() < 1e-8);
        assert!(f.feed(None, 2.0));
        assert!(f.feed(Some(f64::NAN), 2.0));
        assert!(!f.feed(None, 2.0));
        assert!(!f.healthy());
        assert!(!f.feed(None, 2.0));
        assert!(f.feed(Some(10.0), 2.0));
        assert!(f.healthy());
    }
    #[test]
    fn governors_hysteresis_and_caps() {
        let c = Config::default();
        let mut g = Governor::new(2, &c);
        assert_eq!(g.step(&[(0.5, 0.5)], 2.0), 1150.0);
        assert_eq!(g.step(&[(2.0, 0.5), (-5.0, 1.0)], 2.0), 1154.0);
        assert_eq!(g.step(&[(-10.0, 1.0), (-2.0, 0.5)], 2.0), 1152.0);
        assert_eq!(g.step(&[(1000.0, 1.0)], 20.0), 2500.0);
        assert_eq!(g.step(&[], 2.0), 2500.0);
    }
    #[test]
    fn cpu_target_energy_and_rounding() {
        let c = Config::default();
        assert_eq!(cpu_target(None, &c), 48.0);
        assert_eq!(cpu_target(Some(228.0), &c), 57.5);
        assert_eq!(cpu_target(Some(180.5), &c), 52.75);
        assert_eq!(
            energy_rate(Some((0.0, 90e6)), 2.0, Some(10e6), Some(100e6)),
            Some(10.0)
        );
        assert_eq!(energy_rate(Some((0.0, 90e6)), 2.0, Some(10e6), None), None);
        assert_eq!(round_even(500.5), 500.0);
        assert_eq!(round_even(501.5), 502.0);
    }
    #[test]
    fn fans_latch_recover_and_settle() {
        let mut f = FanHealth::default();
        assert!(!f.sample(1150.0, Some(0.0), 0.0));
        assert!(f.sample(1150.0, Some(0.0), 31.0));
        assert!(f.sample(1150.0, Some(1150.0), 32.0));
        assert!(!f.sample(1150.0, Some(1150.0), 63.0));
        assert!(!f.sample(2500.0, Some(1150.0), 64.0));
        assert!(!f.sample(2500.0, Some(1150.0), 85.0));
        assert!(f.sample(2500.0, Some(1150.0), 116.0));
    }
}
