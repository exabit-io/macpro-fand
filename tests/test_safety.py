#!/usr/bin/env python3
"""test_safety.py SCRIPT -- failure-path regression tests for macpro-fand.

Each test asserts the SAFE behaviour for a failure mode found in two external reviews (finding ids in brackets:
F/L first review, R/Q second): power transactions that stop at the first failed or unconfirmed write and never raise
a limit while any limit is unreadable (every combination), serialized writes that cannot land out of order and a stop
that cannot be overtaken by the worker, retry until read-back, unmanaged dies and an uncontrolled CPU charged at their
worst case in every path (pins, resets, the stop hook), the backstop hold outranking pins, private bounded locks and
one hardware owner (the stop hook included), strict and exception-proof configuration validation, rounding and
read-back that never exceed the allocation, per-die reading ages and sample-time filtering, persistent read alarms,
temperature alarms independent of power, RAPL read gaps, and cleanup from the first hardware write. Nothing is
written to hardware and the host inventory (GPUs, NVMe, disks) is replaced by a fixture, so it runs as any user on any
build host. Exit status 0 when every check passes.
"""
import importlib.machinery
import importlib.util
import json
import math
import os
import sys
import tempfile
import threading
import time
from unittest.mock import patch

SCRIPT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "macpro-fand")
DIES = ["0b:00.0", "0e:00.0", "1b:00.0", "1e:00.0"]
failures = []


def load():
    loader = importlib.machinery.SourceFileLoader("macpro_fand_safety", SCRIPT)
    spec = importlib.util.spec_from_loader(loader.name, loader)
    m = importlib.util.module_from_spec(spec)
    loader.exec_module(m)
    # host inventory replaced by a fixture (the Mac Pro under test: four Vega 20 dies, no extra devices), so the result
    # does not depend on the build host's GPUs, NVMe drives or disks
    m.die_map = lambda: {"0b:00.0": 1, "0e:00.0": 1, "1b:00.0": 2, "1e:00.0": 2}
    m.devices = lambda: {"mellanox": 0, "nvme": 0, "disk": 0}
    m.gpu_model_ok = lambda addr: True
    return m


def check(cond, what):
    print(("ok    " if cond else "FAIL  ") + what)
    if not cond:
        failures.append(what)


class Hw:
    """A fake set of power limits (uW) with scripted failures, delays and a record of the order of writes."""

    def __init__(self, m, gpu_w, pl1_w, pl2_w=None):
        self.m = m
        self.caps = [f"/hw/{a}/power1_cap" for a in DIES]
        self.v = {p: gpu_w * 1e6 for p in self.caps}
        self.v[m.RAPL_PL1] = pl1_w * 1e6
        self.v[m.RAPL_PL2] = (pl2_w if pl2_w is not None else pl1_w) * 1e6
        self.fail_write, self.unreadable, self.writes = set(), set(), []
        self.unknown_first, self.unknown, self.raised_blind = set(), set(), False
        self.peak = self.model()

    def model(self):
        m = self.m
        gpu = sum(self.v[p] for p in self.caps) / 1e6
        cpu = max(self.v[m.RAPL_PL1], self.v[m.RAPL_PL2]) / 1e6
        return m.base() + m.K_GPU * gpu + m.K_CPU * cpu

    def read(self, p):
        if p in self.unknown_first:              # unreadable once (the transaction's first read), fine afterwards
            self.unknown_first.discard(p)
            self.unknown.add(p)
            return None
        return None if p in self.unreadable else self.v[p]

    def write(self, p, uw):
        self.writes.append(p)
        if p in self.fail_write:
            return False
        if uw > self.v[p] and (self.unknown - {p} or p in self.unknown):
            self.raised_blind = True              # a limit went up while some limit's value was not known
        self.unknown.discard(p)
        self.v[p] = uw
        self.peak = max(self.peak, self.model())
        return True


def want(m, gpu_w, pl1_w, pl2=True):
    w = {f"/hw/{a}/power1_cap": gpu_w * 1e6 for a in DIES}
    w[m.RAPL_PL1] = pl1_w * 1e6
    if pl2:
        w[m.RAPL_PL2] = pl1_w * 1e6
    return w


def test_transaction(m):
    m.GPU_CAP["max"] = 200
    # [F01] a failed decrease stops the transaction: the CPU increase that depended on it is never written
    hw = Hw(m, 190, 90)
    hw.fail_write = set(hw.caps)
    ok, failed, _ = m.transact(want(m, 125, 362), hw.read, hw.write)
    check(not ok and failed in hw.caps and m.RAPL_PL1 not in hw.writes and hw.model() < 1148 + 1,
          f"[F01] a failed GPU decrease blocks the CPU increase (model {hw.model():.0f} W, writes {len(hw.writes)})")
    hw = Hw(m, 190, 90)                            # the same with the CPU's limits first in the request
    hw.fail_write = set(hw.caps)
    w = want(m, 125, 362)
    ok, failed, _ = m.transact({m.RAPL_PL1: w[m.RAPL_PL1], m.RAPL_PL2: w[m.RAPL_PL2], **w}, hw.read, hw.write)
    check(not ok and m.RAPL_PL1 not in hw.writes and m.RAPL_PL2 not in hw.writes,
          "[F01] ... whatever the order of the request")
    # [F02] an unreadable cap is never treated as a decrease: confirmed decreases (PL1, PL2) go first
    hw = Hw(m, 125, 362)
    hw.unreadable = set(hw.caps)
    ok, _, _ = m.transact(want(m, 180, 125), hw.read, hw.write)
    check(hw.writes[:2] == [m.RAPL_PL1, m.RAPL_PL2] and hw.peak <= max(Hw(m, 125, 362).model(), hw.model()) + 1,
          f"[F02] unknown caps are written after the confirmed decreases (peak {hw.peak:.0f} W)")
    # [F02] ... and if a confirmed decrease fails, nothing unknown or increasing is written
    hw = Hw(m, 125, 362)
    hw.unreadable = set(hw.caps)
    hw.fail_write = {m.RAPL_PL1}
    ok, _, _ = m.transact(want(m, 180, 125), hw.read, hw.write)
    check(not ok and hw.writes == [m.RAPL_PL1], "[F02] a failed PL1 decrease blocks the unknown GPU writes")
    # read-back: a write that does not read back stops the transaction
    hw = Hw(m, 150, 200)
    orig = hw.read
    hw.read = lambda p: (orig(p) + 5e6) if p == hw.caps[0] and hw.writes else orig(p)
    ok, failed, _ = m.transact(want(m, 125, 250), hw.read, hw.write)
    check(not ok and failed == hw.caps[0] and m.RAPL_PL1 not in hw.writes,
          "a decrease that does not read back counts as failed; no increase follows")
    # PL2 stays >= PL1: lowered after PL1, raised before it
    down, up = Hw(m, 125, 362), Hw(m, 125, 125)
    m.transact(want(m, 125, 125), down.read, down.write)
    m.transact(want(m, 125, 362), up.read, up.write)
    check(down.writes == [m.RAPL_PL1, m.RAPL_PL2] and up.writes == [m.RAPL_PL2, m.RAPL_PL1],
          "PL2 is lowered after PL1 and raised before it")
    # [R07] read-back never counts a limit above its target as set, and a limit above its target is lowered
    hw = Hw(m, 150, 200)
    orig = hw.read
    hw.read = lambda p: (orig(p) + 0.5e6) if hw.writes else orig(p)
    ok, failed, _ = m.transact(want(m, 125, 250), hw.read, hw.write)
    check(not ok and failed == hw.caps[0], "[R07] a limit reading back 0.5 W above its target is not confirmed")
    hw = Hw(m, 125.5, 362)
    ok, _, _ = m.transact(want(m, 125, 362), hw.read, hw.write)
    check(ok and sorted(hw.writes) == sorted(hw.caps), "[R07] a limit 0.5 W above its target is lowered, not accepted")
    # [R04] no target below LIMIT_MIN (amdgpu: 0 = restore the firmware default) is ever written
    hw = Hw(m, 125, 362)
    ok, failed, _ = m.transact({**want(m, 125, 362), hw.caps[0]: 0.0}, hw.read, hw.write)
    check(not ok and not hw.writes, "[R04] a zero (or sub-LIMIT_MIN) target refuses the transaction before any write")


