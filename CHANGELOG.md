# Changelog

## 2.0.0

- Native Rust implementation of the 1.0.6 control policy, config, CLI, overrides and service interfaces.
- One power worker with generation cancellation between operations; read-back transactions, conservative CPU
  accounting, lower-only infeasible allocations, secure owner locks and deadline-bounded static recovery.
- Independent DC sampler, early full cooling for known startup faults, and failure reporting when ownership,
  service state, fan writes or recovery cannot be verified. Only clean foreground stops hand over to t2fanrd.
- Bounded non-executing config parser, regular-file checks, no external crates or Python runtime dependency.
- Native safety tests, 2,958 differential scenarios against the SHA-pinned specification, and a hardware runner
  adapted for the native executable. Qualified on the Mac Pro 7,1: 27/27 hardware tests on the released executable.
- Costs less than 1.0.6: -20 % CPU at idle (-18 % under GPU load), -88 % memory, -50 % syscalls, -58 % startup CPU,
  the same wakeups, and no Python runtime. The sampler threads sleep on a condition variable that a stop wakes at
  once; the control loop checks for a stop twice a second.

## 1.0.6

Documentation only, from a fifth external review of 1.0.5 (1.0.5 was not released). The daemon's code is unchanged.

- The 10 W staging step for an unreadable limit is the one bounded exception to "no step costs more than the larger of
  the state before and the target". It is at most 59.76 W of model with the default four-die calibration, inside the
  default 80 W margin; a custom margin, coefficients or topology need their own check.

## 1.0.5

Fixes from a fourth external review of 1.0.4 (1.0.4 was not released). Control behaviour unchanged.

- **Runtime feasibility:** the budget's feasibility is judged on every loop with the CPU's present charge, not once at
  start. An allocation that stops fitting (RAPL no longer enforcing, PL2 not held) raises an alarm, with every fan at
  its cap, for as long as it lasts, while the power worker only lowers limits. The state log reports "feasible"
  separately from "confirmed".
- **Configuration:** `STATIC_SAFE` limits must be whole watts too.
- **Tests:** the `--check-config` subprocess runs on the inventory fixture.

## 1.0.4

Fixes from a third external review of 1.0.3 (1.0.3 was not released). Control behaviour unchanged.

- **Unknown limits:** a power limit whose value cannot be read is first set to 10 W, the lowest value ever written,
  instead of its configured floor. With raised floors and older caps (or a CPU pin) below them, the floor could be an
  increase made before headroom existed.
- **Infeasible plans:** an allocation that does not fit the budget only lowers limits, in normal operation and in the
  stop hook; a static-safe plan that cannot fit no longer raises a low CPU pin, and the stop reports incomplete
  recovery.
- **Reading age:** every power-limit reading carries the time it was actually read, so a slow transaction no longer
  makes an old PL2 reading look fresh.
- **Stop:** every fan goes to full speed before the wait for the power worker.
- **Input:** a binary override file is ignored instead of restarting the daemon; `PERIPH` keys must be driver names;
  power limits in the configuration must be whole watts.
- **Tests:** the host inventory is a fixture, so both suites pass on a builder with no GPU. A crashing test counts as a
  named failure. `tests/mutation_check.py` requires a passing baseline, and each re-injected defect must fail its own
  finding's check.

## 1.0.3

Fixes from a second external review of 1.0.2 (1.0.2 was not released). The control behaviour (zones, targets, budget
constants, full auto) is unchanged.

- **Unknown limits:** a power limit whose value cannot be read goes to its floor before anything is raised; nothing goes
  up until every limit is confirmed, so no step costs more than the larger of the state before and the target.
- **Rounding and read-back:** a limit counts as set only at or up to 1 W below its target, never above. Limits are
  written in whole watts rounded down, and never below 10 W (amdgpu takes 0 as "restore the default").
- **Uncontrolled CPU:** the charge for an unheld PL2 or RAPL not enforcing applies to override pins, the reset after a
  pin and the stop hook. An unheld PL2 is read every cycle; a stale reading counts as 413 W.
- **Stop:** the power worker starts nothing more and the operation in progress is waited for before the static-safe
  limits are written. With an operation stuck in the driver, no competing transaction is started: fans stay at their
  caps and a stop by hand exits 1. No bounded operation starts after its deadline.
