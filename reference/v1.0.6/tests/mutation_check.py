#!/usr/bin/env python3
"""mutation_check.py SCRIPT -- re-inject each fixed defect into a temporary copy of the daemon and require
tests/test_safety.py to fail on it, in a check tagged with that defect's finding id: evidence that the safety suite
actually guards every fix. The unmodified daemon must pass the suite first (a failing baseline stops the scoring).
The daemon itself is never modified. Slow (one safety-suite run per mutation), so not part of the package build;
exit 0 when every mutation is caught by its own check.
"""
import os
import subprocess
import sys
import tempfile

SCRIPT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "macpro-fand")
SUITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "test_safety.py")

# (finding, description, text in the daemon, replacement that brings the defect back); a defect guarded by two
# layers is re-injected with both: (finding, description, [(text, replacement), ...]). The finding id must appear in a
# failing check (tags like "[R01/V01]" match "R01" and "V01").
MUTATIONS = [
    ("F01", "a failed write does not stop the transaction",
     "        if not put(p, want[p]):\n            return False, p, seen\n    for p in pl_order([p for p in want if cur[p] is None], False):",
     "        if not put(p, want[p]):\n            continue\n    for p in pl_order([p for p in want if cur[p] is None], False):"),
    ("R01", "an unreadable limit is written straight to its target",
     "        v = LIMIT_MIN * 1e6\n        if p == RAPL_PL2", "        v = want[p]\n        if p == RAPL_PL2"),
    ("R07", "read-back accepts a limit above its target",
     "    return got is not None and target - POWER_TOL <= got <= target\n",
     "    return got is not None and abs(got - target) <= POWER_TOL\n"),
    ("R07", "desired limits are rounded to nearest",
     "        w = {p: math.floor(self.gpu[a]) * 1e6 for a, p in self.paths.items()}\n",
     "        w = {p: round(self.gpu[a]) * 1e6 for a, p in self.paths.items()}\n"),
    ("R04", "no lower bound on written limits",
     "or v < LIMIT_MIN * 1e6]", "or v < 0]"),
    ("R04", "GPU init below the floor accepted",
     "and _num(g[\"init\"], gf, GPU_HW_MAX)", "and _num(g[\"init\"], 0, GPU_HW_MAX)"),
    ("R02", "a pin ignores the CPU's charge",
     "            self.fit(min(pp, CPU_PL1[\"floor\"]) if pp else None)\n            return None\n",
     "            self.pl1 = min(self.pl1, self.room(self.gpu))\n            return None\n"),
    ("R02", "the stop hook ignores RAPL not enforcing",
     "    cpu_min = 0.0 if enforcing and pl2_present else CPU_HW_MAX\n", "    cpu_min = 0.0\n"),
    ("R02", "an unheld PL2 is no longer read",
     "            if RAPL_PL1 in want and RAPL_PL2 not in want:\n                watch.add(RAPL_PL2)\n", ""),
    ("R02", "an unheld PL2 reading never goes stale",
     "    return st[\"seen\"].get(RAPL_PL2) if t is not None and now - t <= PL2_FRESH else None\n",
     "    return st[\"seen\"].get(RAPL_PL2) if st else None\n"),
    ("R03", "stop() does not wait for the operation in progress",
     "            return self.cv.wait_for(lambda: self.pending is None, timeout)\n", "            return True\n"),
    ("R03", "write_static_safe() does not stop a live worker",
     "    for w in list(PowerWorker.live):                     # one writer", "    for w in []:                     # one writer"),
    ("Q01", "an operation starts after its deadline",
     "    if end - time.monotonic() <= 0:\n        return False, None\n    res = []\n", "    res = []\n"),
    ("R05", "a malformed literal raises (both layers)",
     [("        except Exception as e:                              # noqa: BLE001 -- any malformed literal is an error, not",
       "        except (ValueError, SyntaxError) as e:              # noqa: BLE001 -- any malformed literal is an error, not"),
      ("    try:\n        cand, errors = read_config(path)\n        errors += check_config(cand)\n"
       "    except Exception as e:                                 # noqa: BLE001\n        return [f\"{path}: {type(e).__name__}: {e}\"]\n",
       "    cand, errors = read_config(path)\n    errors += check_config(cand)\n")]),
    ("R05", "an unhashable fan value raises (both layers)",
     [("    return isinstance(v, int) and not isinstance(v, bool) and v in CAP\n", "    return v in CAP\n"),
      ("    try:\n        return _check_config(cand)\n    except Exception as e:                                 # noqa: BLE001\n"
       "        return [f\"configuration rejected: {type(e).__name__}: {e}\"]\n", "    return _check_config(cand)\n"),
      ("    try:\n        cand, errors = read_config(path)\n        errors += check_config(cand)\n"
       "    except Exception as e:                                 # noqa: BLE001\n        return [f\"{path}: {type(e).__name__}: {e}\"]\n",
       "    cand, errors = read_config(path)\n    errors += check_config(cand)\n")]),
    ("R06", "the stop hook ignores the owner lock",
     "        owned = flock_within(fd, 3.0)\n", "        owned = True\n"),
    ("R08", "a read alarm is raised only when a new sample is fed",
     "                if any(flt.fails >= READ_FAILS for flt in (d.tj, d.tm, d.p)):   # every loop, until a good reading\n"
     "                    alarms.append(\"read \" + d.addr)\n", ""),
    ("R09", "GPU filters use the loop's time step",
     "                        flt.feed(v, gap)\n", "                        flt.feed(v, dt)\n"),
    ("Q02", "a failed SMC key lookup is never retried",
     "            if time.monotonic() >= self.next_try and not (self.finder and self.finder.is_alive()):",
     "            if False:"),
    ("V01", "an unreadable limit goes to its configured floor (not LIMIT_MIN)",
     "        v = LIMIT_MIN * 1e6\n        if p == RAPL_PL2",
     "        v = min(want[p], (GPU_CAP[\"floor\"] if p.endswith(\"power1_cap\") else CPU_PL1[\"floor\"]) * 1e6)\n        if p == RAPL_PL2"),
    ("V02", "an infeasible plan still raises limits",
     "    if up and not raise_ok:\n        return False, \"infeasible\", seen\n", ""),
    ("V03", "readings are stamped with the transaction's completion time",
     "                    if p in read_t:\n                        self.seen[p], self.seen_t[p] = v, read_t[p]\n",
     "                    self.seen[p], self.seen_t[p] = v, now\n"),
    ("V04", "fans go to their caps only after the worker wait",
     "        if not args.dry_run:\n            full()                                # cooling first: the control loop has stopped\n", ""),
    ("N01", "a binary override raises",
     "    except (OSError, ValueError):                          # unreadable, or not UTF-8 text\n",
     "    except OSError:                                        # unreadable, or not UTF-8 text\n"),
    ("N02", "PERIPH keys are not checked",
     "    need(isinstance(pe, dict) and all(isinstance(kk, str) and isinstance(v, tuple)",
     "    need(isinstance(pe, dict) and all(isinstance(v, tuple)"),
    ("N03", "fractional limits are accepted",
     "    whole = lambda v: _num(v) and float(v).is_integer()   # limits are written in whole watts\n",
     "    whole = lambda v: True\n"),
    ("W01", "feasibility is not re-judged at run time (no infeasible alarm)",
     "                if not feasible:                                    # start's: an infeasible allocation alarms (fans\n",
     "                if False:                                           # start's: an infeasible allocation alarms (fans\n"),
    ("N404", "fractional static-safe limits are accepted",
     "    need(not isinstance(ss, dict) or all(whole(ss.get(k)) for k in (\"gpu\", \"pl1\") if _num(ss.get(k))),\n",
     "    need(True,\n"),
    ("F05", "an unmanaged die is not charged", "        return base() + K_GPU * GPU_HW_MAX * self.n_unmanaged\n",
     "        return base()\n"),
    ("F06", "the CPU charge is never updated", "        if abs(low - self.cpu_min) < 0.5:\n            return False\n",
     "        return False\n"),
    ("F10", "a pin ending during the backstop hold resets at once",
     "        if now < self.floor_until:\n            self.reset_pending = True\n        else:\n            self.reset()\n",
     "        self.reset()\n"),
    ("F11", "a failed RAPL read crashes the rate", "    if e is None or prev[1] is None or prev[0] is None or now <= prev[0]:\n",
     "    if now <= prev[0]:\n"),
    ("L02", "a NaN override pin is accepted", "    if not math.isfinite(v):\n        raise ValueError(word)\n    return v\n",
     "    return v\n"),
]


