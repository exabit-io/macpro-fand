//! Read/decrease/stage/raise transactions, with read-back after every write.
use crate::{budget::Allocation, LIMIT_MIN_UW, PL1, PL2};
use std::collections::BTreeMap;
pub trait PowerIo {
    fn read(&mut self, path: &str) -> Option<f64>;
    fn write(&mut self, path: &str, uw: f64) -> bool;
}
#[derive(Clone, Debug)]
pub struct Transaction {
    pub ok: bool,
    pub failed: Option<String>,
    pub seen: BTreeMap<String, Option<f64>>,
}
pub fn is_set(got: Option<f64>, target: f64) -> bool {
    got.is_some_and(|v| v.is_finite() && v >= target - 1e6 && v <= target)
}
fn pl_order(mut paths: Vec<String>, pl2_first: bool) -> Vec<String> {
    let (first, second) = if pl2_first { (PL2, PL1) } else { (PL1, PL2) };
    if paths.iter().any(|p| p == first) && paths.iter().any(|p| p == second) {
        paths.retain(|p| p != second);
        let i = paths.iter().position(|p| p == first).unwrap();
        paths.insert(i + 1, second.into());
    }
    paths
}
pub fn transact(want: &Allocation, io: &mut impl PowerIo, raise_ok: bool) -> Transaction {
    let mut t = Transaction {
        ok: false,
        failed: None,
        seen: BTreeMap::new(),
    };
    if let Some((p, _)) = want
        .iter()
        .find(|(_, v)| !v.is_finite() || **v < LIMIT_MIN_UW || **v > i64::MAX as f64)
    {
        t.failed = Some(p.clone());
        return t;
    }
    let mut cur: BTreeMap<String, Option<f64>> = want
        .keys()
        .map(|p| (p.clone(), io.read(p).filter(|v| v.is_finite())))
        .collect();
    t.seen = cur.clone();
    fn put(
        p: &str,
        v: f64,
        io: &mut impl PowerIo,
        cur: &mut BTreeMap<String, Option<f64>>,
        t: &mut Transaction,
    ) -> bool {
        if !io.write(p, v) {
            return false;
        }
        let got = io.read(p).filter(|v| v.is_finite());
        cur.insert(p.into(), got);
        t.seen.insert(p.into(), got);
        is_set(got, v)
    }
    let mut down: Vec<_> = want
        .keys()
        .filter(|p| cur[*p].is_some_and(|v| v > want[*p]))
        .cloned()
        .collect();
    down.sort_by(|a, b| {
        (want[a] - cur[a].unwrap())
            .total_cmp(&(want[b] - cur[b].unwrap()))
            .then(a.cmp(b))
    });
    for p in pl_order(down, false) {
        if !put(&p, want[&p], io, &mut cur, &mut t) {
            t.failed = Some(p);
            return t;
        }
    }
    let unknown = want.keys().filter(|p| cur[*p].is_none()).cloned().collect();
    for p in pl_order(unknown, false) {
        let mut v = LIMIT_MIN_UW;
        if p == PL2 {
            if let Some(Some(pl1)) = cur.get(PL1) {
                v = want[&p].min(v.max(*pl1));
            }
        }
        if !put(&p, v, io, &mut cur, &mut t) {
            t.failed = Some(p);
            return t;
        }
    }
    let up = pl_order(
        want.keys()
            .filter(|p| !is_set(cur[*p], want[*p]))
            .cloned()
            .collect(),
        true,
    );
    if !up.is_empty() && !raise_ok {
        t.failed = Some("infeasible".into());
        return t;
    }
    for p in up {
        if !put(&p, want[&p], io, &mut cur, &mut t) {
            t.failed = Some(p);
            return t;
        }
    }
    t.ok = true;
    t
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub struct Fake {
        pub values: Allocation,
        pub unread: std::collections::BTreeSet<String>,
        pub writes: Vec<(String, f64)>,
        pub fail: Option<String>,
        pub round_up: bool,
        pub states: Vec<Allocation>,
    }
    impl Fake {
        pub fn new(values: Allocation) -> Self {
            Self {
                values,
                unread: Default::default(),
                writes: vec![],
                fail: None,
                round_up: false,
                states: vec![],
            }
        }
    }
    impl PowerIo for Fake {
        fn read(&mut self, p: &str) -> Option<f64> {
            if self.unread.remove(p) {
                None
            } else {
                self.values.get(p).copied()
            }
        }
        fn write(&mut self, p: &str, uw: f64) -> bool {
            self.writes.push((p.into(), uw));
            if self.fail.as_deref() == Some(p) {
                return false;
            }
            self.values
                .insert(p.into(), uw + if self.round_up { 1.0 } else { 0.0 });
            self.states.push(self.values.clone());
            true
        }
    }
    fn alloc(gpu: f64, cpu: f64) -> Allocation {
        (0..4)
            .map(|i| (format!("g{i}"), gpu * 1e6))
            .chain([(PL1.into(), cpu * 1e6), (PL2.into(), cpu * 1e6)])
            .collect()
    }
    fn cost(g: &Allocation) -> f64 {
        327.0
            + 1.205 * (0..4).map(|i| g[&format!("g{i}")] / 1e6).sum::<f64>()
            + 1.156 * (g[PL1] / 1e6).max(g[PL2] / 1e6)
    }
    #[test]
    fn unreadable_raised_floors_do_not_take_headroom() {
        let mut cases = 0;
        for start_g in [10.0, 50.0, 85.0, 125.0, 150.0, 200.0] {
            for start_c in [10.0, 80.0, 200.0, 413.0] {
                for target_cpu in [10.0, 80.0, 188.0, 413.0] {
                    for mask in 0..64 {
                        let start = alloc(start_g, start_c);
                        let want = alloc(125.0, target_cpu);
                        let bound = cost(&start).max(cost(&want));
                        let mut io = Fake::new(start);
                        for (i, p) in want.keys().enumerate() {
                            if mask & (1 << i) != 0 {
                                io.unread.insert(p.clone());
                            }
                        }
                        assert!(transact(&want, &mut io, true).ok);
                        for state in &io.states {
                            assert!(
                                cost(state) <= bound + 1e-6,
                                "mask {mask}, cost {} > {bound}",
                                cost(state)
                            );
                        }
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 6144);
    }
    #[test]
    fn below_minimum_exception_stays_in_default_margin() {
        let want = alloc(125.0, 413.0);
        let mut io = Fake::new(alloc(0.0, 0.0));
        io.unread = want.keys().cloned().collect();
        assert!(transact(&want, &mut io, true).ok);
        let stage = &io.states[5];
        assert!((cost(stage) - 327.0 - 59.76).abs() < 1e-6);
    }
    #[test]
    fn infeasible_only_lowers_even_for_low_cpu_pin() {
        let mut io = Fake::new(alloc(200.0, 10.0));
        let t = transact(&alloc(125.0, 80.0), &mut io, false);
        assert!(!t.ok);
        assert_eq!(t.failed.as_deref(), Some("infeasible"));
        assert!(io.writes.iter().all(|(p, _)| p.starts_with('g')));
        assert_eq!(io.values[PL1], 10e6);
    }
    #[test]
    fn first_failure_stops_and_rounding_never_confirms_above_target() {
        for round in [false, true] {
            let mut io = Fake::new(alloc(200.0, 413.0));
            io.round_up = round;
            if !round {
                io.fail = Some(PL1.into());
            }
            let t = transact(&alloc(125.0, 200.0), &mut io, true);
            assert!(!t.ok);
            assert_eq!(io.writes.len(), 1);
        }
        for v in [Some(125e6 + 1.0), Some(123e6), None, Some(f64::NAN)] {
            assert!(!is_set(v, 125e6));
        }
        assert!(is_set(Some(124e6), 125e6));
        let mut io = Fake::new(alloc(200.0, 413.0));
        let mut want = alloc(125.0, 200.0);
        want.insert("g0".into(), 0.0);
        assert!(!transact(&want, &mut io, true).ok);
        assert!(io.writes.is_empty());
    }
    #[test]
    fn cpu_write_order_and_reconciliation() {
        let mut io = Fake::new(alloc(125.0, 200.0));
        let want = alloc(125.0, 300.0);
        assert!(transact(&want, &mut io, true).ok);
        assert_eq!(
            io.writes
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            vec![PL2, PL1]
        );
        io.writes.clear();
        assert!(transact(&want, &mut io, true).ok);
        assert!(io.writes.is_empty());
        assert!(transact(&alloc(125.0, 80.0), &mut io, true).ok);
        assert_eq!(
            io.writes
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            vec![PL1, PL2]
        );
    }
}
