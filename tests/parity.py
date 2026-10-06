#!/usr/bin/env python3
"""Differential policy checks against the byte-pinned, unmodified 1.0.6 source.

Only temporary files and the test-only Rust oracle are used. Any attempt by the
reference daemon to read real hardware or invoke a service fails the test.
"""
import copy
import hashlib
import importlib.util
import io
import json
import math
import pathlib
import random
import subprocess
import sys
import tempfile
import types

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "reference/v1.0.6/macpro-fand.py"
SOURCE_HASH = "4bb1f2e700249308333f63bf3b5fccc441e4dd2e90ab3941ae3ddc77a7adbc8f"
assert hashlib.sha256(SOURCE.read_bytes()).hexdigest() == SOURCE_HASH, "reference source changed"
spec = importlib.util.spec_from_file_location("reference", SOURCE)
py = importlib.util.module_from_spec(spec)
spec.loader.exec_module(py)
DEFAULTS = {k: copy.deepcopy(getattr(py, k)) for k in py.CONFIGURABLE}


def forbidden(*args, **kwargs):
    raise AssertionError(f"unexpected real hardware/service access: {args}")


py.rd = py.rds = py.wr = py.systemctl = forbidden
py.devices = lambda: {"mellanox": 0, "nvme": 0, "disk": 0}
py.die_map = lambda: {f"g{i}": 1 if i < 2 else 2 for i in range(4)}
py.TEST_BUDGET = py.TEST_BACKSTOP = 0
py.subprocess = types.SimpleNamespace(run=forbidden)
oracle = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/examples/oracle")
worker = subprocess.Popen([str(oracle)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, text=True, bufsize=1)
scratch = tempfile.TemporaryDirectory(prefix="macpro-fand-parity-")
config_file = pathlib.Path(scratch.name) / "config"
checks = 0
iterations = 0


def ask(request):
    worker.stdin.write(repr(request) + "\n")
    worker.stdin.flush()
    line = worker.stdout.readline()
    assert line, f"oracle exited: {worker.stderr.read()}"
    return json.loads(line)


def equal(actual, expected, where=""):
    global iterations
    iterations += 1
    if isinstance(expected, dict):
        assert actual.keys() == expected.keys(), (where, actual, expected)
        for k, v in expected.items():
            equal(actual[k], v, f"{where}/{k}")
    elif isinstance(expected, (list, tuple)):
        assert len(actual) == len(expected), (where, actual, expected)
        for i, v in enumerate(expected):
            equal(actual[i], v, f"{where}/{i}")
    elif isinstance(expected, bool) or expected is None or isinstance(expected, str):
        assert actual == expected, (where, actual, expected)
    else:
        assert isinstance(actual, (int, float)) and math.isclose(actual, expected, rel_tol=1e-10, abs_tol=1e-6), (where, actual, expected)


def configure(text="", count=4):
    for k, v in DEFAULTS.items():
        setattr(py, k, copy.deepcopy(v))
    py._BASE = None
    py.die_map = lambda: {f"g{i}": 1 if i < 2 else 2 for i in range(count)}
    config_file.write_text(text)
    cand, errors = py.read_config(str(config_file))
    errors += py.check_config(cand)
    if not errors:
        vars(py).update(cand)
    return errors


def snapshot(b):
    return {"gpu": b.gpu.copy(), "pl1": b.pl1, "tcap": b.tcap.copy(), "cpu_min": b.cpu_min,
            "cpu_w": b.cpu_w(), "cost": b.cost(), "feasible": b.cost() <= b.limit + 1e-6,
            "floor_until": b.floor_until, "reset_pending": b.reset_pending, "want": b.want()}


def budget_case(request):
    global checks
    assert not configure(request.get("config", ""), request["n"])
    py._BASE = request.get("base", 127)
    b = py.Budget({f"g{i}": f"/gpu/{i}" for i in range(request["n"])}, request.get("unmanaged", 0))
    if "limit" in request:
        b.limit = request["limit"]
        b.fit()
    expected = [snapshot(b)]
    for a in request["actions"]:
        kind = a["kind"]
        if kind == "charge": b.cpu_charge(a["pl2"], a["enforcing"])
        elif kind == "held": b.pl2_held = a["value"]
        elif kind == "degraded": b.degraded = a["value"]
        elif kind == "pin": b.pin = a["value"]
        elif kind == "rebalance": b.rebalance(a["now"], a["watts"], a["cpu"])
        elif kind == "backstop": b.backstop_check(a["dc"], a["now"])
        elif kind == "end_pin": b.end_pin(a["now"])
        elif kind == "reset": b.reset()
        elif kind == "thermal": b.thermal(a["dies"], a["full"], a["dt"])
        else: raise AssertionError(kind)
        expected.append(snapshot(b))
    equal(ask(request), expected, "budget")
    checks += 1


def transaction_case(request):
    global checks
    values = request["start"].copy()
    unread = set(request.get("unread", []))
    writes, states = [], []

    def read(p):
        if p in unread:
            unread.remove(p)
            return None
        if request.get("read_fail") == p:
            return None
        return values.get(p)

    def write(p, v):
        writes.append([p, v])
        if request.get("fail") == p:
            return False
        values[p] = v + request.get("round", 0)
        states.append(values.copy())
        return True

    ok, failed, seen = py.transact(request["want"], read, write, request.get("raise_ok", True))
    equal(ask(request), {"ok": ok, "failed": failed, "seen": seen, "writes": writes, "states": states}, "transaction")
    checks += 1


try:
    rng = random.Random(10602000)
    configurations = ["", "GPU_CAP = {'max':125}", "GPU_CAP = {'floor':125,'init':150}\nSTATIC_SAFE={'gpu':150}",
                      "CPU_PL1={'floor':10}\nGPU_CAP={'max':175}", "BASE = 200\nK_GPU=1.5\nK_CPU=1.3"]
    for config in configurations:
        for trial in range(80):
            count = rng.choice([0, 1, 2, 4])
            actions = []
            now = 0
            for i in range(50):
                now += rng.uniform(1, 10)
                kind = rng.randrange(12)
                if kind == 0: actions.append({"kind": "held", "value": rng.choice([True, False])})
                elif kind == 1: actions.append({"kind": "charge", "pl2": rng.choice([None, 80e6, 250e6, 413e6]), "enforcing": rng.choice([True, False])})
                elif kind == 2: actions.append({"kind": "pin", "value": rng.choice([None, (125, 10), (175, None), (None, 250)])})
                elif kind == 3: actions.append({"kind": "backstop", "dc": rng.choice([None, 500, 1200]), "now": now})
                elif kind == 4: actions.append({"kind": "degraded", "value": rng.choice([True, False])})
                elif kind == 5: actions.append({"kind": "end_pin", "now": now})
                elif kind == 6: actions.append({"kind": "reset"})
                elif kind == 7:
                    actions.append({"kind": "thermal", "dies": {f"g{j}": (rng.uniform(20, 95), rng.choice([None, rng.uniform(30, 95)]), rng.choice([None, rng.uniform(20, 200)]), 1 if j < 2 else 2) for j in range(count)},
                                    "full": {1: rng.choice([True, False]), 2: rng.choice([True, False])}, "dt": rng.uniform(0.1, 10)})
                else:
                    actions.append({"kind": "rebalance", "now": now, "watts": {f"g{j}": rng.choice([None, rng.uniform(0, 250)]) for j in range(count)}, "cpu": rng.choice([None, rng.uniform(0, 450)])})
            budget_case({"op": "budget", "config": config, "n": count, "base": rng.uniform(127, 327),
                         "unmanaged": rng.choice([0, 1, 2]), "actions": actions})
    # The phase-order assertion is tested on all unknown combinations, including
    # a CPU pin below its configured floor and floors raised above existing limits.
    configure()
    paths = sorted([f"g{i}" for i in range(4)] + [py.RAPL_PL1, py.RAPL_PL2])
    for start_gpu in [0, 10, 50, 85, 125, 200]:
        for start_cpu in [0, 10, 80, 200, 413]:
            for mask in range(64):
                start = {p: (start_cpu if p in [py.RAPL_PL1, py.RAPL_PL2] else start_gpu) * 1e6 for p in paths}
                want = {p: (188 if p in [py.RAPL_PL1, py.RAPL_PL2] else 125) * 1e6 for p in paths}
                transaction_case({"op": "transaction", "start": start, "want": want, "unread": [p for i, p in enumerate(paths) if mask & (1 << i)]})
    for i in range(400):
        start = {p: rng.uniform(0, 450) * 1e6 for p in paths}
        want = {p: rng.randint(10, 413) * 1e6 for p in paths}
        request = {"op": "transaction", "start": start, "want": want, "raise_ok": rng.choice([True, False]), "unread": rng.sample(paths, rng.randint(0, len(paths)))}
        if i % 3 == 0: request["fail"] = rng.choice(paths)
        if i % 5 == 0: request["read_fail"] = rng.choice(paths)
        if i % 7 == 0: request["round"] = rng.choice([-1e6, 1, 2e6])
        transaction_case(request)
    for config in configurations:
        assert not configure(config)
        for count in range(9):
            for cpu_min in [0, 80, 250, 413]:
                base = rng.uniform(127, 400)
                py._BASE = base
                unmanaged = rng.randint(0, 3)
                request = {"op": "static", "config": config, "base": base, "n": count, "unmanaged": unmanaged, "cpu_min": cpu_min}
                equal(ask(request), py.static_safe(count, unmanaged, cpu_min), "static")
                checks += 1
    configure()
    for fan in range(1, 5):
        actions = [{"dt": rng.uniform(0.01, 10), "errs": [(rng.uniform(-40, 150), rng.uniform(.1, 5)) for _ in range(rng.randint(0, 8))]} for i in range(200)]
        governor, expected = py.Governor(fan), []
        for a in actions:
            governor.step(a["errs"], a["dt"])
            expected.append({"rpm": governor.rpm, "rate": governor.rate, "err": governor.err})
        equal(ask({"op": "governor", "fan": fan, "actions": actions}), expected, "governor")
        checks += 1
    for tau in [0, 4, 10, 30]:
        actions = [(rng.choice([None, rng.uniform(-10, 110)]), rng.uniform(.01, 10)) for _ in range(200)]
        filter_, expected = py.Filter(tau), []
        for v, dt in actions:
            ok = filter_.feed(v, dt)
            expected.append([filter_.raw, filter_.val, filter_.fails, ok])
        equal(ask({"op": "filter", "tau": tau, "actions": actions}), expected, "filter")
        checks += 1
    for text in ["", "auto", "max", "hold 500 1000 1100 1200", "hold 0 9999 700 700 gpu=150 pl1=10", "gpu=175", "pl1=NaN", "gpu=inf max", "hold NaN 500 500 500", "max ttl=nan", "gpu=20 gpu=200", "auto ignored gpu=150", "max ttl=-1", "max ttl=10", "bad gpu=150"]:
        for age in [0, 11]:
            py.open = lambda *args, **kwargs: io.StringIO(text)
            original_os = py.os
            original_time = py.time
            py.os = types.SimpleNamespace(stat=lambda _: types.SimpleNamespace(st_mtime=1000))
            py.time = types.SimpleNamespace(time=lambda: 1000 + age)
            mode, hold, note, pin = py.parse_override()
            py.os, py.time = original_os, original_time
            del py.open
            expected = {"mode": mode, "hold": None if hold is None else list(hold.values()), "pin": pin}
            equal(ask({"op": "override", "text": text, "age": age}), expected, "override")
            checks += 1
    for text in configurations + ["K_CPU = True", "GPU_CAP={'floor':126}", "GPU_CAP={'max':100.5}", "STATIC_SAFE={'pl1':99.5}", "PERIPH={1:(50,1,80)}", "CPU_TARGET=[(133,48),(228,57.5)]", "SMC={'x':([],[1],1,1)}", "BAND=1e999", "CPU_PL1={'floor':9}", "DIES={'x':True}", "ZONES=[]", "NVME=([],55,1)", "DEVICE_W={'disk':-1}", "BASE=1/0", "UNKNOWN=1"]:
        expected_error = bool(configure(text, count=0))
        result = ask({"op": "config", "config": text})
        assert ("error" in result) == expected_error, (text, result, expected_error)
        checks += 1
    print(f"Parity: {checks} scenarios pass; {iterations} compared fields; 20,000 allocation transitions; reference SHA-256 {SOURCE_HASH}")
finally:
    worker.stdin.close()
    try:
        worker.wait(timeout=5)
    except subprocess.TimeoutExpired:
        worker.kill()
        worker.wait()
    scratch.cleanup()
