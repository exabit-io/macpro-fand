# Changelog

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
