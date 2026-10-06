//! Pure power planning. All hardware writes belong to the transaction worker.
use crate::{config::Config, PL1, PL2};
use std::collections::BTreeMap;
pub type Allocation = BTreeMap<String, f64>;
pub type Telemetry = BTreeMap<String, (Option<f64>, Option<f64>, Option<f64>, usize)>;

pub fn level(
    g: &mut Allocation,
    dies: &[String],
    amount: f64,
    bound: &Allocation,
    down: bool,
) -> f64 {
    if dies.is_empty() || amount <= 0.0 {
        return 0.0;
    }
    let moved = |l: f64| -> f64 {
        dies.iter()
            .map(|a| {
                if down {
                    (g[a] - l.max(bound[a])).max(0.0)
                } else {
                    (l.min(bound[a]) - g[a]).max(0.0)
                }
            })
            .sum()
    };
    let lo = dies
        .iter()
        .map(|a| if down { bound[a] } else { g[a] })
        .fold(f64::INFINITY, f64::min);
    let hi = dies
        .iter()
        .map(|a| if down { g[a] } else { bound[a] })
        .fold(f64::NEG_INFINITY, f64::max);
    let mut l = if down { lo } else { hi };
    if moved(l) > amount {
        let (mut a, mut b) = (lo, hi);
        for _ in 0..60 {
            let mid = (a + b) / 2.0;
            if (moved(mid) > amount) == down {
                a = mid;
            } else {
                b = mid;
            }
        }
        l = if down { b } else { a };
    }
    let total = moved(l);
    for a in dies {
        let v = if down {
            g[a].min(l.max(bound[a]))
        } else {
            g[a].max(l.min(bound[a]))
        };
        g.insert(a.clone(), v);
    }
    total
}

