# Changelog

## 2.0.2

Peripheral support (slot cards, SATA/USB disks) and sensor I/O off the control thread.

- **No device I/O on the watchdog thread.** On 2026-10-22 the control loop blocked for more than 20 s in one sysfs
  read and systemd's watchdog killed the daemon. Every sensor read and fan write now runs in its own thread: one for
  the fans (settings written at once, tachometers and fan registers every 2 s, a changed fan put back), one for the
  CPU package and SMC temperatures, one per peripheral sensor, beside the existing GPU and DC samplers. Stale
  readings are failed reads (alarm: every fan to its cap), so a hung temperature read raises the alarm and the fan
  thread still writes the caps. A fan's registers are checked 1.5 s or more after it was written (the SMC shows a
  write in its registers 13-203 ms later, measured; a check at once made a ramping fan look changed by another
  writer). The keepalive needs the fan thread's progress: after 60 s without a fan round the
  loop withholds it and the watchdog's restart and failure path take over. Measured on the box: a disk temperature
  read issued during an HDD spin-up blocks for 2.7 s, and a drive in error recovery can hold one for 30 s or more.
- **Per-device power allowances.** The budget charges each peripheral by class (`DEVICE_W`: mellanox, nvme,
  nvme-switched, pcie-switch, pcie-card, disk, ssd, usb-disk), or by PCI id (`DEVICE_PCI_W`). Disks are classified by
  the rotation rate they report (SCSI VPD B1), never by the kernel's `rotational` flag for USB disks: 2.0.1 charged a
  USB-attached SATA SSD as a 25 W spinning disk. Only cards in `SLOT_ZONE` slots are peripherals; an NVMe drive
  directly in a slot is now charged 25 W (the drive's own maximum power state on the Samsung PM1725), 9 W behind a
  switch on an M.2 carrier.
- **Peripheral sensors:** one reader thread each, with a period and staleness limit per class (`PERIPH_POLL`:
  drivetemp every 30 s, NVMe every 5 s); rediscovered once a minute (driver reloads renumber hwmon); a sensor steers
  its fan from its first reading; one whose driver is gone for 20 s (before it goes stale: a driver reload raises no
  alarm) or that reads nothing for 600 s is retired to its
  device's `UNMONITORED` floor instead of a permanent alarm, and comes back when it reads again. USB and Thunderbolt
  devices are not cooled by the box's fans and steer none.
- **`TEMP_COMMAND`** (opt-in): a program that prints a slot card's temperature, for cards whose driver exposes no
  hwmon, e.g. MFT's `mget_temp -d {pci}` for ConnectX-4 cards; run on its own thread with a deadline.
- **`UNMONITORED`:** a zone floor for devices with no live temperature sensor: an rpm, or `(rpm, fan)` to also follow
  another fan. Default for Mellanox and other cards `(1000, 4)`: the zone test (below) found the top fan heating the
  slot-5/6 cards, by about 60 % of what the middle fan removes once settled and more in a state that had not settled.
- **Zone test with the peripherals (2026-10-23)**, one fan +600 rpm at a time to thermal equilibrium, drift-corrected
  between bracketing baselines: middle fan -5.3 / -5.2 / -4.0 °C on the slot 5 / 6 / 7 cards; top fan -2.7 / -3.0 °C on
  the internal-bay HDDs and +3.2 / +2.9 °C on the slot 5 / 6 cards (settled; a later step to +6.2 / +4.3 °C had not
  settled when the point ended: the NIC readings were logged beside the test, not gated by it); bottom fan at most
  -1.4 °C on any of them. Rerun with the cards cabled and their temperatures gated: the top fan +600 alone left them
  wandering +4 to +9 °C for 100 minutes without settling; with the middle fan +600 as well they settled -7.1 / -7.3 °C
  and the HDDs kept their -3 °C. A sustained PM1725 write (+9.3 W on the slot rail) at the base fans: PM1725 +13 °C,
  slot 6 card +1.7 °C, slot 5 card 0; with the middle fan +600, -8.0 / -7.2 °C on the cards.
- **`--inventory`:** prints the charged peripherals, the budget base and the peripheral sensors (JSON).
- Defaults: `DISK_ZONE` 4 (the internal drive bay sits in the top fan's zone), `SLOT_ZONE` without slot 8 (the Apple
  I/O card).
- **One apt source for every release:** the rolling channel
  `https://github.com/exabit-io/macpro-fand/releases/download/apt` holds every published version, so new releases
  arrive with `apt update && apt upgrade`. Until 2.0.1 each release was a separate repository and the source line had
  to be edited for every upgrade; replace it with the channel line in the README once.
- Hardware tests: the 2.0.1 fail-safe tests are T24/T25 (T13/T14 were numbered twice); GPU dies are found by bus
  (adding a slot-5 card moved them); T26-T31 for the inventory, both thread stalls, a driver unloaded and loaded,
  `TEMP_COMMAND` and the `UNMONITORED` floor.

## 2.0.1

systemd integration and packaging only; the daemon's code is unchanged.

- **macpro-fand takes priority over t2fanrd; t2fanrd is an optional backup.** 2.0.0 declared
  `Conflicts=t2fanrd.service`, and that dependency is symmetric: anything that started t2fanrd (a test script, a package
  upgrade, an administrator) stopped macpro-fand and cancelled its pending restart. On 2026-10-22 a start of t2fanrd
  in the 2 s restart window after a watchdog timeout left macpro-fand down for three hours. Now:
  - starting macpro-fand stops t2fanrd and waits until it has fully exited (`ExecStartPre`), so the fans never have
    two writers;
  - t2fanrd cannot start while macpro-fand is active, starting or waiting to restart: the package installs a t2fanrd
    drop-in (`t2fanrd.service.d/macpro-fand-backup.conf`) whose `ExecCondition` skips the start;
  - t2fanrd takes over when macpro-fand stops cleanly (the stop hook's handover, unchanged) or fails for good
    (`OnFailure=macpro-fand-backup.service`);
  - t2fanrd is not required: without it, `Suggests:` only, nothing else changes.
- **Fail-safe to full speed** whenever neither governor runs: macpro-fand's failure path (unchanged), the new
  `macpro-fand-backup.service` (every fan to full speed, then t2fanrd if installed), and t2fanrd's stop hook (when it
  stops after running and macpro-fand is not taking the fans). The new `/usr/lib/macpro-fand/fans-to-max` acts only on a
  MacPro7,1 and never uses the SMC's automatic mode.
- **Install:** t2fanrd is disabled at boot on install and enabled again on removal. The service start that debhelper
  runs quietly is now reported: the postinst says when the start can take minutes and, if the service is not running
  afterwards, prints its state and last messages instead of finishing silently.
- **Start timeout 300 -> 540 s:** the start waits up to 240 s for applesmc and then up to 240 s for the amdgpu dies;
  300 s let systemd kill a start that would have succeeded when both drivers came up late.
- Hardware tests: T9 (a t2fanrd start beside macpro-fand is refused), T11 (the restart window), T12 (permanent failure:
  full speed, then t2fanrd), T13 (t2fanrd stopping with no owner: full speed), T14 (no t2fanrd: full speed).

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