def test_unknown_combinations():
    """[R01/V01] Every combination of unreadable limits, for transitions in both directions, under the default policy,
    with 200 W of extra devices, and with raised GPU and CPU floors while the old caps (or a CPU pin) sit below them:
    no state on the way costs more than the larger of start and target, nothing is raised while any limit is unknown,
    and every transaction ends confirmed at its target."""
    import itertools
    n = bad_peak = blind = unconfirmed = 0
    worst = 0.0
    # (GPU floor, GPU max, CPU floor, BASE)
    for gf, gm, cf, base in ((85, 200, 80, 127.0), (85, 200, 80, 200.0), (125, 125, 80, 200.0), (150, 200, 80, 127.0),
                             (85, 200, 160, 127.0), (125, 125, 160, 200.0)):
        m = load()
        m._BASE = m.BASE = base
        m.GPU_CAP.update(floor=gf, max=gm, init=gf)
        m.CPU_PL1["floor"] = cf
        lim = m.BUDGET_LIMIT - m.BUDGET_MARGIN
        room = lambda g: min(413, math.floor((lim - base - m.K_GPU * 4 * g) / m.K_CPU))
        moves = [((85, 413), gf), ((85, 40), gf), ((gm, cf), gf), ((gf, None), gm), ((125, None), max(gf, 125)),
                 ((200, 80), gf), ((gf, None), gf)]
        for (g0, c0), g1 in moves:
            c0 = room(g0) if c0 is None else c0
            c1 = room(g1)
            if c1 < cf or c0 < 10:
                continue                                   # no valid target at this floor
            paths = [f"/hw/{a}/power1_cap" for a in DIES] + [m.RAPL_PL1, m.RAPL_PL2]
            for k in range(len(paths) + 1):
                for sub in itertools.combinations(paths, k):
                    hw = Hw(m, g0, c0)
                    hw.unknown_first = set(sub)
                    start = hw.model()
                    ok, _, _ = m.transact(want(m, g1, c1), hw.read, hw.write)
                    target = Hw(m, g1, c1).model()
                    n += 1
                    worst = max(worst, hw.peak - max(start, target))
                    bad_peak += hw.peak > max(start, target) + 1e-6
                    blind += hw.raised_blind
                    unconfirmed += not (ok and abs(hw.model() - target) < 1e-6)
    check(bad_peak == 0 and blind == 0 and unconfirmed == 0,
          f"[R01/V01] {n} transactions, every combination of unreadable limits, raised floors with old caps and CPU pins "
          f"below them: peak never above max(start, target) (worst excess {worst:+.3f} W), nothing raised while "
          f"unknown ({blind}), all confirmed ({n - unconfirmed})")
    # [V01] the review's case: floor/max 125 at BASE 200, four GPUs at 85 W and the CPU at 413 W, all reads missing
    m = load()
    m._BASE = m.BASE = 200.0
    m.GPU_CAP.update(floor=125, max=125, init=125)
    hw = Hw(m, 85, 413)
    hw.unknown_first = set(hw.v)
    start = hw.model()
    ok, _, _ = m.transact(want(m, 125, 298), hw.read, hw.write)
    check(ok and hw.peak <= max(start, Hw(m, 125, 298).model()) + 1e-6,
          f"[V01] raised 125 W floor, caps at 85 W, CPU at 413 W, every read missing: peak {hw.peak:.0f} W "
          f"(start {start:.0f}, target {Hw(m, 125, 298).model():.0f})")
    # [V02] a plan that does not fit only lowers: nothing is raised, and the transaction says so
    hw = Hw(m, 85, 40)
    ok, failed, _ = m.transact(want(m, 125, 160), hw.read, hw.write, raise_ok=False)
    hw2 = Hw(m, 150, 300)
    ok2, _, _ = m.transact(want(m, 125, 160), hw2.read, hw2.write, raise_ok=False)
    check(not ok and failed == "infeasible" and not hw.writes and sorted(hw2.writes) == sorted(hw2.v)
          and hw2.model() == Hw(m, 125, 160).model(),
          "[V02] raise_ok=False: increases refused before any write, decreases still made")


def test_worker(m):
    m.RECONCILE = 0.05
    vals, log_w, gate = {}, [], threading.Event()
    path = "/hw/x/power1_cap"

    def rd(p, scale=1.0):
        return vals.get(p)

    def wr(p, v):
        log_w.append((p, v, time.monotonic()))
        if v == 190e6:                    # the first write hangs in the "driver" until released
            gate.wait(5)
        if fail_once and not done_once[0]:
            done_once[0] = True
            raise OSError("transient")
        vals[p] = float(v)
    fail_once, done_once = False, [False]
    m.rd, m.wr = rd, wr
    vals[path] = 125e6
    w = m.PowerWorker()
    w.start()
    w.set({path: 190e6})
    time.sleep(0.2)
    w.set({path: 125e6})                  # a newer, lower allocation arrives while the old write hangs
    time.sleep(0.2)
    pending = w.status()["pending"]
    started_during_hang = len(log_w)
    gate.set()
    time.sleep(0.5)
    check(pending is not None and pending[1] == "write" and started_during_hang == 1,
          "[F03] writes are serialized: nothing else starts while a write hangs, and the hang is visible")
    check(vals[path] == 125e6 and w.status()["ok"],
          "[F03] after the hung write completes, the newest allocation is applied and confirmed")
    # [F04] a failed write is retried on its own (no change of the desired allocation needed)
    m2 = load()
    m2.RECONCILE = 0.05
    vals2, attempts = {path: 200e6}, []

    def wr2(p, v):
        attempts.append(v)
        if len(attempts) == 1:
            raise OSError("transient")
        vals2[p] = float(v)
    m2.rd, m2.wr = (lambda p, scale=1.0: vals2.get(p)), wr2
    w2 = m2.PowerWorker()
    w2.start()
    w2.set({path: 125e6})
    time.sleep(0.4)
    check(len(attempts) >= 2 and vals2[path] == 125e6 and w2.status()["ok"],
          f"[F04] a failed startup decrease is retried until it reads back ({len(attempts)} attempts)")
    # someone else's write is detected and undone
    vals2[path] = 200e6
    time.sleep(0.3)
    st = w2.status()
    check(vals2[path] == 125e6 and any("someone else" in n for n in st["notes"]),
          "an outside write is restored and reported")
    w.stop(1.0)
    w2.stop(1.0)


