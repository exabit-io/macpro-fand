#!/usr/bin/env python3
"""test_macpro_fand.py SCRIPT -- offline tests of macpro-fand: no hardware is read or written.

Loads the daemon as a module and drives its power budget and settings parser with simulated sensors:
  - the budget invariant holds after every reallocation, through idle, GPU-only, CPU-only and combined load, with and
    without extra uncapped devices in the base load;
  - CPU first: a saturated CPU gets its default share at once, every die gives, none gains;
  - the dies borrow only what the CPU does not use, and give it back;
  - the working-temperature ceilings hold a hot die near T_WORK and bring a hotter die lower;
  - override pins and the backstop;
  - the settings parser: defaults round-trip, dict settings merge, bad settings are ignored.
Exit status 0 when every check passes.
"""
import importlib.machinery
import importlib.util
import os
import sys
import tempfile

SCRIPT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "macpro-fand")
DIES = ["0b:00.0", "0e:00.0", "1b:00.0", "1e:00.0"]
BAY = {"0b:00.0": 1, "0e:00.0": 1, "1b:00.0": 2, "1e:00.0": 2}
failures = []


def load():
    loader = importlib.machinery.SourceFileLoader("macpro_fand", SCRIPT)
    spec = importlib.util.spec_from_loader("macpro_fand", loader)
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


class Box:
    """Four dies with a first-order thermal response (junction = air + R * W, tau 60 s) and a CPU drawing its demand
    up to its limit. One die runs hotter (higher R)."""

    def __init__(self, m, base):
        self.m = m
        m._BASE = base
        self.b = m.Budget({a: "/nonexistent" for a in DIES})
        self.R = {a: 0.29 for a in DIES}
        self.R["1e:00.0"] = 0.34
        self.J = {a: 30.0 for a in DIES}
        self.t = 0.0
        self.worst = 0.0
        self.jmax = 0.0

    def run(self, seconds, gpu_demand, cpu_demand):
        for _ in range(int(seconds / 2)):
            self.t += 2.0
            w = {a: min(self.b.gpu[a], gpu_demand) for a in DIES}
            for a in DIES:
                self.J[a] += (22 + self.R[a] * w[a] - self.J[a]) * 2.0 / 60.0
                self.jmax = max(self.jmax, self.J[a])
            self.b.thermal({a: (self.J[a], self.J[a] - 15, w[a], BAY[a]) for a in DIES}, {1: True, 2: True}, 2.0)
            if int(self.t) % 4 == 0:
                self.b.rebalance(self.t, w, min(cpu_demand, 0.99 * self.b.pl1))
                self.worst = max(self.worst, self.b.cost())
                if self.b.cost() > self.b.limit + 1e-6:
                    check(False, f"invariant broken at t={self.t}: cost {self.b.cost():.1f} > {self.b.limit}")
                    return


def test_configured_ceiling(m):
    """A configured GPU ceiling (125 W here, as a node running only memory-bound work would set) holds: no
    reallocation raises a die above it, however idle the CPU."""
    m._BASE = m.BASE
    m.GPU_CAP["max"] = 125
    b = m.Budget({a: "/nonexistent" for a in DIES})
    for t in range(4, 200, 4):
        b.rebalance(float(t), {a: 0.97 * b.gpu[a] for a in DIES}, 20.0)
    check(max(b.gpu.values()) <= 125.0 + 1e-6,
          f"a configured 125 W ceiling holds, CPU idle, dies saturated ({[round(v) for v in b.gpu.values()]})")


def test_budget(m):
    for extra in (0.0, 118.0):
        box = Box(m, m.BASE + extra)
        b = box.b
        check(abs(b.cost() - b.limit) < 1e-6 and all(v == m.GPU_CAP["init"] for v in b.gpu.values()),
              f"base {box.m._BASE:.0f}: start at the default allocation, exactly on the budget ({b.cost():.1f} W)")
        box.run(60, 20, 25)
        box.run(600, 300, 25)
        g0, p0 = dict(b.gpu), b.pl1
        check(all(abs(box.J[a] - m.T_WORK[0]) <= 2.0 for a in DIES),
              f"base {box.m._BASE:.0f}: GPUs flat out hold T_WORK +-2 C ({[round(box.J[a], 1) for a in DIES]})")
        check(b.gpu["1e:00.0"] < min(b.gpu[a] for a in DIES[:3]) - 5,
              f"base {box.m._BASE:.0f}: the hotter die runs at a lower cap ({[round(v) for v in b.gpu.values()]})")
        box.run(120, 300, 280)
        cpu_took = b.pl1 >= min(p0 + 1, m.CPU_PL1["max"]) or b.pl1 >= 0.95 * 280 / m.SAT_CPU
        check(cpu_took and all(b.gpu[a] <= g0[a] + 1 for a in DIES),
              f"base {box.m._BASE:.0f}: CPU first: PL1 {p0:.0f} -> {b.pl1:.0f} W, no die gained "
              f"({[round(v) for v in g0.values()]} -> {[round(v) for v in b.gpu.values()]})")
        check(b.pl1 >= 280 * m.SAT_CPU - 1,
              f"base {box.m._BASE:.0f}: a CPU drawing 280 W is not starved (PL1 {b.pl1:.0f})")
        g1 = dict(b.gpu)
        box.run(300, 300, 150)
        at_ceiling = all(b.gpu[a] >= b.tcap[a] - 1 for a in DIES)
        check((at_ceiling or sum(b.gpu.values()) > sum(g1.values()) + 5)
              and b.pl1 >= 150 * m.CPU_KEEP[0] + m.CPU_KEEP[1] - 1,
              f"base {box.m._BASE:.0f}: CPU at 150 W lends what the dies can use (at their ceilings: {at_ceiling}) "
              f"and keeps 1.2 x + 20 W (PL1 {b.pl1:.0f})")
        box.run(300, 20, 150)
        check(box.worst <= b.limit + 1e-6, f"base {box.m._BASE:.0f}: worst cost {box.worst:.1f} <= {b.limit}")
        check(box.jmax < m.T_WORK[0] + 8, f"base {box.m._BASE:.0f}: hottest junction {box.jmax:.1f} C")