- **Stop hook:** takes the owner lock and leaves another running macpro-fand alone. It recovers with the built-in
  defaults whatever the configuration file holds; no parse or validation error can stop it.
- **Configuration:** `GPU_CAP` floor <= init, `STATIC_SAFE` no lower than the floors, PL1 floor at least 10 W;
  unhashable or oversized values are reported as errors.
- **Readings:** GPU read-failure alarms persist until a good reading; GPU filters use the time between samples; SMC key
  lookups that failed are retried; GPU dies that are not Vega 20 are logged as unsupported.
- **Build:** lock files are checked against the running user, so both test suites run as an ordinary user.

## 1.0.2

Safety fixes from an external review of 1.0.1. The control behaviour (zones, targets, budget constants, full auto) is
unchanged.

- **Power-limit writes:** one worker thread owns every power-limit read and write, so a blocked amdgpu call never stalls
  the fan loop. Each change is a transaction (confirmed decreases, then limits that could not be read, then increases);
  every write is read back and the first failure stops the transaction, so nothing is raised on budget that was not
  released. The limits are re-read every 2 s and restored or retried until they read back; a stuck or unconfirmed
  limit is an alarm.
- **Stop hook:** the static-safe limits are written by the same rule, every read and write bounded, 10 s for the whole.
- **Accounting:** a die without a manageable power cap is charged 200 W, in the budget and in the static-safe limits.
  The CPU is charged at PL2 as read back when PL2 cannot be held at PL1, and at 413 W while RAPL does not enforce.
- **Backstop:** an override pin that ends during the 60 s backstop hold waits for it. Without a measured DC reading
  for 15 s the allocation is held where it is.
- **Configuration:** the whole file is validated (types, shapes, ranges, finite numbers, floors that fit the budget)
  before anything is used. An invalid file is reported and the daemon exits 78 with nothing applied. New option
  `--check-config`. A lower `GPU_CAP` maximum also bounds the start allocation and the static-safe caps.
- **Locks:** `/run/macpro-fand/owner.lock` makes the hardware owner exclusive (a second instance refuses to start). The
  SMC lock moves to `/run/macpro-fand/smc.lock`: root-only, never opened through a symlink, waited for with a bound.
- **Readings:** GPU readings carry their own read time per die. A die at its emergency temperature alarms even when
  its power cannot be read. A failed RAPL read no longer crashes the loop, and a wrapped energy counter is unwrapped.
  Non-finite sensor and override values are rejected.
- **Modes:** cleanup covers every hardware write from the first one; `--dry-run` loads no kernel module; contradictory
  modes are refused.
- **Tests:** a failure-path suite (`tests/test_safety.py`) runs at build time beside the offline checks.
- **Documentation:** the override example pins 110 W, within the installed 125 W ceiling (a pin above the configured
  maximum is lowered to it).

## 1.0.1

- The installed `/etc/macpro-fand.conf` sets `GPU_CAP = {"max": 125}`: GPU dies at most 125 W by default. The built-in
  range is unchanged (85-200 W); comment the line out for the full range.
- The budget's reallocation clamps every die to the configured `GPU_CAP` maximum itself (a ceiling below the default
  share + 10 W could be exceeded until the working-temperature ceilings caught up).

## 1.0.0

First release.

- Four zone fan governors (MPX bay 1 + PCH, MPX bay 2 + slots 5-8, CPU, memory side) with hysteresis, each holding
  its zone at measured targets; any alarm sends every fan to full speed; the SMC's automatic mode is never used.
- 1228 W power budget over the GPU die caps and the CPU's PL1: a worst-case invariant, demand-driven reallocation
  with the CPU first, lowering writes before raising writes, PL2 held at PL1, a measured-DC backstop, statically safe
  limits on stop and on failure.
- Full auto: per-die GPU ceilings that hold each die at a working temperature.
- CPU target that follows package power.
- Uncapped devices (Mellanox cards, NVMe drives, rotational disks) found at start are added to the base load, and
  their temperatures join the zone of their PCI slot.
- GPU dies found automatically, their MPX bay from their PCI slot.
- `/etc/macpro-fand.conf` for targets, calibration and hardware map; the override file for benchmarks (`max`,
  `hold`, `gpu=`, `pl1=`, `ttl=`).