def test_accounting(m):
    m._BASE = m.BASE
    m.GPU_CAP["max"] = 200
    # [F05] a die without hwmon is charged GPU_HW_MAX: no extra CPU headroom appears
    full = m.Budget({a: f"/hw/{a}" for a in DIES})
    three = m.Budget({a: f"/hw/{a}" for a in DIES[:3]}, n_unmanaged=1)
    check(three.pl1 <= full.pl1 + 1e-6 and three.cost() <= three.limit + 1e-6
          and abs(three.fixed() - (m.base() + m.K_GPU * m.GPU_HW_MAX)) < 1e-6,
          f"[F05] an unmanaged die is charged {m.GPU_HW_MAX:.0f} W (PL1 {three.pl1:.0f} vs {full.pl1:.0f} W)")
    for n in (1, 3, 4, 5):
        ss = m.static_safe(n, 0)
        cost = m.base() + m.K_GPU * n * ss["gpu"] + m.K_CPU * ss["pl1"]
        check(cost <= m.BUDGET_LIMIT - m.BUDGET_MARGIN + 1e-6, f"[F05] static-safe limits fit for {n} dies ({cost:.0f} W)")
    # [F06] PL2 that cannot be held is charged as read back; RAPL off charges the hardware maximum
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.pl2_held = False
    b.cpu_charge(413e6, True)
    check(b.cost() <= b.limit + 1e-6 and abs(b.cpu_w() - 413) < 1 and max(b.gpu.values()) < 125,
          f"[F06] an unheld PL2 of 413 W is charged in full (GPUs {[round(v) for v in b.gpu.values()]})")
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.cpu_charge(None, False)
    check(abs(b.cpu_w() - m.CPU_HW_MAX) < 1 and b.cost() <= b.limit + 1e-6,
          "[F06] with RAPL not enforcing, the CPU is charged CPU_HW_MAX")
    check(not hasattr(m.Budget, "apply") and not hasattr(m.Budget, "external"),
          "[F07] the budget does no I/O (no apply/external): the fan loop never touches a power-limit file")


def test_backstop_pin(m):
    m._BASE = m.BASE
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.pin = (150.0, None)
    b.rebalance(90.0, {a: 140.0 for a in DIES}, 50.0)
    for _ in range(m.BACKSTOP_N):
        b.backstop_check(m.BACKSTOP + 20, 100.0)
    b.end_pin(102.0)
    b.pin = None
    held = all(v == m.GPU_CAP["floor"] for v in b.gpu.values()) and b.pl1 == m.CPU_PL1["floor"]
    b.rebalance(150.0, {a: 80.0 for a in DIES}, 20.0)
    still = all(v == m.GPU_CAP["floor"] for v in b.gpu.values())
    msg = b.rebalance(170.0, {a: 80.0 for a in DIES}, 20.0)
    check(held and still and msg and all(v == m.GPU_CAP["init"] for v in b.gpu.values()),
          "[F10] a pin that ends during the backstop hold leaves every limit at its floor until the hold ends")


def test_locks(m):
    d = tempfile.mkdtemp()
    m.RUN_DIR, m.OWNER_LOCK, m.SMC_LOCK = d, d + "/owner.lock", d + "/smc.lock"
    fd = m.open_lock(m.OWNER_LOCK)
    st = os.fstat(fd)
    check(oct(st.st_mode & 0o777) == "0o600" and st.st_uid == os.geteuid(),
          "[F08/R10] lock files are private (0600, owned by the running user: root for the daemon)")
    os.symlink("/etc/hostname", d + "/evil.lock")
    try:
        m.open_lock(d + "/evil.lock")
        sym = False
    except OSError:
        sym = True
    check(sym, "[F08/L03] a lock path that is a symlink is refused")
    check(m.flock_within(fd, 0), "[F14] the first owner gets the lock")
    fd2 = m.open_lock(m.OWNER_LOCK)
    check(not m.flock_within(fd2, 0), "[F14] a second instance cannot take the owner lock")
    # bounded SMC constructor while another holder keeps the lock
    m.rds = lambda p: "5" if p.endswith("key_count") else "XXXX"
    m.wr = lambda p, v: None
    holder = m.open_lock(m.SMC_LOCK)
    m.flock_within(holder, 0)
    t0 = time.monotonic()
    k = m.SmcKey("/virtual/smc", "PZ0G")
    dt = time.monotonic() - t0
    check(k.idx is None and dt < 3.0, f"[F08] SMC key lookup gives up after {dt:.1f} s while the lock is held")


def test_config(m):
    def errs(text):
        with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
            f.write(text)
        e = m.load_config(f.name)
        os.unlink(f.name)
        return e
    bad = {'ZONES = {4: {"floor": "slow"}}': "nested type", "CPU_TARGET = ((133.0, 48.0),)": "one-point CPU target",
           'DIES = {"0b:00.0": 9}': "bay 9", "K_CPU = 0": "zero coefficient", "BUDGET_LIMIT = 1e309": "infinity",
           "BAY_TARGET = True": "bool", "BASE = 1100": "infeasible floors (on any inventory)", 'GPU_CAP = {"floor": 150, "max": 100}': "floor > max"}
    before = (m.ZONES[4]["floor"], m.K_CPU, m.BASE, m.BAY_TARGET)
    for text, what in bad.items():
        check(bool(errs(text)), f"[F09] rejected: {what}")
    check((m.ZONES[4]["floor"], m.K_CPU, m.BASE, m.BAY_TARGET) == before, "[F09] nothing was committed from a bad config")
    check(not errs('GPU_CAP = {"max": 100}') and m.static_safe(4, 0)["gpu"] <= 100
          and all(v <= 100 for v in m.Budget({a: f"/hw/{a}" for a in DIES}).gpu.values()),
          "[F09] a lower GPU maximum also bounds the start allocation and the static-safe caps")


def test_rapl(m):
    r = m.energy_rate
    check(r([100.0, None], 102.0, 3e6, 2**32) is None and r([100.0, 1e6], 102.0, None, 2**32) is None,
          "[F11] a missing RAPL reading gives no power value instead of an exception")
    check(abs(r([100.0, 1e6], 102.0, 3e6, 2**32) - 1.0) < 1e-9, "[F11] RAPL power from two readings")
    check(abs(r([100.0, 2**32 - 1e6], 102.0, 1e6, 2**32) - 1.0) < 1e-9, "[F11] a wrapped energy counter is unwrapped")
    check(m.rd("/nonexistent") is None, "rd: a missing file is None")


def test_nonfinite(m):
    f = m.Filter(4.0)
    f.feed(40.0, 2.0)
    f.feed(float("nan"), 2.0)
    check(f.val == 40.0 and f.fails == 1, "[L02] a NaN reading counts as a failed read and leaves the filter as it was")
    d = tempfile.mkdtemp()
    m.OVERRIDE = d + "/override"
    res = {}
    for text in ("auto gpu=nan", "auto pl1=nan", "max ttl=nan", "hold 700 nan 1500 1500", "auto gpu=inf"):
        with open(m.OVERRIDE, "w") as f2:
            f2.write(text)
        res[text] = m.parse_override()
    check(res["auto gpu=nan"][3] is None and res["auto pl1=nan"][3] is None and res["max ttl=nan"][0] == "auto"
          and res["hold 700 nan 1500 1500"][0] == "auto" and res["auto gpu=inf"][3] is None,
          f"[L02] override values that are not finite are ignored ({[r[0] + ' ' + str(r[3]) for r in res.values()]})")
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.pin = (res["auto gpu=nan"][3] or (None, None))[0], None
    b.rebalance(10.0, {a: 80.0 for a in DIES}, 20.0)
    check(all(math.isfinite(v) for v in b.gpu.values()) and b.cost() <= b.limit + 1e-6,
          "[L02] the allocation stays finite and inside the budget")


