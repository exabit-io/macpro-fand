# macpro-fand

Zone fan control and a 1228 W power budget for the **Mac Pro 7,1 (2019)** on Linux.

**2.0.0 is the Rust rewrite of 1.0.6**: the same fan and power policy, configuration, override file, command line and
systemd unit, as a native executable without a Python runtime. It passed the same 27 hardware tests on the Mac Pro as
1.0.6, against the exact executable the package installs, and its policy calculations are checked against the pinned
1.0.6 source in 2,958 differential scenarios. Measured against 1.0.6 on the same machine, it uses 20 % less CPU,
88 % less memory and half the syscalls, with the same wakeups and no Python runtime (details in the handoff).

`macpro-fand` runs the four fans of a Mac Pro through the T2 `applesmc` driver as four independent zones, and keeps
the whole machine under the 1228 W DC envelope of its power supply by managing the power caps of the GPU dies and the
CPU's RAPL limits. It replaces `t2fanrd` as the fan owner and keeps it as a backup governor.

## Why

`t2fanrd` follows the CPU temperature only. On a Mac Pro with MPX GPU modules it holds every fan at its 500 rpm
minimum while the GPU dies soak at 58 °C and above, and the hot silicon leaks more power than the fans would cost.
Measured on a Mac Pro 7,1 with a Xeon W-3275M and two Radeon Pro Vega II Duo modules, every point at thermal
equilibrium:

- **Idle:** the MPX-bay fans at 1150 rpm cut total DC power by 11.4 W against the 500 rpm floor. The GPU junctions
  drop from 58 to 38 °C. The CPU fan and the blower save nothing at idle and run lower, saving another 0.5 W.
- **CPU under load:** the Xeon's package power is flat up to about 60 °C and then rises steeply: +4.6 W at 78 °C at
  228 W. The best fan speed rises with load, so the CPU target follows package power.
- **Power envelope:** crossing 1228 W DC makes the SMC latch the GPU dies at 1000 MHz until a cold power cycle. Four
  dies and the CPU can spike together while every zone is still cool, so the power limits are managed on their own
  invariant, independent of temperature.

## What it does

| Fan | Zone | Holds |
|---|---|---|
| 2, bottom front | MPX bay 1, PCH | bay mean GPU junction 37.8 °C (100 °F), HBM, PCH |
| 3, middle front | MPX bay 2, PCIe slots 5-8 | bay mean GPU junction 37.8 °C, HBM, NVMe and Mellanox cards in slots 5-8 |
| 4, top front | CPU | package 48 °C at 133 W rising to 57.5 °C at 228 W |
| 1, blower | memory side | DIMMs 50 °C, power supply 55 °C, internal flash 55 °C |

- **One governor per fan, with hysteresis:** the fan speeds up in proportion to the excess over target, holds inside
  the band, and slows down slowly. It never hunts.
- **Alarms:** any alarm sends every fan to full speed. Alarms are a sensor near its critical limit, failed or stale
  readings, a fan not following its setting, or another process writing the fans. The SMC's automatic mode is never
  used.
- **Power budget:**
  - The rule is `BASE + 1.205 × Σ die caps + 1.156 × PL1 ≤ 1228 − 80 W`, with the constants calibrated on the machine.
  - Power moves by demand, CPU first.
  - One thread writes every power limit, so a blocked driver call never stalls the fans. Each change is one
    transaction: decreases first, then any limit that could not be read goes to 10 W (the lowest value ever written),
    and only with every limit confirmed do limits go up -- and only if the allocation fits the budget. Every write is read back, and the first that fails stops the transaction. Limits are
    re-checked every 2 s and restored if they differ.
  - PL2 is held at PL1. If it cannot be, or RAPL is not enforcing, the CPU is charged at what it can actually draw, in
    every path: demand, pins and the stop hook.
  - A die whose power cap cannot be managed is charged at Vega 20's 200 W maximum.
  - Uncapped devices found at start (Mellanox cards, extra NVMe drives, disks) add their worst case to the base.
  - A measured-DC backstop drops every limit to its floor.
