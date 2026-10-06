//! A single writer, including during stop. A blocked kernel operation is not cancellable.
use crate::{
    budget::Allocation,
    transaction::{self, PowerIo},
    PL1, PL2,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

/// A detached operation retains the owner fd, even if its caller times out or
/// unwinds. Other controllers cannot acquire ownership before that I/O ends.
pub struct OwnedIo<I> {
    io: I,
    _owner: Option<Arc<crate::os::Lock>>,
}
impl<I> OwnedIo<I> {
    pub fn new(io: I, owner: Option<Arc<crate::os::Lock>>) -> Self {
        Self { io, _owner: owner }
    }
}
impl<I: PowerIo> PowerIo for OwnedIo<I> {
    fn read(&mut self, path: &str) -> Option<f64> {
        self.io.read(path)
    }
    fn write(&mut self, path: &str, uw: f64) -> bool {
        self.io.write(path, uw)
    }
}

#[derive(Clone, Debug)]
pub struct Pending {
    pub path: String,
    pub kind: &'static str,
    pub since: Instant,
}
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub ok: Option<bool>,
    pub failed: Option<String>,
    pub fail_since: Option<Instant>,
    pub pending: Option<Pending>,
    pub seen: BTreeMap<String, Option<f64>>,
    pub seen_t: BTreeMap<String, Instant>,
    pub notes: Vec<String>,
    pub pl2_bad: usize,
    pub generation: u64,
    pub applied_generation: Option<u64>,
    pub panicked: bool,
}
impl Status {
    pub fn pl2_observed(&self, now: Instant) -> Option<f64> {
        let stamp = self.seen_t.get(PL2)?;
        if now.saturating_duration_since(*stamp) > Duration::from_secs(10) {
            None
        } else {
            self.seen.get(PL2).copied().flatten()
        }
    }
    pub fn confirmed(&self) -> bool {
        self.ok == Some(true) && self.applied_generation == Some(self.generation)
    }
}
struct State {
    desired: Option<Allocation>,
    watch: BTreeSet<String>,
    raise_ok: bool,
    halted: bool,
    status: Status,
}
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}
pub struct PowerWorker {
    shared: Arc<Shared>,
}
impl PowerWorker {
    pub fn start(io: impl PowerIo + Send + 'static) -> std::io::Result<Self> {
        Self::start_with_period(io, Duration::from_secs(2))
    }
    fn start_with_period(
        mut io: impl PowerIo + Send + 'static,
        period: Duration,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                desired: None,
                watch: Default::default(),
                raise_ok: true,
                halted: false,
                status: Status::default(),
            }),
            cv: Condvar::new(),
        });
        let s = shared.clone();
        thread::Builder::new()
            .name("power-worker".into())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| run(&s, &mut io, period))).is_err() {
                    let mut state = s.lock();
                    state.status.panicked = true;
                    state.status.ok = Some(false);
                    state.status.failed = Some("power worker panicked".into());
                    state.status.fail_since.get_or_insert_with(Instant::now);
                    state.status.pending = None;
                    state.halted = true;
                    s.cv.notify_all();
                }
            })?;
        Ok(Self { shared })
    }
    pub fn set(&self, want: Allocation, mut watch: BTreeSet<String>, raise_ok: bool) {
        if want.contains_key(PL1) && !want.contains_key(PL2) {
            watch.insert(PL2.into());
        }
        let mut s = self.shared.lock();
        if s.halted {
            return;
        }
        if s.desired.as_ref() != Some(&want) || s.watch != watch || s.raise_ok != raise_ok {
            s.desired = Some(want);
            s.watch = watch;
            s.raise_ok = raise_ok;
            s.status.generation = s.status.generation.wrapping_add(1);
            self.shared.cv.notify_all();
        }
    }
    pub fn status(&self) -> Status {
        let mut s = self.shared.lock();
        let result = s.status.clone();
        s.status.notes.clear();
        result
    }
    pub fn stop(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut s = self.shared.lock();
        s.halted = true;
        self.shared.cv.notify_all();
        while s.status.pending.is_some() {
            let Some(left) = end.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, _) = self
                .shared
                .cv
                .wait_timeout(s, left)
                .unwrap_or_else(|e| e.into_inner());
            s = next;
        }
        true
    }
}
impl Drop for PowerWorker {
    fn drop(&mut self) {
        let mut s = self.shared.lock();
        s.halted = true;
        self.shared.cv.notify_all();
    }
}
struct Operation<'a> {
    s: &'a Shared,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        let mut s = self.s.lock();
        s.status.pending = None;
        self.s.cv.notify_all();
    }
}
struct WorkerIo<'a, I> {
    s: &'a Shared,
    io: &'a mut I,
    gen: u64,
    aborted: bool,
    found: BTreeMap<String, Option<f64>>,
    read_t: BTreeMap<String, Instant>,
}
impl<I: PowerIo> WorkerIo<'_, I> {
    fn begin(&mut self, p: &str, kind: &'static str) -> bool {
        let mut s = self.s.lock();
        if self.aborted || s.halted || s.status.generation != self.gen {
            self.aborted = true;
            return false;
        }
        s.status.pending = Some(Pending {
            path: p.into(),
            kind,
            since: Instant::now(),
        });
        true
    }
}
impl<I: PowerIo> PowerIo for WorkerIo<'_, I> {
    fn read(&mut self, p: &str) -> Option<f64> {
        if !self.begin(p, "read") {
            return None;
        }
        let _operation = Operation { s: self.s };
        let v = self.io.read(p);
        self.read_t.insert(p.into(), Instant::now());
        self.found.entry(p.into()).or_insert(v);
        v
    }
    fn write(&mut self, p: &str, uw: f64) -> bool {
        if !self.begin(p, "write") {
            return false;
        }
        let _operation = Operation { s: self.s };
        self.io.write(p, uw)
    }
}
fn run(s: &Shared, io: &mut impl PowerIo, period: Duration) {
    loop {
        let (want, watch, gen, raise_ok, before) = {
            let mut state = s.lock();
            if state.status.applied_generation == Some(state.status.generation) && !state.halted {
                state =
                    s.cv.wait_timeout(state, period)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
            }
            while state.desired.is_none() && !state.halted {
                state = s.cv.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.halted {
                return;
            }
            (
                state.desired.clone().unwrap(),
                state.watch.clone(),
                state.status.generation,
                state.raise_ok,
                state.status.seen.clone(),
            )
        };
        let mut worker = WorkerIo {
            s,
            io,
            gen,
            aborted: false,
            found: Default::default(),
            read_t: Default::default(),
        };
        let mut t = transaction::transact(&want, &mut worker, raise_ok);
        for p in watch.iter().filter(|p| !want.contains_key(*p)) {
            t.seen.insert(p.clone(), worker.read(p));
        }
        let mut state = s.lock();
        if worker.aborted || state.halted || state.status.generation != gen {
            continue;
        }
        if state.status.applied_generation == Some(gen) {
            let changed: Vec<_> = want
                .iter()
                .filter(|(p, v)| {
                    transaction::is_set(before.get(*p).copied().flatten(), **v)
                        && worker.found.get(*p).copied().flatten().is_some()
                        && !transaction::is_set(worker.found.get(*p).copied().flatten(), **v)
                })
                .map(|(p, _)| p.clone())
                .collect();
            if !changed.is_empty() {
                state.status.notes.push(format!(
                    "limits changed by someone else, ours restored: {changed:?}"
                ));
            }
        }
        for (p, v) in t.seen {
            if let Some(stamp) = worker.read_t.get(&p) {
                state.status.seen_t.insert(p.clone(), *stamp);
                state.status.seen.insert(p, v);
            }
        }
        state.status.ok = Some(t.ok);
        state.status.applied_generation = Some(gen);
        if t.ok {
            state.status.fail_since = None;
        } else {
            state.status.fail_since.get_or_insert_with(Instant::now);
        }
        state.status.pl2_bad = if t.failed.as_deref() == Some(PL2) {
            state.status.pl2_bad.saturating_add(1)
        } else {
            0
        };
        state.status.failed = t.failed;
        s.cv.notify_all();
    }
}

/// Bound a whole static recovery to one deadline. Timed-out work is detached;
/// its caller must not start another operation or transaction afterwards.
pub fn call_deadline<T: Send + 'static>(
    end: Instant,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    end.checked_duration_since(Instant::now())?;
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("recovery-io".into())
        .spawn(move || {
            let result = if Instant::now() < end {
                Some(f())
            } else {
                None
            };
            let _ = tx.send(result);
        })
        .ok()?;
    rx.recv_timeout(end.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
}
pub struct DeadlineIo<I> {
    pub io: Arc<Mutex<I>>,
    pub end: Instant,
    pub unresolved: Vec<String>,
    aborted: bool,
}
impl<I: PowerIo + Send + 'static> DeadlineIo<I> {
    pub fn new(io: I, end: Instant) -> Self {
        Self {
            io: Arc::new(Mutex::new(io)),
            end,
            unresolved: Vec::new(),
            aborted: false,
        }
    }
}
impl<I: PowerIo + Send + 'static> PowerIo for DeadlineIo<I> {
    fn read(&mut self, p: &str) -> Option<f64> {
        if self.aborted {
            return None;
        }
        let io = self.io.clone();
        let path = p.to_string();
        match call_deadline(self.end, move || {
            io.lock().unwrap_or_else(|e| e.into_inner()).read(&path)
        }) {
            Some(v) => v,
            None => {
                self.unresolved.push(format!("read {p}"));
                self.aborted = true;
                None
            }
        }
    }
    fn write(&mut self, p: &str, uw: f64) -> bool {
        if self.aborted {
            return false;
        }
        let io = self.io.clone();
        let path = p.to_string();
        match call_deadline(self.end, move || {
            io.lock()
                .unwrap_or_else(|e| e.into_inner())
                .write(&path, uw)
        }) {
            Some(v) => v,
            None => {
                self.unresolved.push(format!("write {p}"));
                self.aborted = true;
                false
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::tests::Fake;
    fn wait_for(w: &PowerWorker, p: impl Fn(&Status) -> bool) {
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if p(&w.status()) {
                return;
            }
            assert!(Instant::now() < end, "worker did not reach expected state");
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn reconcile_and_fresh_unheld_pl2() {
        let values = [(PL1.into(), 80e6), (PL2.into(), 300e6)]
            .into_iter()
            .collect();
        let w =
            PowerWorker::start_with_period(Fake::new(values), Duration::from_millis(10)).unwrap();
        w.set(
            [(PL1.into(), 80e6)].into_iter().collect(),
            Default::default(),
            true,
        );
        wait_for(&w, |s| s.confirmed());
        let s = w.status();
        assert_eq!(s.pl2_observed(Instant::now()), Some(300e6));
        assert_eq!(
            s.pl2_observed(Instant::now() + Duration::from_secs(11)),
            None
        );
        assert!(w.stop(Duration::from_secs(1)));
    }
    struct Block {
        values: Allocation,
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        writes: Arc<Mutex<Vec<f64>>>,
    }
    impl PowerIo for Block {
        fn read(&mut self, p: &str) -> Option<f64> {
            self.values.get(p).copied()
        }
        fn write(&mut self, p: &str, uw: f64) -> bool {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            self.writes.lock().unwrap().push(uw);
            self.values.insert(p.into(), uw);
            true
        }
    }
    #[test]
    fn stop_and_supersession_start_no_competing_operation() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writes = Arc::new(Mutex::new(vec![]));
        let w = PowerWorker::start(Block {
            values: [("a".into(), 200e6), ("b".into(), 200e6)]
                .into_iter()
                .collect(),
            entered: entered_tx,
            release: release_rx,
            writes: writes.clone(),
        })
        .unwrap();
        w.set(
            [("a".into(), 100e6), ("b".into(), 100e6)]
                .into_iter()
                .collect(),
            Default::default(),
            true,
        );
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        w.set(
            [("a".into(), 80e6), ("b".into(), 80e6)]
                .into_iter()
                .collect(),
            Default::default(),
            true,
        );
        assert!(!w.stop(Duration::from_millis(20)));
        release_tx.send(()).unwrap();
        assert!(w.stop(Duration::from_secs(1)));
        thread::sleep(Duration::from_millis(10));
        assert_eq!(*writes.lock().unwrap(), vec![100e6]);
    }
    #[test]
    fn recovery_deadline_does_not_start_late_work() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let writes = Arc::new(Mutex::new(vec![]));
        let block = Block {
            values: [("a".into(), 200e6), ("b".into(), 200e6)]
                .into_iter()
                .collect(),
            entered: entered_tx,
            release: release_rx,
            writes: writes.clone(),
        };
        let mut io = DeadlineIo::new(block, Instant::now() + Duration::from_millis(30));
        let t = transaction::transact(
            &[("a".into(), 100e6), ("b".into(), 100e6)]
                .into_iter()
                .collect(),
            &mut io,
            true,
        );
        assert!(!t.ok);
        assert_eq!(io.unresolved.len(), 1);
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(*writes.lock().unwrap(), vec![100e6]);
    }
    struct SharedFake(Arc<Mutex<Fake>>);
    impl PowerIo for SharedFake {
        fn read(&mut self, p: &str) -> Option<f64> {
            self.0.lock().unwrap().read(p)
        }
        fn write(&mut self, p: &str, v: f64) -> bool {
            self.0.lock().unwrap().write(p, v)
        }
    }
    #[test]
    fn failed_pl2_is_retried_and_external_caps_are_restored() {
        let fake = Arc::new(Mutex::new(Fake::new(
            [
                (PL1.into(), 413e6),
                (PL2.into(), 413e6),
                ("gpu".into(), 200e6),
            ]
            .into_iter()
            .collect(),
        )));
        fake.lock().unwrap().fail = Some(PL2.into());
        let w = PowerWorker::start_with_period(SharedFake(fake.clone()), Duration::from_millis(5))
            .unwrap();
        let want = [
            (PL1.into(), 80e6),
            (PL2.into(), 80e6),
            ("gpu".into(), 125e6),
        ]
        .into_iter()
        .collect();
        w.set(want, Default::default(), true);
        wait_for(&w, |s| s.pl2_bad >= 3);
        assert_eq!(w.status().seen[PL2], Some(413e6));
        fake.lock().unwrap().fail = None;
        wait_for(&w, Status::confirmed);
        fake.lock().unwrap().values.insert("gpu".into(), 200e6);
        wait_for(&w, |s| !s.notes.is_empty());
        assert_eq!(fake.lock().unwrap().values["gpu"], 125e6);
        assert!(w.stop(Duration::from_secs(1)));
    }
    struct Panics;
    impl PowerIo for Panics {
        fn read(&mut self, _: &str) -> Option<f64> {
            panic!("injected I/O panic")
        }
        fn write(&mut self, _: &str, _: f64) -> bool {
            panic!("unexpected write")
        }
    }
    #[test]
    fn worker_panic_reports_alarm_and_is_quiescent() {
        let w = PowerWorker::start(Panics).unwrap();
        w.set(
            [("gpu".into(), 125e6)].into_iter().collect(),
            Default::default(),
            true,
        );
        wait_for(&w, |s| s.panicked);
        assert_eq!(w.status().ok, Some(false));
        assert!(w.stop(Duration::from_secs(1)));
    }
    struct SlowLastRead {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl PowerIo for SlowLastRead {
        fn read(&mut self, p: &str) -> Option<f64> {
            if p == "gpu" {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Some(80e6)
        }
        fn write(&mut self, _: &str, _: f64) -> bool {
            panic!("all limits already set")
        }
    }
    #[test]
    fn pl2_timestamp_is_its_read_time_not_the_transaction_end() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let w = PowerWorker::start(SlowLastRead {
            entered: tx,
            release: release_rx,
        })
        .unwrap();
        w.set(
            [(PL1.into(), 80e6), (PL2.into(), 80e6), ("gpu".into(), 80e6)]
                .into_iter()
                .collect(),
            Default::default(),
            true,
        );
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let since = w.status().pending.unwrap().since;
        thread::sleep(Duration::from_millis(20));
        release_tx.send(()).unwrap();
        wait_for(&w, Status::confirmed);
        let s = w.status();
        assert!(s.seen_t[PL2] <= since);
        assert!(s.seen_t["gpu"] > since);
        assert!(w.stop(Duration::from_secs(1)));
    }
    #[test]
    fn detached_worker_and_recovery_retain_owner_until_io_finishes() {
        for recovery in [false, true] {
            let root = crate::os::tests::temp();
            let path = root.join("owner.lock");
            let owner = Arc::new(crate::os::Lock::open(&path).unwrap());
            assert!(owner.acquire(Duration::ZERO).unwrap());
            let (tx, rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let writes = Arc::new(Mutex::new(vec![]));
            let io = OwnedIo::new(
                Block {
                    values: [("gpu".into(), 200e6)].into_iter().collect(),
                    entered: tx,
                    release: release_rx,
                    writes: writes.clone(),
                },
                Some(owner.clone()),
            );
            let want = [("gpu".into(), 125e6)].into_iter().collect();
            if recovery {
                let mut bounded = DeadlineIo::new(io, Instant::now() + Duration::from_millis(20));
                assert!(!transaction::transact(&want, &mut bounded, true).ok);
                rx.recv_timeout(Duration::from_secs(1)).unwrap();
                drop(bounded);
            } else {
                let w = PowerWorker::start(io).unwrap();
                w.set(want, Default::default(), true);
                rx.recv_timeout(Duration::from_secs(1)).unwrap();
                assert!(!w.stop(Duration::from_millis(20)));
                drop(w);
            }
            drop(owner);
            let next = crate::os::Lock::open(&path).unwrap();
            assert!(!next.acquire(Duration::ZERO).unwrap());
            release_tx.send(()).unwrap();
            assert!(next.acquire(Duration::from_secs(1)).unwrap());
            assert_eq!(*writes.lock().unwrap(), vec![125e6]);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