def test_gpu_reader(m):
    done = threading.Event()

    class D:
        def __init__(self, a, delay):
            self.addr, self.delay = a, delay

        def read(self):
            if done.is_set():
                raise SystemExit                  # ends the reader thread once the test is over
            time.sleep(self.delay)
            return (40.0, 40.0, 30.0)
    m.PERIOD = 0.01
    r = m.GpuReader([D("a", 0.0), D("b", 0.6)])
    r.start()
    time.sleep(0.75)
    lat = r.latest()
    done.set()
    check(lat["a"][1] < 0.3 and lat["b"][1] > 0.65,
          f"[F12] each die's age counts from the start of its own read: a slow die is not made fresh by the "
          f"scan (a {lat['a'][1]:.2f} s, b {lat['b'][1]:.2f} s)")


def stop_hook(m, gpu_w, pl1_w, hang=None, missing=0, deadline=None, rapl=(True, True), pl2_w=None, fail=()):
    """The real write_static_safe() against fake limits; returns (result, hw, seconds, log lines)."""
    d = tempfile.mkdtemp()
    m.RAPL_PL1, m.RAPL_PL2 = d + "/pl1", d + "/pl2"
    open(m.RAPL_PL2, "w").close()
    hw = Hw(m, gpu_w, pl1_w, pl2_w)
    hw.fail_write = set(m.RAPL_PL2 if f == "pl2" else f for f in fail)
    found = {a: f"/hw/{a}/power1_cap" for a in DIES[missing:]}
    m.die_map = lambda: {a: 1 for a in DIES}
    m.cap_paths_now = lambda: (found, DIES[:missing])
    m.rapl_state = lambda: rapl
    m.rd = lambda p, scale=1.0: hw.read(p)

    def wr(p, v):
        if p == hang:
            threading.Event().wait()          # a driver call that never returns
        if not hw.write(p, v):
            raise OSError(5, "EIO")
    m.wr = wr
    logs = []
    m.log = lambda **kv: logs.append(kv)
    if deadline:
        m.STOP_DEADLINE = deadline
    t0 = time.monotonic()
    res = m.write_static_safe()
    return res, hw, time.monotonic() - t0, logs


def test_stop_hook():
    m = load()
    lim = m.BUDGET_LIMIT - m.BUDGET_MARGIN
    res, hw, _, logs = stop_hook(m, 190, 90)
    check(res and hw.model() <= lim + 1 and hw.peak <= max(Hw(m, 190, 90).model(), hw.model()) + 1e-6
          and logs and logs[-1]["static_safe"]["ok"],
          f"[F01] the stop hook lowers the GPUs before raising PL1 and confirms it ({hw.model():.0f} W, peak {hw.peak:.0f} W)")
    m = load()
    res, hw, dt, logs = stop_hook(m, 190, 90, hang="/hw/0b:00.0/power1_cap", deadline=0.5)
    check(res is False and dt < 1.5 and hw.v[m.RAPL_PL1] == 90e6 and hw.peak <= Hw(m, 190, 90).model() + 1e-6,
          f"[F01] a GPU write that hangs in the stop hook: gives up after {dt:.1f} s, PL1 is not raised")
    m = load()
    res, hw, _, logs = stop_hook(m, 125, 362, missing=1)
    model = (m.base() + m.K_GPU * (sum(hw.v[p] for p in hw.caps[1:]) / 1e6 + m.GPU_HW_MAX)
             + m.K_CPU * max(hw.v[m.RAPL_PL1], hw.v[m.RAPL_PL2]) / 1e6)
    check(res and model <= lim + 1 and logs[-1]["static_safe"]["unmanaged_dies"] == DIES[:1],
          f"[F05] the stop hook charges a die without hwmon at {m.GPU_HW_MAX:.0f} W ({model:.0f} W)")


def test_cli():
    """Only option parsing and configuration checks: every case exits before any hardware access."""
    import subprocess
    conf = os.path.join(os.path.dirname(os.path.abspath(SCRIPT)), "macpro-fand.conf")

    # the daemon runs in a subprocess, with the same inventory fixture as load() (not the build host's)
    boot = ("import importlib.machinery, importlib.util, sys; "
            f"l = importlib.machinery.SourceFileLoader('mf', {SCRIPT!r}); "
            "m = importlib.util.module_from_spec(importlib.util.spec_from_loader('mf', l)); l.exec_module(m); "
            "m.die_map = lambda: {'0b:00.0': 1, '0e:00.0': 1, '1b:00.0': 2, '1e:00.0': 2}; "
            "m.devices = lambda: {'mellanox': 0, 'nvme': 0, 'disk': 0}; "
            f"sys.argv = [{SCRIPT!r}] + sys.argv[1:]; m.main()")

    def run(*a):
        return subprocess.run([sys.executable, "-c", boot, *a], capture_output=True, text=True, timeout=30).returncode
    check(run("--dry-run", "--release") == 2 and run("--stopped", "--dry-run") == 2,
          "[L01] modes that contradict each other are refused")
    with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
        f.write("K_CPU = 0\n")
    bad = run("--check-config", "--config", f.name)
    os.unlink(f.name)
    check(bad == 78 and (not os.path.exists(conf) or run("--check-config", "--config", conf) == 0),
          f"[F09] --check-config: a bad file exits 78, the shipped file passes (bad: {bad})")