#[derive(Clone, Debug)]
pub struct Budget {
    pub config: Config,
    pub paths: BTreeMap<String, String>,
    pub n_unmanaged: usize,
    pub fixed_base: f64,
    pub limit: f64,
    pub backstop: f64,
    pub gpu: Allocation,
    pub pl1: f64,
    pub pin: Option<(Option<f64>, Option<f64>)>,
    pub reset_pending: bool,
    pub tcap: Allocation,
    pub cpu_min: f64,
    pub pl2_held: bool,
    pub degraded: bool,
    pub over_n: usize,
    pub floor_until: f64,
    pub dc: Option<f64>,
}
impl Budget {
    pub fn new(
        config: Config,
        paths: BTreeMap<String, String>,
        n_unmanaged: usize,
        fixed_base: f64,
    ) -> Self {
        let gpu = paths
            .keys()
            .map(|a| (a.clone(), config.gpu_init.min(config.gpu_max)))
            .collect();
        let tcap = paths
            .keys()
            .map(|a| (a.clone(), config.gpu_max.min(config.gpu_init + 10.0)))
            .collect();
        let mut b = Self {
            gpu,
            tcap,
            paths,
            n_unmanaged,
            fixed_base,
            limit: config.budget_limit - config.budget_margin,
            backstop: config.backstop,
            pl1: config.cpu_max,
            config,
            pin: None,
            reset_pending: false,
            cpu_min: 0.0,
            pl2_held: true,
            degraded: false,
            over_n: 0,
            floor_until: 0.0,
            dc: None,
        };
        b.fit(None);
        b
    }
    pub fn fixed(&self) -> f64 {
        self.fixed_base + self.config.k_gpu * 200.0 * self.n_unmanaged as f64
    }
    pub fn cost_with(&self, gpu: &Allocation, pl1: f64) -> f64 {
        self.fixed()
            + self.config.k_gpu * gpu.values().sum::<f64>()
            + self.config.k_cpu * pl1.max(self.cpu_min)
    }
    pub fn cost(&self) -> f64 {
        self.cost_with(&self.gpu, self.pl1)
    }
    pub fn cpu_w(&self) -> f64 {
        self.pl1.max(self.cpu_min)
    }
    pub fn room(&self, gpu: &Allocation) -> f64 {
        self.config.cpu_max.min(
            (self.limit - self.fixed() - self.config.k_gpu * gpu.values().sum::<f64>())
                / self.config.k_cpu,
        )
    }
    pub fn feasible(&self) -> bool {
        self.cost() <= self.limit + 1e-6
    }
    pub fn fit(&mut self, floor: Option<f64>) -> bool {
        self.pl1 = floor
            .unwrap_or(self.config.cpu_floor)
            .max(self.pl1.min(self.room(&self.gpu)));
        while !self.feasible() {
            let mut any = false;
            for v in self.gpu.values_mut() {
                if *v > self.config.gpu_floor {
                    *v = self.config.gpu_floor.max(*v - 1.0);
                    any = true;
                }
            }
            if !any {
                return false;
            }
        }
        true
    }
    pub fn reset(&mut self) {
        for v in self.gpu.values_mut() {
            *v = self.config.gpu_init.min(self.config.gpu_max);
        }
        self.pl1 = self.config.cpu_max;
        self.fit(None);
    }
    pub fn cpu_charge(&mut self, pl2_uw: Option<f64>, enforcing: bool) -> bool {
        let low = if !enforcing || (!self.pl2_held && pl2_uw.is_none()) {
            413.0
        } else if !self.pl2_held {
            413.0f64.min(pl2_uw.unwrap() / 1e6)
        } else {
            0.0
        };
        if (low - self.cpu_min).abs() < 0.5 {
            return false;
        }
        self.cpu_min = low;
        self.fit(None);
        true
    }
    pub fn want(&self) -> Allocation {
        let mut w: Allocation = self
            .paths
            .iter()
            .map(|(a, p)| (p.clone(), self.gpu[a].floor() * 1e6))
            .collect();
        w.insert(PL1.into(), self.pl1.floor() * 1e6);
        if self.pl2_held {
            w.insert(PL2.into(), self.pl1.floor() * 1e6);
        }
        w
    }
    pub fn end_pin(&mut self, now: f64) {
        if now < self.floor_until {
            self.reset_pending = true;
        } else {
            self.reset();
        }
    }
    pub fn thermal(&mut self, dies: &Telemetry, fan_full: &BTreeMap<usize, bool>, dt: f64) {
        for (a, (tj, tm, w, bay)) in dies {
            let (Some(tj), Some(cap)) = (tj, self.tcap.get_mut(a)) else {
                continue;
            };
            let err = (tj - self.config.t_work.0).max(
                tm.map(|t| t - self.config.hbm_work)
                    .unwrap_or(tj - self.config.t_work.0),
            );
            if err > self.config.t_work.1
                && (fan_full.get(bay).copied().unwrap_or(false) || err > self.config.t_work.1 + 5.0)
            {
                let start = w.map(|w| cap.min(w + 5.0)).unwrap_or(*cap);
                *cap = start - 10.0f64.min(err - self.config.t_work.1) * dt;
            } else if err < -self.config.t_work.1 && *cap < self.gpu[a] + 10.0 {
                *cap = (self.gpu[a] + 10.0)
                    .min(*cap + 1.0f64.min(0.1 * (-err - self.config.t_work.1)) * dt);
            }
            *cap = cap.clamp(self.config.gpu_floor, self.config.gpu_max);
        }
    }
    pub fn backstop_check(&mut self, dc: Option<f64>, now: f64) -> bool {
        self.dc = dc.filter(|v| v.is_finite());
        self.over_n = if self.dc.is_some_and(|w| w > self.backstop) {
            self.over_n + 1
        } else {
            0
        };
        if self.over_n < 2 {
            return false;
        }
        for v in self.gpu.values_mut() {
            *v = self.config.gpu_floor;
        }
        self.pl1 = self.config.cpu_floor;
        self.floor_until = now + 60.0;
        self.over_n = 0;
        true
    }
    pub fn rebalance(
        &mut self,
        now: f64,
        die_w: &BTreeMap<String, Option<f64>>,
        cpu_w: Option<f64>,
    ) -> Option<String> {
        if now < self.floor_until {
            return None;
        }
        if self.reset_pending {
            self.reset_pending = false;
            self.reset();
            return Some("backstop hold over: default allocation restored".into());
        }
        if let Some((gp, pp)) = self.pin {
            if let Some(gp) = gp {
                for v in self.gpu.values_mut() {
                    *v = gp;
                }
            }
            self.pl1 = pp.unwrap_or(self.config.cpu_max);
            self.fit(pp.map(|p| p.min(self.config.cpu_floor)));
            return None;
        }
        if self.degraded {
            return None;
        }
        let c = &self.config;
        let lo = c.gpu_floor;
        let top: Allocation = self
            .gpu
            .keys()
            .map(|a| (a.clone(), self.tcap[a].min(c.gpu_max)))
            .collect();
        let mut g: Allocation = self
            .gpu
            .iter()
            .map(|(a, v)| (a.clone(), v.min(top[a])))
            .collect();
        let mut pl1 = self.pl1;
        let free = |g: &Allocation, p: f64| self.limit - self.cost_with(g, p);
        let power = |a: &String| die_w.get(a).copied().flatten();
        let idle: Vec<String> = g
            .keys()
            .filter(|a| power(a).is_some_and(|p| p <= 0.60 * g[*a]) && g[*a] > lo)
            .cloned()
            .collect();
        let cpu_sat = cpu_w.is_some_and(|w| w >= 0.85 * pl1) && pl1 < c.cpu_max;
        let mut wanted = Vec::new();
        if cpu_sat {
            let defaults = g.keys().map(|a| (a.clone(), c.gpu_init)).collect();
            let dflt = c.cpu_floor.max(self.room(&defaults));
            let target = c.cpu_max.min(dflt.max(pl1 + 10.0f64.max(0.20 * pl1)));
            let mut need = (c.k_cpu * (target - pl1) - free(&g, pl1).max(0.0)) / c.k_gpu;
            let bound = idle
                .iter()
                .map(|a| (a.clone(), lo.max(power(a).unwrap() * 1.3)))
                .collect();
            need -= level(&mut g, &idle, need, &bound, true);
            let all: Vec<_> = g.keys().cloned().collect();
            let bound = all.iter().map(|a| (a.clone(), lo)).collect();
            level(&mut g, &all, need, &bound, true);
            pl1 = target.min(pl1 + free(&g, pl1).max(0.0) / c.k_cpu);
        } else {
            wanted = g
                .keys()
                .filter(|a| power(a).is_some_and(|p| p >= 0.93 * g[*a]) && g[*a] < top[*a] - 0.5)
                .cloned()
                .collect();
            if !wanted.is_empty() {
                for a in &idle {
                    let v = lo
                        .max(power(a).unwrap() * 1.3)
                        .max(g[a] - 5.0f64.max(0.20 * g[a]));
                    g.insert(a.clone(), v);
                }
            }
            let keep = cpu_w
                .map(|w| c.cpu_floor.max(w * c.cpu_keep.0 + c.cpu_keep.1))
                .unwrap_or(pl1);
            let ceil: Allocation = wanted
                .iter()
                .map(|a| (a.clone(), top[a].min(g[a] + 5.0f64.max(0.20 * g[a]))))
                .collect();
            let need = c.k_gpu * wanted.iter().map(|a| ceil[a] - g[a]).sum::<f64>();
            let short = need - free(&g, pl1).max(0.0);
            if short > 0.0 && pl1 > keep {
                pl1 -= (pl1 - keep).min(short / c.k_cpu);
            }
            let amount = free(&g, pl1).max(0.0) / c.k_gpu;
            level(&mut g, &wanted, amount, &ceil, false);
        }
        pl1 = c.cpu_max.min(pl1 + free(&g, pl1).max(0.0) / c.k_cpu);
        if self.cost_with(&g, pl1) > self.limit + 1e-6 {
            return Some("rebalance skipped: invariant".into());
        }
        let changed =
            (pl1 - self.pl1).abs() >= 1.0 || g.iter().any(|(a, v)| (v - self.gpu[a]).abs() >= 1.0);
        self.gpu = g;
        self.pl1 = pl1;
        if changed {
            Some(format!(
                "rebalance ({}): gpu {:?} pl1 {:.0}",
                if cpu_sat {
                    "cpu wants"
                } else if !wanted.is_empty() {
                    "gpu wants"
                } else {
                    "thermal ceilings"
                },
                self.gpu.values().collect::<Vec<_>>(),
                pl1
            ))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct StaticSafe {
    pub gpu: f64,
    pub pl1: f64,
    pub fits: bool,
}
pub fn static_safe(c: &Config, base: f64, n: usize, unmanaged: usize, cpu_min: f64) -> StaticSafe {
    let limit = c.budget_limit - c.budget_margin;
    let fixed = base + c.k_gpu * 200.0 * unmanaged as f64;
    let cost = |g: f64, p: f64| fixed + c.k_gpu * n as f64 * g + c.k_cpu * p.max(cpu_min);
    let mut gpu = c.static_gpu.min(c.gpu_max).floor();
    while n > 0 && cost(gpu, c.cpu_floor) > limit && gpu > c.gpu_floor {
        gpu -= 1.0;
    }
    let mut pl1 = c
        .cpu_max
        .min((limit - fixed - c.k_gpu * n as f64 * gpu) / c.k_cpu);
    if let Some(p) = c.static_pl1 {
        pl1 = pl1.min(p);
    }
    pl1 = c.cpu_floor.max(pl1).floor();
    StaticSafe {
        gpu,
        pl1,
        fits: cost(gpu, pl1) <= limit + 1e-6,
    }
}
pub fn static_want(
    paths: impl IntoIterator<Item = String>,
    ss: &StaticSafe,
    pl2: bool,
) -> Allocation {
    let mut w: Allocation = paths.into_iter().map(|p| (p, ss.gpu * 1e6)).collect();
    w.insert(PL1.into(), ss.pl1 * 1e6);
    if pl2 {
        w.insert(PL2.into(), ss.pl1 * 1e6);
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    fn budget() -> Budget {
        Budget::new(
            Config::default(),
            (0..4)
                .map(|i| (format!("g{i}"), format!("/gpu/{i}")))
                .collect(),
            0,
            127.0,
        )
    }
    #[test]
    fn pins_charge_uncontrolled_cpu_and_reset() {
        let mut b = budget();
        b.pl2_held = false;
        b.cpu_charge(None, true);
        assert_eq!(b.cpu_min, 413.0);
        b.pin = Some((Some(200.0), Some(10.0)));
        b.rebalance(0.0, &BTreeMap::new(), None);
        assert!(b.feasible());
        assert_eq!(b.pl1, 10.0);
        b.pin = None;
        b.end_pin(1.0);
        assert!(b.feasible());
        assert!(b.cpu_w() >= 413.0);
        b.cpu_charge(Some(250e6), true);
        assert_eq!(b.cpu_min, 250.0);
        b.cpu_charge(Some(50e6), false);
        assert_eq!(b.cpu_min, 413.0);
    }
    #[test]
    fn priority_thermal_backstop_and_degraded() {
        let mut b = budget();
        let old = b.pl1;
        let watts = b.gpu.iter().map(|(a, w)| (a.clone(), Some(*w))).collect();
        b.rebalance(0.0, &watts, Some(20.0));
        assert!(b.pl1 < old);
        assert!(b.feasible());
        let before = b.pl1;
        b.rebalance(4.0, &watts, Some(before));
        assert!(b.pl1 > before);
        assert!(b.feasible());
        let data = b
            .gpu
            .keys()
            .map(|a| (a.clone(), (Some(80.0), Some(80.0), Some(100.0), 1)))
            .collect();
        b.thermal(&data, &BTreeMap::new(), 2.0);
        assert!(b.tcap.values().all(|v| *v <= 100.0));
        assert!(!b.backstop_check(Some(1200.0), 10.0));
        assert!(b.backstop_check(Some(1200.0), 12.0));
        b.pin = Some((Some(200.0), Some(413.0)));
        b.rebalance(20.0, &watts, Some(300.0));
        assert_eq!(b.pl1, 80.0);
        b.pin = None;
        b.end_pin(20.0);
        assert!(b.reset_pending);
        b.rebalance(73.0, &watts, None);
        assert!(!b.reset_pending);
        b.degraded = true;
        let before = b.gpu.clone();
        b.rebalance(100.0, &watts, Some(400.0));
        assert_eq!(b.gpu, before);
    }
    #[test]
    fn randomized_invariants_and_whole_watt_writes() {
        let mut seed = 4u64;
        let mut rand = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as f64 / u32::MAX as f64
        };
        let mut b = budget();
        for i in 0..20_000 {
            let watts = b
                .gpu
                .keys()
                .map(|a| (a.clone(), Some(rand() * 230.0)))
                .collect();
            b.rebalance(i as f64 * 4.0, &watts, Some(rand() * 450.0));
            assert!(b.feasible());
            assert!(b.gpu.values().all(|v| *v >= 85.0 && *v <= 200.0));
            for (p, uw) in b.want() {
                assert_eq!((uw / 1e6).fract(), 0.0);
                if p == PL1 {
                    assert!(uw <= b.pl1 * 1e6);
                }
            }
        }
    }
    #[test]
    fn static_safety_and_impossible_allocations() {
        let c = Config::default();
        let ss = static_safe(&c, 127.0, 4, 0, 0.0);
        assert!(ss.fits);
        assert_eq!(ss.gpu, 125.0);
        let ss = static_safe(&c, 227.0, 4, 0, 413.0);
        assert!(ss.fits);
        assert!(ss.gpu < 125.0);
        assert!(!static_safe(&c, 327.0, 4, 0, 413.0).fits);
        let mut b = budget();
        b.fixed_base = 900.0;
        b.cpu_charge(None, false);
        assert!(!b.feasible());
        let ss = static_safe(&c, 900.0, 4, 0, 413.0);
        assert!(!ss.fits);
    }
}
