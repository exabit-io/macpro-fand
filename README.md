# macpro-fand

Zone fan control and a 1228 W power budget for the **Mac Pro 7,1 (2019)** on Linux.

`macpro-fand` runs the four fans of a Mac Pro through the T2 `applesmc` driver as four independent zones, and keeps
the whole machine under the 1228 W DC envelope of its power supply by managing the power caps of the GPU dies and the
CPU's RAPL limits. It replaces `t2fanrd` as the fan owner.

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
  - Power moves by demand, CPU first. Limits are always lowered before any limit is raised.
  - PL2 is held at PL1.
  - Uncapped devices found at start (Mellanox cards, extra NVMe drives, disks) add their worst case to the base.
  - A measured-DC backstop drops every limit to its floor.
- **Full auto:** each GPU die's cap also follows its temperature, held at a working temperature that limits leakage.
- **Clean stop:** every fan goes to full speed and then to `t2fanrd`. On a failure, every fan stays at full speed and
  the power limits go to a statically safe allocation.

The details are in `macpro-fand(8)` and `macpro-fand.conf(5)`.

## Install

From the Exabit apt repository (Ubuntu 24.04):

```sh
sudo install -d -m 755 /etc/apt/keyrings
curl -fsSL https://exabit-io.github.io/resize-amdgpu-bars/keys/resize-amdgpu-bars.gpg \
    | sudo tee /etc/apt/keyrings/resize-amdgpu-bars.gpg > /dev/null
echo 'deb [signed-by=/etc/apt/keyrings/resize-amdgpu-bars.gpg] https://github.com/exabit-io/resize-amdgpu-bars/releases/download/macpro-fand-1.0.0 ./' \
    | sudo tee /etc/apt/sources.list.d/macpro-fand.list
sudo apt update
sudo apt install macpro-fand
```

Requires the `applesmc-t2` driver (fan control through the T2 chip), `amdgpu`, `coretemp` and `intel-rapl`. The
service starts on install and stops `t2fanrd`.

## Use

```sh
systemctl status macpro-fand
journalctl -u macpro-fand -o cat | tail -1 | python3 -m json.tool    # full state, every 30 s

echo "max ttl=600" > /run/macpro-fand/override                     # every fan at full speed for 10 min
echo "auto gpu=150 ttl=7200" > /run/macpro-fand/override           # pin the GPU caps at 150 W for a benchmark
rm /run/macpro-fand/override                                       # back to normal

systemctl stop macpro-fand                                         # hand the fans back to t2fanrd
```

Settings (targets, calibration, hardware map) are in `/etc/macpro-fand.conf`, listed with their defaults.

## License

MIT. Copyright 2026 Exabit, Inc. Report security issues to security@exabit.io.