def test_cpu_fallback():
    """[R02] An uncontrolled CPU (PL2 not held at PL1, RAPL not enforcing) is charged in every path."""
    m = load()
    m._BASE = m.BASE = 200.0                      # 73 W of extra devices beyond the default base
    m.GPU_CAP["max"] = 125
    lim = m.BUDGET_LIMIT - m.BUDGET_MARGIN
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.pl2_held = False
    b.cpu_charge(413e6, True)
    costs = []
    for pin in ((125.0, None), (None, 300.0), (125.0, 60.0)):
        b.pin = pin
        b.rebalance(100.0, {a: 80.0 for a in DIES}, 20.0)
        costs.append(b.cost())
    b.pin = None
    b.end_pin(200.0)
    costs.append(b.cost())
    eff = lambda w: m.base() + m.K_GPU * sum(v for p, v in w.items() if p.endswith("power1_cap")) / 1e6 + \
        m.K_CPU * max(w[m.RAPL_PL1] / 1e6, 413.0)
    check(all(c <= lim + 1e-6 for c in costs) and eff(b.want()) <= lim + 1e-6,
          f"[R02] pins (gpu, pl1, both) and the reset after a pin keep a 413 W unheld PL2 inside the budget "
          f"({[round(c) for c in costs]} W)")
    res, hw, _, logs = stop_hook(m, 97, 413, rapl=(False, True))
    actual = m.base() + m.K_GPU * sum(hw.v[p] for p in hw.caps) / 1e6 + m.K_CPU * 413.0
    check(res and actual <= lim + 1e-6,
          f"[R02] stop hook with RAPL not enforcing: GPUs make room for the CPU's 413 W ({actual:.0f} W)")
    m = load()
    m._BASE = m.BASE = 200.0
    m.GPU_CAP["max"] = 125
    res, hw, _, logs = stop_hook(m, 125, 300, pl2_w=413, fail=("pl2",))
    actual = m.base() + m.K_GPU * sum(hw.v[p] for p in hw.caps) / 1e6 + m.K_CPU * max(hw.v[m.RAPL_PL1],
                                                                                     hw.v[m.RAPL_PL2]) / 1e6
    check(res and actual <= lim + 1e-6 and hw.peak <= max(Hw(m, 125, 300, 413).model(), actual) + 1e-6,
          f"[R02] stop hook when PL2 will not follow PL1: charged as read (413 W), GPUs lowered ({actual:.0f} W)")
    # an unheld PL2 stays watched: an outside change is seen fresh, and a stale observation charges CPU_HW_MAX
    m = load()
    m.RECONCILE = 0.05
    vals = {"/hw/x/power1_cap": 125e6, m.RAPL_PL1: 300e6, m.RAPL_PL2: 200e6}
    m.rd = lambda p, scale=1.0: vals.get(p)
    m.wr = lambda p, v: vals.__setitem__(p, float(v))
    w = m.PowerWorker()
    w.start()
    w.set({"/hw/x/power1_cap": 125e6, m.RAPL_PL1: 300e6}, (m.RAPL_PL2,))
    time.sleep(0.2)
    vals[m.RAPL_PL2] = 413e6
    time.sleep(0.3)
    st = w.status()
    w.stop(1.0)
    w2 = m.PowerWorker()                           # no watch given: PL2 is watched anyway while PL1 is managed alone
    w2.start()
    w2.set({"/hw/x/power1_cap": 125e6, m.RAPL_PL1: 300e6})
    time.sleep(0.3)
    auto = m.pl2_observed(w2.status(), time.monotonic())
    w2.stop(1.0)
    check(auto == 413e6, "[R02] an unmanaged PL2 is read every cycle even when no watch is asked for")
    seen_now = m.pl2_observed(st, time.monotonic())
    stale = m.pl2_observed(st, time.monotonic() + m.PL2_FRESH + 1)
    b = m.Budget({a: f"/hw/{a}" for a in DIES})
    b.pl2_held = False
    b.cpu_charge(stale, True)
    check(seen_now == 413e6 and stale is None and b.cpu_w() == m.CPU_HW_MAX,
          "[R02] an unheld PL2 is still read every cycle (outside change to 413 W seen); a stale reading charges "
          "CPU_HW_MAX")


def test_stop_vs_worker():
    """[R03/Q01] The stop goes through the worker: nothing it had in progress can land after the static-safe
    limits, and a worker stuck in the driver means no competing transaction."""
    for release_in, label in ((None, "stuck"), (0.2, "released")):
        m = load()
        m._BASE = m.BASE = 200.0
        m.GPU_CAP["max"] = 125
        m.RECONCILE = 1000.0
        d = tempfile.mkdtemp()
        m.RAPL_PL1, m.RAPL_PL2 = d + "/pl1", d + "/pl2"
        open(m.RAPL_PL2, "w").close()
        lim = m.BUDGET_LIMIT - m.BUDGET_MARGIN
        hw = Hw(m, 125, math.floor((lim - 200 - m.K_GPU * 500) / m.K_CPU))
        gate, blocked, calls = threading.Event(), threading.Event(), []

        def wr(p, v):
            if threading.current_thread().name == "power-worker" and p == m.RAPL_PL2 and v == 413e6:
                blocked.set()
                gate.wait(5)
            calls.append((threading.current_thread().name, p, v))
            hw.write(p, v)
        m.wr, m.rd = wr, (lambda p, scale=1.0: hw.read(p))
        m.cap_paths_now = lambda: ({a: f"/hw/{a}/power1_cap" for a in DIES}, [])
        m.rapl_state = lambda: (True, True)
        m.log = lambda **kv: None
        w = m.PowerWorker()
        w.start()
        w.set(want(m, 85, 413))                   # lowers the GPUs, then hangs raising PL2 to 413 W
        blocked.wait(2)
        if release_in:
            threading.Timer(release_in, gate.set).start()
        idle = w.stop(1.0)
        static = m.write_static_safe() if idle else None
        n_static = len(calls)
        gate.set()
        time.sleep(0.3)
        late = [c for c in calls[n_static:] if c[0] == "power-worker"
                and (c[1], c[2]) != (m.RAPL_PL2, 413e6)]      # the write stuck in the driver itself may land
        cpu = max(hw.v[m.RAPL_PL1], hw.v[m.RAPL_PL2]) / 1e6
        model = m.base() + m.K_GPU * sum(hw.v[p] for p in hw.caps) / 1e6 + m.K_CPU * cpu
        if label == "stuck":
            check(not idle and static is None and not late and model <= lim + 1e-6,
                  f"[R03] stop with the worker stuck mid-write: no competing transaction; only the stuck write "
                  f"lands, nothing after it ({model:.0f} W)")
        else:
            check(idle and static and not late and model <= lim + 1e-6,
                  f"[R03] stop after the worker's write completes: static-safe confirmed and no older write lands "
                  f"after it ({model:.0f} W)")
    # write_static_safe() itself takes the limits from a live worker first (whoever calls it)
    m = load()
    m.RECONCILE = 1000.0
    d = tempfile.mkdtemp()
    m.RAPL_PL1, m.RAPL_PL2 = d + "/pl1", d + "/pl2"
    open(m.RAPL_PL2, "w").close()
    hw = Hw(m, 125, 362)
    gate, blocked, calls = threading.Event(), threading.Event(), []

    def wr(p, v):
        if threading.current_thread().name == "power-worker" and p == m.RAPL_PL2 and v == 413e6:
            blocked.set()
            gate.wait(5)
        calls.append((threading.current_thread().name, p, v))
        hw.write(p, v)
    m.wr, m.rd = wr, (lambda p, scale=1.0: hw.read(p))
    m.cap_paths_now = lambda: ({a: f"/hw/{a}/power1_cap" for a in DIES}, [])
    m.rapl_state = lambda: (True, True)
    m.log = lambda **kv: None
    m.STOP_DEADLINE = 0.5
    w = m.PowerWorker()
    w.start()
    w.set(want(m, 85, 413))
    blocked.wait(2)
    res = m.write_static_safe()                    # called directly while the worker hangs mid-transaction
    n = len(calls)
    gate.set()
    time.sleep(0.3)
    late = [c for c in calls[n:] if c[0] == "power-worker" and (c[1], c[2]) != (m.RAPL_PL2, 413e6)]
    check(res is False and not [c for c in calls[:n] if c[0] != "power-worker"] and not late,
          "[R03] write_static_safe() called beside a stuck worker refuses (no competing writes, nothing lands after)")
    m = load()
    called = []
    done, _ = m.io_call(lambda: called.append(1), time.monotonic() - 1)
    time.sleep(0.05)
    check(not done and not called, "[Q01] an operation whose deadline has passed is never started")