- **Full auto:** each GPU die's cap also follows its temperature, held at a working temperature that limits leakage.
- **GPU cap ceiling:** the installed `/etc/macpro-fand.conf` sets `GPU_CAP = {"max": 125}`; comment that line out for
  Vega 20's full 200 W range.
- **Clean stop:** every fan goes to full speed and then to `t2fanrd`. On a failure, every fan stays at full speed and
  the power limits go to a statically safe allocation.
- **t2fanrd is an optional backup:** it is not required. When it is installed, starting macpro-fand stops it, and it
  cannot start while macpro-fand is running, starting or waiting to restart (a drop-in this package installs). t2fanrd
  takes over only when macpro-fand stops cleanly or fails for good; it is disabled at boot while macpro-fand is installed.
- **Fail-safe:** whenever neither governor runs, every fan runs at full speed: macpro-fand's failure path, the
  `macpro-fand-backup` unit (full speed, then t2fanrd if installed) and t2fanrd's stop hook.
- **Strict configuration:** an invalid `/etc/macpro-fand.conf` is reported and the daemon does not start;
  check a change with `macpro-fand --check-config`. One process owns the hardware: a second instance refuses to start.

The details are in `macpro-fand(8)` and `macpro-fand.conf(5)`.

## Build and verify

Rust 1.75 or newer, Cargo, and x86-64 Linux are required. There are no third-party crates, downloaded build dependencies
or Python runtime calls. Python 3 is needed for the development tests against the pinned specification.

```sh
make                 # target/release/macpro-fand
make check           # Rust safety tests, differential policy checks and CLI checks; no hardware/service calls
make fmt             # rustfmt and Clippy, warnings treated as errors
make package         # unsigned native .deb; also requires debhelper, scdoc and dpkg-dev
sudo tests/hardware/failover-tests.sh   # on an installed package: the t2fanrd ownership rules, fans at full speed
```

The existing `/etc/macpro-fand.conf`, `/run/macpro-fand/override`, command-line options and systemd unit paths are
preserved. The package installs `/usr/sbin/macpro-fand` and requires `systemd`, libc and libgcc, with no Python
runtime dependency. Installation starts the service. Hardware prerequisites remain `applesmc-t2`, `amdgpu`, `coretemp`
and `intel-rapl`.

## Install

From the Exabit apt repository (Ubuntu 24.04, x86-64):

```sh
sudo install -d -m 755 /etc/apt/keyrings
curl -fsSL https://exabit-io.github.io/resize-amdgpu-bars/keys/resize-amdgpu-bars.gpg \
    | sudo tee /etc/apt/keyrings/resize-amdgpu-bars.gpg > /dev/null
echo 'deb [signed-by=/etc/apt/keyrings/resize-amdgpu-bars.gpg] https://github.com/exabit-io/macpro-fand/releases/download/macpro-fand-2.0.1 ./' \
    | sudo tee /etc/apt/sources.list.d/macpro-fand.list
sudo apt update
sudo apt install macpro-fand
```

Each release is its own signed flat apt repository, tagged on the source commit it was built from:
https://github.com/exabit-io/macpro-fand/releases. To go back to 1.0.6, point the source line at `macpro-fand-1.0.6` and
`sudo apt install --allow-downgrades macpro-fand=1.0.6`.

See [the engineering handoff](docs/v2-handoff.md) for architecture, compatibility differences, verification evidence
and the hardware qualification procedure. The original daemon and its tests are retained under
`reference/v1.0.6/`; they are never installed by the native package.

## Use

```sh
systemctl status macpro-fand
journalctl -u macpro-fand -o cat | tail -1 | python3 -m json.tool    # full state, every 30 s

echo "max ttl=600" > /run/macpro-fand/override                     # every fan at full speed for 10 min
echo "auto gpu=110 ttl=7200" > /run/macpro-fand/override           # pin the GPU caps at 110 W for a benchmark
rm /run/macpro-fand/override                                       # back to normal

systemctl stop macpro-fand                                         # hand the fans back to t2fanrd
```

Settings (targets, calibration, hardware map) are in `/etc/macpro-fand.conf`, listed with their defaults. After a
change: `macpro-fand --check-config && systemctl restart macpro-fand`.

## License

MIT. Copyright 2026 Exabit, Inc. Report security issues to security@exabit.io.