def run_suite(path):
    r = subprocess.run([sys.executable, SUITE, path], capture_output=True, text=True, timeout=900)
    return r.returncode, [line[6:] for line in r.stdout.splitlines() if line.startswith("FAIL  ")]


def main():
    src = open(SCRIPT).read()
    rc, fails = run_suite(SCRIPT)
    if rc != 0:
        print(f"BASELINE FAILS ({len(fails)}): the unmodified daemon must pass the suite before mutations are scored")
        for f in fails[:10]:
            print("  " + f)
        sys.exit(2)
    print("baseline: the unmodified daemon passes the suite")
    missed, broken = [], []
    with tempfile.TemporaryDirectory() as d:
        for fid, what, *edits in MUTATIONS:
            pairs = edits[0] if isinstance(edits[0], list) else [tuple(edits)]
            if any(src.count(old) != 1 for old, _ in pairs):
                broken.append(f"{fid} {what}: anchor not found once")
                print(f"BROKEN  {fid}  {what}: anchor not found")
                continue
            mutated = src
            for old, new in pairs:
                mutated = mutated.replace(old, new)
            path = os.path.join(d, "macpro-fand")
            open(path, "w").write(mutated)
            rc, fails = run_suite(path)
            own = [f for f in fails if fid in f.split("]")[0]]          # a failing check tagged with this finding
            caught = rc != 0 and bool(own)
            print(f"{'caught' if caught else 'MISSED'}  {fid}  {what}: "
                  + (f"{len(own)} of {len(fails)} failing checks are {fid}'s, e.g. {own[0][:60]}" if own
                     else f"{len(fails)} failing, none tagged {fid}" if fails else "suite passed"))
            if not caught:
                missed.append(f"{fid} {what}")
    print(f"\n{len(MUTATIONS) - len(missed) - len(broken)}/{len(MUTATIONS)} mutations caught"
          + (f"; missed: {missed}" if missed else "") + (f"; broken: {broken}" if broken else ""))
    sys.exit(1 if missed or broken else 0)


if __name__ == "__main__":
    main()