def test_config_recovery():
    """[R04/R05/R06] Unsafe and malformed configurations are reported, never raise, and never stop the stop hook;
    the stop hook leaves another owner alone."""
    m = load()
    for cand, what in (({"GPU_CAP": {"floor": 85, "max": 125, "init": 0}}, "GPU init 0"),
                       ({"GPU_CAP": {"floor": 85, "max": 125, "init": 50}}, "GPU init below the floor"),
                       ({"STATIC_SAFE": {"gpu": 60, "pl1": None}}, "static GPU below the floor"),
                       ({"CPU_PL1": {"floor": 0, "max": 413}}, "PL1 floor 0")):
        check(bool(m.check_config(cand)), f"[R04] rejected: {what}")
    bad = ["DISK_ZONE = []", "NVME = ([], 55.0, 1.0)", 'SMC = {"TPCD": ([], 45.7, 90.0, 1.0)}', 'SLOT_ZONE = {"5": []}',
           "ZONES = {[]: {}}", "BUDGET_LIMIT = " + "9" * 400, "K_GPU = 1e999", "ZONES = {4: {\"floor\": [1]}}"]
    for text in bad + [b"BASE = 1\xff\xfe"]:
        mode = "wb" if isinstance(text, bytes) else "w"
        with tempfile.NamedTemporaryFile(mode, suffix=".conf", delete=False) as f:
            f.write(text)
        try:
            errs, exc = m.load_config(f.name), None
        except Exception as e:                     # noqa: BLE001
            errs, exc = [], e
        res = run_stopped(load(), f.name)
        os.unlink(f.name)
        label = text[:40] if isinstance(text, str) else "non-UTF-8 bytes"
        check(exc is None and errs and res["exc"] is None and res["fans_full"] and res["static_ok"],
              f"[R05] malformed config ({label!r}): errors reported, no exception; the stop hook still recovers")
    res = run_stopped(load(), "/nonexistent.conf", other_owner=True)
    check(res["exc"] is None and not res["writes"] and not res["static_called"],
          "[R06] the stop hook leaves another macpro-fand's fans and limits alone (owner lock held)")
    res = run_stopped(load(), "/nonexistent.conf")
    check(res["exc"] is None and res["fans_full"] and res["static_ok"],
          "[R06] the stop hook of the owner itself recovers (owner lock free)")


def test_round3():
    """[V02/V03/V04/N01-N03] Third review: infeasible static recovery, PL2 read age, fans before the stop wait, and
    the small input checks."""
    # V02: GPU caps gone (charged 200 W each) and a raised CPU floor: the static target cannot fit; the CPU pin of
    # 40 W must not be raised to it
    m = load()
    m._BASE = m.BASE = 127.0
    m.CPU_PL1["floor"] = 160
    m.GPU_CAP.update(floor=85, max=200, init=125)
    res, hw, _, logs = stop_hook(m, 200, 40, missing=4)
    unmanaged = m.base() + m.K_GPU * 4 * m.GPU_HW_MAX
    model = unmanaged + m.K_CPU * max(hw.v[m.RAPL_PL1], hw.v[m.RAPL_PL2]) / 1e6
    ss = logs[-1].get("static_safe", {}) if logs else {}
    check(res is False and hw.v[m.RAPL_PL1] == 40e6 and hw.v[m.RAPL_PL2] == 40e6 and model <= 1137.25
          and ss.get("fits") is False,
          f"[V02] infeasible static plan (fits False): the 40 W CPU pin is not raised ({model:.2f} W), recovery "
          f"reported incomplete")
    # V03: a PL2 value read before a slow operation keeps its own read time when its write then fails
    m = load()
    m.RECONCILE, m.PL2_FRESH = 1000.0, 0.3
    cap = "/hw/x/power1_cap"
    vals = {cap: 150e6, m.RAPL_PL1: 300e6, m.RAPL_PL2: 200e6}

    def wr(p, v):
        if p == cap:
            time.sleep(0.6)                        # a slow driver call
        if p == m.RAPL_PL2:
            raise OSError(5, "EIO")                # PL2 will not follow PL1
        vals[p] = float(v)
    m.rd, m.wr = (lambda p, scale=1.0: vals.get(p)), wr
    w = m.PowerWorker()
    w.start()
    w.set({cap: 125e6, m.RAPL_PL1: 300e6, m.RAPL_PL2: 300e6})
    end = time.monotonic() + 3
    while w.status()["ok"] is None and time.monotonic() < end:
        time.sleep(0.05)
    st = w.status()
    w.stop(1.0)
    check(st["failed"] == m.RAPL_PL2 and m.pl2_observed(st, time.monotonic()) is None
          and st["seen_t"][m.RAPL_PL2] < st["seen_t"][cap] - 0.5,
          "[V03] a PL2 reading keeps the time it was read: after a slow transaction it is stale, not relabeled fresh")
    # V04: fans are at their caps before the shutdown waits for the power worker
    m = load()
    seen_at_wait = []
    orig = m.PowerWorker.stop

    def stop(self, timeout):
        seen_at_wait.append({f: m.rd(f"/virtual/smc/fan{f}_output") for f in m.CAP})
        return orig(self, timeout)
    m.PowerWorker.stop = stop
    exc, logs, events, regs, power_io = run_main(m, {"budget": True, "loops": 3})
    check(exc is None and seen_at_wait and all(v == m.CAP[f] for f, v in seen_at_wait[0].items()),
          f"[V04] fans already at their caps when the stop waits for the power worker ({seen_at_wait[:1]})")
    # N01: a binary override is ignored, not a crash
    m = load()
    d = tempfile.mkdtemp()
    m.OVERRIDE = d + "/override"
    open(m.OVERRIDE, "wb").write(b"\xff\xfe max")
    try:
        r, exc = m.parse_override(), None
    except Exception as e:                         # noqa: BLE001
        r, exc = None, e
    check(exc is None and r[0] == "auto" and r[3] is None, f"[N01] a binary override is ignored, no exception ({r})")
    # N02/N03: PERIPH keys must be driver names; limits are whole watts
    check(bool(m.check_config({"PERIPH": {3: (60.0, 1.0, 80.0)}})), "[N02] a PERIPH key that is not a driver name is rejected")
    check(bool(m.check_config({"GPU_CAP": {"floor": 85.5, "max": 125, "init": 125}}))
          and bool(m.check_config({"CPU_PL1": {"floor": 80, "max": 412.5}}))
          and not m.check_config({"GPU_CAP": {"floor": 85.0, "max": 125, "init": 125}}),
          "[N03] fractional power limits are rejected (whole watts; 85.0 is fine)")
    check(bool(m.check_config({"STATIC_SAFE": {"gpu": 125.5, "pl1": None}}))
          and bool(m.check_config({"STATIC_SAFE": {"gpu": 125, "pl1": 300.5}}))
          and not m.check_config({"STATIC_SAFE": {"gpu": 125.0, "pl1": 300}}),
          "[N404] fractional static-safe limits are rejected too")