def test_rapl_regulated_cpu(m):
    """With PL2 held at PL1, RAPL keeps the package near 90 % of PL1: that must count as saturated."""
    m._BASE = m.BASE
    b = m.Budget({a: "/nonexistent" for a in DIES})
    b.limit = 900.0
    b.reset()
    for t in range(4, 40, 4):                       # dies saturated, CPU idle: the dies borrow
        b.rebalance(float(t), {a: 0.97 * b.gpu[a] for a in DIES}, 30.0)
    p0 = b.pl1
    msg = b.rebalance(44.0, {a: 0.97 * b.gpu[a] for a in DIES}, 0.90 * b.pl1)
    check(msg is not None and "cpu wants" in msg and b.pl1 > p0,
          f"a CPU held at 90 % of PL1 by RAPL counts as saturated (PL1 {p0:.0f} -> {b.pl1:.0f}: {msg})")


def test_pins_backstop(m):
    m._BASE = m.BASE
    b = m.Budget({a: "/nonexistent" for a in DIES})
    b.pin = (150.0, None)
    b.rebalance(4.0, {a: 140.0 for a in DIES}, 50.0)
    check(all(v == 150.0 for v in b.gpu.values()) and b.cost() <= b.limit + 1e-6,
          f"gpu=150 pin: caps {[round(v) for v in b.gpu.values()]}, PL1 {b.pl1:.0f}, cost {b.cost():.0f}")
    b.pin = (200.0, 400.0)
    b.rebalance(8.0, {a: 190.0 for a in DIES}, 300.0)
    check(b.cost() <= b.limit + 1e-6, f"a pin that does not fit shrinks: cost {b.cost():.0f} <= {b.limit}")
    b.pin = None
    b.reset()
    check(all(v == m.GPU_CAP["init"] for v in b.gpu.values()), "pin removed: default allocation")
    fired = [b.backstop_check(m.BACKSTOP + 10, 100.0 + i) for i in range(m.BACKSTOP_N)]
    check(fired[-1] and all(v == m.GPU_CAP["floor"] for v in b.gpu.values()) and b.pl1 == m.CPU_PL1["floor"],
          f"backstop after {m.BACKSTOP_N} samples over {m.BACKSTOP:.0f} W: every limit at its floor")
    check(b.rebalance(110.0, {a: 85.0 for a in DIES}, 80.0) is None, "no reallocation while the backstop holds")


def test_level(m):
    g = {"a": 150.0, "b": 120.0, "c": 100.0}
    moved = m.level(g, list(g), 40.0, {k: 85.0 for k in g}, True)
    check(abs(moved - 40.0) < 1e-6 and abs(g["a"] - 115.0) < 0.01 and abs(g["b"] - 115.0) < 0.01 and g["c"] == 100.0,
          f"level down takes from the highest caps first: {g}")
    moved = m.level(g, list(g), 30.0, {k: 200.0 for k in g}, False)
    check(abs(moved - 30.0) < 1e-6 and all(abs(v - 120.0) < 0.01 for v in g.values()),
          f"level up gives to the lowest caps first, then evenly: {g}")


def test_config(m):
    def load(text):
        with tempfile.NamedTemporaryFile("w", suffix=".conf", delete=False) as f:
            f.write(text)
        errors = m.load_config(f.name)
        os.unlink(f.name)
        return errors
    good = ('# comment\nBAY_TARGET = 39\nT_WORK = (65.0, 1.0)  # trailing\nZONES = {4: {"floor": 700},\n'
            '         1: {"init": 650}}\n')
    errors = load(good)
    check(not errors and m.BAY_TARGET == 39.0 and m.T_WORK == (65.0, 1.0), f"settings parse ({errors})")
    check(m.ZONES[4]["floor"] == 700 and m.ZONES[4]["init"] == 500 and m.ZONES[1]["init"] == 650
          and m.ZONES[2]["init"] == 1150, "a dict setting merges entry by entry, across continuation lines")
    m.BAY_TARGET = 37.8
    errors = load(good + 'FOO = 1\nBASE = "x"\nK_GPU = (1,\n')
    check(len(errors) == 3 and m.BAY_TARGET == 37.8 and m.BASE == 127.0 and m.K_GPU == 1.205,
          f"any error: nothing is committed, every error reported ({len(errors)})")


def test_cpu_target(m):
    pts = [(None, 48.0), (100, 48.0), (180.5, 52.75), (228, 57.5), (400, 57.5)]
    check(all(abs(m.cpu_target(w) - t) < 0.01 for w, t in pts), "CPU target follows package power, flat outside")


def main():
    m = load()
    test_level(m)
    test_cpu_target(m)
    test_configured_ceiling(load())
    check(load().GPU_CAP["max"] == 200, "the default ceiling is Vega 20's own 200 W")
    test_budget(m)
    test_rapl_regulated_cpu(load())
    test_pins_backstop(load())
    test_config(load())
    print(f"\n{'FAILED: ' + str(len(failures)) if failures else 'all checks passed'}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