def test_runtime_feasibility():
    """[W01] The budget's feasibility is judged every loop, not once at start: when RAPL stops enforcing (the CPU is
    then charged 413 W) and even the floors no longer fit, the infeasible alarm (every fan at its cap) is up on every
    loop while it lasts -- with the worker's lower-only transaction confirmed -- and clears when RAPL is back."""
    m = load()
    m._BASE, m.BASE = None, 700.0
    m.GPU_CAP.update(floor=125, max=125, init=125)
    sc = {"budget": True, "loops": 16, "rapl_enabled": lambda n: "0" if 4 <= n < 10 else "1"}
    exc, logs, events, regs, power_io = run_main(m, sc)
    notes = [e[1]["budget_note"] for e in events if e[0] == "log" and "budget_note" in e[1]]
    bad = [l for l in logs if any("budget infeasible" in a for a in l["alarms"])]
    caps = all(all(v == m.CAP[int(f)] for f, v in l["rpm"].items()) for l in bad)
    infeasible_idx = next((i for i, n in enumerate(notes) if n.startswith("budget infeasible")), None)
    back_idx = next((i for i, n in enumerate(notes) if n == "budget feasible again"), None)
    check(exc is None and len(bad) >= 3 and caps and infeasible_idx is not None and back_idx is not None
          and back_idx > infeasible_idx,
          f"[W01] RAPL lost mid-run: infeasible alarm on {len(bad)} loops with every fan at its cap; cleared when RAPL "
          f"is back ({'yes' if back_idx is not None else 'no'})")


def run_stopped(m, conf, other_owner=False):
    """main --stopped against fakes: returns what it did."""
    d = tempfile.mkdtemp()
    m.RUN_DIR, m.OWNER_LOCK, m.SMC_LOCK = d, d + "/owner.lock", d + "/smc.lock"
    holder = None
    if other_owner:
        holder = m.open_lock(m.OWNER_LOCK)
        m.flock_within(holder, 0)
    m.RAPL_PL1, m.RAPL_PL2 = d + "/pl1", d + "/pl2"
    open(m.RAPL_PL2, "w").close()
    hw = Hw(m, 125, 362)
    writes, logs, called = [], [], []
    m.find_smc = lambda: "/virtual/smc"
    m.cap_paths_now = lambda: ({a: f"/hw/{a}/power1_cap" for a in DIES}, [])
    m.die_map = lambda: {a: 1 for a in DIES}
    m.rapl_state = lambda: (True, True)
    m.rd = lambda p, scale=1.0: hw.read(p)

    def wr(p, v):
        writes.append((p, v))
        if not p.startswith("/virtual/smc"):
            hw.write(p, v)
    m.wr = wr
    m.log = lambda **kv: logs.append(kv)
    m.unit_exists = lambda u: False
    orig = m.write_static_safe
    m.write_static_safe = lambda: (called.append(1), orig())[1]
    exc = None
    with patch.object(sys, "argv", [SCRIPT, "--stopped", "--config", conf]):
        try:
            m.main()
        except BaseException as e:                 # noqa: BLE001
            exc = f"{type(e).__name__}: {e}"
    if holder is not None:
        os.close(holder)
    full = {p: v for p, v in writes if p.endswith("_output")}
    ss = [x["static_safe"] for x in logs if "static_safe" in x]
    return {"exc": exc, "writes": writes, "static_called": bool(called),
            "fans_full": len(full) == 4 and all(v == m.CAP[int(p.split("fan")[1][0])] for p, v in full.items()),
            "static_ok": bool(ss and ss[-1]["ok"])}


def test_rounding():
    """[R07] Fractional allocations are written rounded down: the limits never cost more than the allocation."""
    m = load()
    lim = m.BUDGET_LIMIT - m.BUDGET_MARGIN
    worst = -1e9
    for g in (85.4, 99.99, 113.51, 124.7, 131.2):
        b = m.Budget({a: f"/hw/{a}/power1_cap" for a in DIES})
        b.gpu = {a: g for a in DIES}
        b.pl1 = 1000.0
        b.fit()
        w = b.want()
        eff = m.base() + m.K_GPU * sum(v for p, v in w.items() if p.endswith("power1_cap")) / 1e6 + \
            m.K_CPU * w[m.RAPL_PL1] / 1e6
        worst = max(worst, eff - lim)
    check(worst <= 1e-6, f"[R07] written limits never exceed the fitted allocation (worst {worst:+.2f} W vs the budget)")


def test_gpu_alarm_and_filter():
    """[R08/R09] A GPU read-failure alarm stays up on repeated (cached) samples until a good reading, even under a
    hold override; filters integrate over the time between samples, not the loop's."""
    hold = ("hold", {1: 600, 2: 500, 3: 500, 4: 500}, "override hold", None)
    reader = lambda n: ((None, None, None), 0.0, n) if n <= 3 else ((None, None, None), 2.0 * (n - 3), 3)
    sc = {"reader": reader, "loops": 6, "override": hold}
    m = load()
    exc, logs, events, outs = run_main(m, sc)
    persist = [("read 0b:00.0" in l["alarms"]) for l in logs[2:]]
    check(exc is None and len(persist) >= 3 and all(persist) and all(v == m.CAP[int(p.split("fan")[1][0])]
                                                                     for p, v in outs.items()),
          f"[R08] read alarm persists on cached samples under a hold: {persist}, fans at caps")
    vals = {1: (40.0, 40.0, 30.0), 5: (80.0, 80.0, 30.0)}
    reader = lambda n: (vals[1], 2.0 * (n - 1), 1) if n < 5 else (vals[5], 0.0, 2)
    sc = {"reader": reader, "loops": 5}
    m = load()
    exc, logs, events, outs = run_main(m, sc)
    d = sc["dies"][0]
    expect = m.ema(40.0, 80.0, 8.0, 4.0)
    check(exc is None and d.tj.val is not None and abs(d.tj.val - expect) < 0.01,
          f"[R09] a sample arriving 8 s after the last one is filtered over 8 s ({d.tj.val:.2f} vs {expect:.2f} C)")


def test_smc_retry():
    """[Q02] A key lookup that failed on lock contention is retried later, so measured DC comes back."""
    m = load()
    d = tempfile.mkdtemp()
    m.RUN_DIR, m.SMC_LOCK = d, d + "/smc.lock"
    keys = {"/virtual/smc/key_count": "3"}
    sel = [0]
    names = ["AAAA", "PZ0G", "BBBB"]
    m.rds = lambda p: keys.get(p) if p.endswith("key_count") else (names[sel[0]] if p.endswith("_name") else None)
    m.wr = lambda p, v: sel.__setitem__(0, int(v))
    holder = m.open_lock(m.SMC_LOCK)
    m.flock_within(holder, 0)
    k = m.SmcKey("/virtual/smc", "PZ0G")
    first = k.idx
    os.close(holder)
    k.next_try = 0.0
    k.read()
    if k.finder:
        k.finder.join(5)
    check(first is None and k.idx == 1, f"[Q02] SMC key lookup retried after the lock was released (idx {k.idx})")


def run_main(m, scenario):
    """The real main loop against fakes (adapted from the review's harness)."""
    events, registers, clock, loop = [], {}, [100.0], [0]
    power_io = []                             # (thread name, path) of every power1_cap read or write
    real_sleep = time.sleep
    smc = "/virtual/smc"
    energy = iter(scenario.get("energy", (1e6, 3e6, 5e6)))
    d = tempfile.mkdtemp()
    m.RUN_DIR, m.OWNER_LOCK, m.SMC_LOCK = d, d + "/owner.lock", d + "/smc.lock"

    class FakeDie:
        def __init__(self, addr, bay):
            self.addr, self.bay = addr, bay
            scenario.setdefault("dies", []).append(self)
            self.tj, self.tm, self.p = m.Filter(4.0), m.Filter(4.0), m.Filter(10.0)
            self.paths = ("/virtual/gpu/junction_input", "/virtual/gpu/mem_input", "/virtual/gpu/power")
            self.jcrit, self.mcrit = 105.0, 95.0

    class FakeReader:
        def __init__(self, dies):
            self.dies, self.n = dies, 0

        def start(self):
            pass

        def latest(self):
            self.n += 1
            if "reader" in scenario:                # (values, age, seq) per loop
                return {x.addr: scenario["reader"](self.n) for x in self.dies}
            return {x.addr: (scenario.get("gpu", (40.0, 40.0, 30.0)), 0.0, self.n) for x in self.dies}

    def rds(path):
        if path.startswith(m.RAPL_ZONE + "/"):
            return {"enabled": scenario.get("rapl_enabled", lambda n: "1")(loop[0]), "constraint_0_name": "long_term",
                    "constraint_1_name": "short_term"}.get(os.path.basename(path))
        if path.endswith("product_name"):
            return "MacPro7,1"
        if path.endswith("_manual"):
            return str(registers.get(path, 1))
        return "mock" if path.endswith("_label") else None

    def rd(path, scale=1.0):
        if path.endswith("power1_cap"):
            power_io.append((threading.current_thread().name, path))
        if path in (m.RAPL_PL1, m.RAPL_PL2) or path.endswith("power1_cap"):
            return float(registers.get(path, 200e6))
        if path == m.RAPL_E:
            return next(energy, 7e6)
        if path.endswith("_crit"):
            return 92.0
        if path.endswith("_output"):
            return float(registers.get(path, 0))
        if path.endswith("_input") and "/fan" in path:
            return float(registers.get(path.replace("_input", "_output"), 1200))
        return 30.0 if path.endswith("_input") else None

    def wr(path, val):
        if path.endswith("power1_cap"):
            power_io.append((threading.current_thread().name, path))
        registers[path] = val
        events.append(("write", path, val))

    def sleep(seconds):
        if threading.current_thread() is not threading.main_thread():
            return real_sleep(seconds)            # other threads (left by earlier tests) must not move the fake clock
        clock[0] += seconds
        if scenario.get("term_in_takeover") and seconds == 1.0:
            raise SystemExit(0)
        if seconds == m.PERIOD:
            if scenario.get("budget"):
                real_sleep(0.05)              # let the power worker run
            loop[0] += 1
            if loop[0] > scenario.get("loops", 3):
                raise KeyboardInterrupt
    m.load_config = lambda p: []
    m.rds, m.rd, m.wr = rds, rd, wr
    m.find_smc = lambda: smc
    m.die_map = lambda: {"0b:00.0": 1}
    m.Die, m.GpuReader = FakeDie, FakeReader
    m.hwmon_named = lambda name: ["/virtual/core"] if name == "coretemp" else []
    m.temp_labels = lambda p: {"Package id 0": "/virtual/core/temp1"} if p == "/virtual/core" else {k: smc + "/" + k for k in m.SMC}
    if scenario.get("budget"):
        d2 = tempfile.mkdtemp()
        m.RAPL_PL1, m.RAPL_PL2 = d2 + "/pl1", d2 + "/pl2"
        open(m.RAPL_PL2, "w").close()
        m.cap_paths_now = lambda: ({"0b:00.0": "/virtual/gpu/power1_cap"}, [])
    else:
        m.budget_ready = lambda: False
    m.under_systemd = lambda: False
    m.unit_active = lambda n: False
    m.unit_exists = lambda n: False
    m.parse_override = lambda: scenario.get("override", ("auto", None, "", None))
    m.notify = lambda msg: events.append(("notify", msg))
    if not scenario.get("budget"):
        m.write_static_safe = lambda: events.append(("static_safe",))
    m.log = lambda **kv: events.append(("log", kv))
    exc = None
    with patch.object(sys, "argv", [SCRIPT]), patch.object(m.time, "monotonic", lambda: clock[0]), \
            patch.object(m.time, "sleep", sleep), patch.object(m.signal, "signal", lambda *a: None):
        try:
            m.main()
        except KeyboardInterrupt:
            pass
        except BaseException as err:          # noqa: BLE001
            exc = f"{type(err).__name__}: {err}"
    logs = [e[1] for e in events if e[0] == "log" and "alarms" in e[1]]
    outs = {p: v for p, v in registers.items() if p.endswith("_output")}
    if scenario.get("budget"):
        return exc, logs, events, registers, power_io
    return exc, logs, events, outs


def test_main_loop():
    exc, logs, _, _ = run_main(load(), {"energy": (None, 3e6, 5e6)})
    check(exc is None and logs, f"[F11] a failed RAPL read followed by a valid one does not crash the loop ({exc})")
    m = load()
    exc, logs, _, outs = run_main(m, {"gpu": (104.0, 94.0, None), "loops": 1})
    first = logs[0]["alarms"] if logs else []
    check(any(a == "hot 0b:00.0" for a in first) and all(v == m.CAP[int(p.split("fan")[1][0])] for p, v in outs.items()),
          f"[F13] a die at its emergency temperature alarms on the first loop even without a power reading ({first})")
    exc, logs, events, outs = run_main(load(), {"term_in_takeover": True})
    check(exc == "SystemExit: 0" and any(e[0] == "static_safe" for e in events),
          "[F15] SIGTERM during takeover still runs the cleanup (static-safe limits, fans to caps)")
    m = load()
    exc, logs, events, regs, power_io = run_main(m, {"budget": True, "loops": 8})
    cap = "/virtual/gpu/power1_cap"
    ss = [e[1]["static_safe"] for e in events if e[0] == "log" and "static_safe" in e[1]]
    check(exc is None and logs and regs.get(cap) is not None and regs.get(m.RAPL_PL1) is not None,
          f"the main loop with the budget enabled runs and the power worker writes the limits ({exc})")
    check(power_io and all(name != "MainThread" for name, _ in power_io),
          f"[F07] no power1_cap read or write from the fan loop's thread "
          f"({sorted({name for name, _ in power_io})})")
    check(ss and ss[-1]["ok"] and regs[cap] == m.static_safe(1, 0)["gpu"] * 1e6,
          "the foreground stop writes and confirms the static-safe limits")


def main():
    def run(t, *a):
        """A test that raises fails by name (a crash is a failure of that test, not of the whole run)."""
        try:
            t(*a)
        except Exception as e:                     # noqa: BLE001
            check(False, f"[crash in {t.__name__}] {type(e).__name__}: {e}")
    for t in (test_transaction, test_worker, test_accounting, test_backstop_pin, test_locks, test_config, test_rapl,
              test_nonfinite, test_gpu_reader):
        run(t, load())
    for t in (test_unknown_combinations, test_cpu_fallback, test_stop_vs_worker, test_rounding, test_smc_retry,
              test_cli, test_stop_hook, test_config_recovery, test_main_loop, test_gpu_alarm_and_filter, test_round3,
              test_runtime_feasibility):
        run(t)
    print(f"\n{'FAILED: ' + str(len(failures)) if failures else 'all safety checks passed'}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
