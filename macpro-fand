#!/usr/bin/env python3
"""macpro-fand: zone fan control and power budget for a Mac Pro 7,1 (2019) on Linux (T2 applesmc).

Four zones, one fan and one governor each (zone map measured one fan at a time):
  bottom front fan  MPX bay 1 + PCH                    middle front fan  MPX bay 2, PCIe slots 5-8
  top front fan     CPU                                blower            DIMMs, internal flash, power supply
Each governor holds its zone at fixed thermal targets with hysteresis: over target + BAND it pushes its fan up,
proportionally to the excess; inside the band it holds; under target - BAND it ramps down slowly. The MPX bays are
held at 100 F (37.8 C, bay mean GPU junction) at whatever fan speed that takes. Any alarm sends every fan to its cap,
and the governors then ramp down slowly from there.

Alarms: a sensor near its critical limit, failed or stale sensor reads, a fan that does not follow its setting,
another process writing the fans. The fans never go to the SMC's automatic mode: it has been seen holding every fan
at its minimum and does not follow GPU heat. A clean stop hands the fans to t2fanrd (cap first); a failure leaves
them at the cap.

GPU sensors are read in a worker thread: a die whose SMU stops answering costs ~4 s per read, and an amdgpu read can
block for good behind a stuck driver operation. The control loop only ever touches applesmc and coretemp; GPU data
older than GPU_STALE raises an alarm.

Power budget: the box must never draw more than the SMC's 1228 W DC envelope (crossing it latches the Vega 20 dies
at 1000 MHz until a cold power cycle). Power caps are kept independent of temperature: whatever spikes at once, the
sum of every consumer's worst case at its current limit stays under the budget,
  BASE + K_GPU * sum(die caps) + K_CPU * PL1  <=  LIMIT - MARGIN,
and the budget moves by demand, CPU first (everything in the box depends on the CPU). A CPU drawing >= 85 % of PL1
gets at least its default share at once, funded from free budget, idle dies, then every die evenly (a little off
each of four); dies drawing >= 93 % of their cap get free budget, idle siblings' budget and only what the CPU does
not use; whatever the dies do not hold is the CPU's. Within the budget each die's cap follows its temperature (full
auto): over its working temperature T_WORK its ceiling comes down, below it the ceiling goes back up slowly.
Lowering writes finish before any raising write. PL2 is held at PL1, so the CPU cannot burst past what the budget
charges it for. BASE grows by a worst case for every uncapped device found at start beyond the calibrated box (NICs,
NVMe drives, hard disks). Backstop: measured DC over BACKSTOP on two samples in a row -> every limit to its floor
for 60 s. On stop or crash the limits go to STATIC_SAFE.

Read-only towards amdgpu except the power caps. Run as root, normally as macpro-fand.service (Conflicts=t2fanrd).
Settings: /etc/macpro-fand.conf (see macpro-fand.conf(5)); usage: macpro-fand(8).
  macpro-fand             control loop
  macpro-fand --dry-run   compute and log, write nothing
  macpro-fand --release   hand the fans to t2fanrd (every fan to its cap first)
  macpro-fand --stopped   systemd ExecStopPost: clean stop -> t2fanrd, failure -> full speed

Override (benchmarks, experiments): one line in /run/macpro-fand/override, re-read every loop:
  max                     every fan to its cap
  hold F1 F2 F3 F4        exactly these rpm (an alarm still sends every fan to its cap)
  gpu=W / pl1=W           pin the GPU caps / PL1 (inside the budget; working-temperature ceilings do not apply)
  auto                    normal control (same as no file)
  optional trailing ttl=SECONDS: ignored once the file is older than that
"""
import argparse
import fcntl
import glob
import json
import math
import os
import signal
import socket
import stat
import struct
import subprocess
import sys
import threading
import time

VERSION = "1.0.6"
CONFIG = "/etc/macpro-fand.conf"
EX_NOHW = 78           # no Mac Pro 7,1 fan hardware: systemd does not restart (RestartPreventExitStatus=78)

# ---------------------------------------------------------------- config
PERIOD = 2.0                          # loop period, s

# applesmc numbering: fan1 = SMC F0 = rear blower (memory side, 500-1200 rpm); fan2..fan4 = F1..F3 = front axials
# (500-2500 rpm), bottom to top. Zone test, one fan raised at a time at thermal equilibrium, C per 100 rpm:
#   fan2 bottom: MPX bay 1 dies -0.66/-0.72, PCH -0.44          fan3 middle: MPX bay 2 dies -0.33/-0.34
#   fan4 top: CPU heatsink -0.12, package -0.14, DIMMs -0.10
#   blower: DIMMs -0.24, flash -0.20, PSU -0.6, CPU heatsink -0.19
BLOWER, BOT, MID, TOP = 1, 2, 3, 4
FRONT = (BOT, MID, TOP)
MIN_RPM = {BLOWER: 500, BOT: 500, MID: 500, TOP: 500}
CAP = {BLOWER: 1200, BOT: 2500, MID: 2500, TOP: 2500}

# GPU dies by PCI address -> MPX bay.
DIES = {}                              # {"0b:00.0": bay, ...}; empty = every AMD display device, bay by slot (MPX_BAY)
MPX_BAY = {"1": 1, "2": 1, "3": 2, "4": 2}   # PCI slot -> MPX bay (bay 1 = lower module, slots 1-2; bay 2 = slots 3-4)
BAY_FAN = {1: BOT, 2: MID}             # each front fan cools the lane behind it (cross terms 15-40 % of the own lane)

# Zone governors. init = the measured idle setpoint (the start point; bays 1150 hold ~100 F in a ~16 C room);
# floor = the lowest rpm the governor may choose. The MPX bays have no energy floor: 100 F decides. The CPU fan buys
# nothing at idle (DC -0.53 W at 500 vs 1150, package +4 C, idle power unchanged); the blower is flat from 500 to 730
# within 0.05 W and 600 keeps the power supply cooler (line search, one fan at a time, at thermal equilibrium). The
# per-zone setting measured -0.5 W against all front fans at 1150 and the blower at 730.
ZONES = {
    BOT:    {"name": "MPX bay 1 + PCH", "floor": 500, "init": 1150},
    MID:    {"name": "MPX bay 2", "floor": 500, "init": 1150},
    TOP:    {"name": "CPU", "floor": 500, "init": 500},
    BLOWER: {"name": "memory side", "floor": 600, "init": 600},
}
# Targets, C. The leakage zones hold their sensors at the idle temperatures of the energy-optimal setpoint (the lowest
# reasonably achievable leakage), measured at thermal equilibrium; the MPX bays at 100 F.
# BAND = default hysteresis half-width; the 1 C-step sensors (CPU package, PCH, flash) get 1.0 so quantisation cannot
# make a governor hunt.
BAY_TARGET = 37.8      # each bay's mean GPU junction (= 100 F; 37.9-38.1 bay 1, 36.0-37.0 bay 2 at 1150 rpm)
HBM_TARGET = 80.0      # each bay's hottest HBM stack: a safety trigger, not an idle target
# CPU target from two CPU fan sweeps under load, every point at equilibrium, down and back up (56 busy workers, clock
# pinned), total DC against the top fan:
#   133 W package (2.2 GHz): package power flat within 0.3 W from 37 to 67 C; DC vs
#     1000 rpm: 500 +1.6, 750 -0.07, 1000 0, 1250 +0.18, 1500 +0.8, 2000 +2.1, 2500 +4.5 W -> optimum 750-1000 rpm,
#     package 44-52 C (the gain there is board cooling around the CPU).
#   228 W package (3.0 GHz): package power flat from 52 to 59 C, then +1.2 W at 64 C
#     and +4.6 W at 78 C (leakage); DC vs 1500 rpm: 750 +7.2, 1000 +1.6, 1250 +0.1, 1500 0, 2000 +0.5, 2500 +2.5 W
#     -> optimum 1250-1500 rpm, package 56-59 C.
# The optimum temperature rises with load, so the target follows package power (RAPL, smoothed over CPU_TARGET_TAU):
# 48 C at 133 W to 57.5 C at 228 W, interpolated, held flat outside. Idle (~28 C) stays on the 500 rpm floor.
CPU_TARGET = ((133.0, 48.0), (228.0, 57.5))   # (package W, package C) points
CPU_BAND = 1.0
CPU_TARGET_TAU = 30.0      # s, package power smoothing for the target
BAND = 0.5
UP_GAIN, UP_MAX = 1.0, 150.0                    # rpm/s per C over the target; cap on the push, rpm/s
DOWN_GAIN, DOWN_MIN, DOWN_MAX = 0.5, 0.25, 2.0  # rpm/s per C under the target, clipped: down is slow by design
EMERG_MARGIN = 5.0     # a raw reading within this of crit sends every fan to its cap
# Power budget. K_GPU / K_CPU / BASE: DC W per die W, per CPU package W, and the idle box with every fan at full
# speed plus an allowance for memory/IO load -- from a calibration run on a Mac Pro 7,1 (W-3275M, 2x Vega II Duo).
BUDGET_LIMIT = 1228.0      # SMC DC envelope, W
BUDGET_MARGIN = 80.0       # kept free: model error, AC-side events (a clamp once followed an AC spike, no DC breach)
# Calibration (fans at full speed, 14 steps, each at equilibrium): DC = 66.9 + 1.205 * dies +
# 1.156 * package, rms 3.9 W, max 8 W (GPUs 85-200 W, CPU 100-268 W, combined up to 975 W predicted / 975 W measured).
# BASE adds 60 W for memory/flash traffic the model does not see (DIMM saturation + flash reads measured +45 W).
BASE, K_GPU, K_CPU = 127.0, 1.205, 1.156
# max = Vega 20's own 200 W. The efficient cap depends on the workload (memory-bound LLM decode gains little above
# ~110 W; compute-bound FP32/FP64 keeps gaining to 175 W and beyond), so a lower ceiling is a per-node policy in
# /etc/macpro-fand.conf, not a default.
GPU_CAP = {"floor": 85, "max": 200, "init": 125}       # W per die; caps <= 85 W all land on the 999 MHz floor
# Full auto: a die's cap follows its temperature, not a fixed number. The fans hold the bays at 100 F
# while they can; once a die passes its working temperature (far above 100 F: its bay fan is at full speed by then) its
# ceiling comes down until it holds there, and goes back up slowly once it is cooler. Hot dies leak, and LLM decode is
# HBM-bandwidth-bound, so watts above the working temperature buy little. The budget allocates within these ceilings.
# T_WORK from the GPU load sweep (four dies at 125 W, both bay fans stepped down and back up, every point at
# equilibrium): total DC is lowest at 1750-2000 rpm with the junctions at 65-67 C; hotter, throughput falls 0.05 % per
# C at a fixed cap and the MPX rails outside the capped dies draw more (+8.4 W at 1250 rpm, 81 C), and below 1500 rpm
# the heat removal collapses. 65 C = the junction at that optimum.
T_WORK = (65.0, 1.0)       # die junction C, band
HBM_WORK = 80.0            # hottest HBM stack, C (same band)
CAP_DOWN = (1.0, 10.0)     # ceiling W/s per C over the band, max W/s (from the die's present draw: acts at once)
CAP_UP = (0.1, 1.0)        # ceiling W/s per C under the band, max W/s
CAP_EARLY = 5.0            # C over the band at which the ceiling comes down before the bay fan reaches full speed
CAP_LEAD = 10.0            # a ceiling rises at most this far above the die's allocation (no windup)
# CPU floor (56 busy workers, W-3275M): PL1 80 W still runs at a real clock (1.17 GHz, 76 W);
# at 60 W the package drops under its 1.0 GHz minimum (775 MHz, duty cycling) and work per watt falls 2.42 -> 2.04 M/s/W
# (best 2.77 at 120 W). Any PL1 <= 100 W costs ~3.5 W DC at idle (no further cost lower down), so the governor only
# lowers PL1 when another consumer needs the budget.
CPU_PL1 = {"floor": 80, "max": 413}                    # W
STATIC_SAFE = {"gpu": 125, "pl1": None}                # stop/crash limits; pl1 None = the largest the invariant allows
CPU_KEEP = (1.2, 20.0)     # a CPU under SAT keeps use * 1.2 + 20 W of PL1; the dies may borrow only above that
# Uncapped devices the calibration did not see (e.g. M.2 NVMe carriers and Mellanox ConnectX-5 cards in slots 5-8, 3.5"
# SATA disks in the AUX bay). Each one found at start adds its worst case to BASE;
# datasheet figures until measured: a ConnectX-5 card up to 25 W, an NVMe drive 9 W at full write, a 3.5" disk 25 W
# while spinning up. The Apple internal flash (vendor 0x106b) is part of the calibrated box.
DEVICE_W = {"mellanox": 25.0, "nvme": 9.0, "disk": 25.0}
BUDGET_PERIOD = 4.0        # s between reallocations
SAT, IDLE = 0.93, 0.60     # a consumer drawing >= SAT of its limit wants more; <= IDLE of it can give
SAT_CPU = 0.85             # the CPU's SAT: with PL2 held at PL1, RAPL keeps the package at ~90 % of PL1 (112 W at 124)
STEP_FRAC = 0.20           # each move: at least this fraction of the current limit (GPU >= 5 W, CPU >= 10 W)
BACKSTOP, BACKSTOP_N, BACKSTOP_HOLD = 1190.0, 2, 60.0  # measured DC W, consecutive samples, s at the floors
RAPL_PL1 = "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw"
RAPL_PL2 = "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw"   # short term (2 ms): held at PL1
RAPL_ZONE = "/sys/class/powercap/intel-rapl:0"
GPU_HW_MAX = 200.0         # W: Vega 20's own power1_cap_max; a die whose cap cannot be managed is charged this
CPU_HW_MAX = 413.0         # W: the package's PL2 maximum here; the CPU is charged this while RAPL does not enforce
POWER_TOL = 1e6            # uW: a limit reading back up to 1 W BELOW its target counts as set (never one above it)
LIMIT_MIN = 10.0           # W: no limit is ever written below this (amdgpu takes a 0 cap as "restore the default");
                           # a limit whose value cannot be read is first set to it (see transact()). A limit somebody
                           # else set below 10 W can rise to 10 W in that step: at most K_GPU x 10 W per die and K_CPU x
                           # 10 W for the CPU (charged max(PL1, PL2)) -- 59.76 W with four dies and the default
                           # calibration, inside the default 80 W margin (custom margins/coefficients: check again).
PL2_FRESH = 10.0           # s: an unheld PL2 last read longer ago than this is charged as CPU_HW_MAX
RECONCILE = 2.0            # s between the power worker's read-backs (and re-applies when the hardware differs)
POWER_STUCK = 15.0         # s a power-limit read or write may stay outstanding before it raises an alarm
POWER_FAILING = 30.0       # s the limits may stay unconfirmed (failed writes) before it raises an alarm
STOP_DEADLINE = 10.0       # s for the stop hook's whole power transaction (TimeoutStopSec=15)
DC_DEGRADED = 15.0         # s without a measured DC reading before the budget stops moving power around
RUN_DIR = "/run/macpro-fand"
OWNER_LOCK = RUN_DIR + "/owner.lock"   # held for the life of the controlling process (one hardware owner)
SMC_LOCK = RUN_DIR + "/smc.lock"       # around every two-step SMC key read (shared with other root tools)
RAPL_E = "/sys/class/powercap/intel-rapl:0/energy_uj"
TEST_BUDGET = float(os.environ.get("MACPRO_FAND_TEST_BUDGET", "0") or 0)     # test hook: budget (limit - margin)
TEST_BACKSTOP = float(os.environ.get("MACPRO_FAND_TEST_BACKSTOP", "0") or 0)

# SMC sensors by applesmc label -> (zone fan, target, emergency, band); emergencies are conservative guesses (an
# emergency only means full speed). The PCH keeps its idle temperature (leakage silicon, bottom fan). The memory side
# gets component-health targets instead of idle temperatures: the zone test measured no leakage payoff there (DIMM VR
# power -0.025 W per 100 rpm of blower, which costs +0.39 W per 100 rpm), and idle targets measured at one room
# temperature pinned the blower at its cap at idle once the room warmed (+1.7 C, ~2 W wasted).
# Measured: DIMM proximity 25-28 C idle, 36-37 C under 16-way memory copies; power supply 31-37 C idle, 43 C with
# four dies at 125 W; flash 22 C idle, 35-38 C under sustained reads.
SMC = {
    "TPCD": (BOT, 45.7, 90.0, 1.0),                                  # PCH at 1150 rpm (idle temperature)
    **{f"TM{i}P": (BLOWER, 50.0, 85.0, 1.0) for i in range(9)},      # DIMM proximity
    **{f"Tp{i}P": (BLOWER, 55.0, 80.0, 1.0) for i in range(3)},      # power supply
}
NVME = (BLOWER, 55.0, 1.0)   # internal flash (hwmon "nvme"): zone fan, target, band; emergency = crit - margin
# Production devices' temperatures (health targets; emergency = crit - margin, or the default crit): zone fan by PCI
# slot -- slots 5-8 sit behind the middle front fan. The AUX-bay disks' fan is not measured yet: MID until a zone test
# places them.
SLOT_ZONE = {"5": MID, "6": MID, "7": MID, "8": MID}
PERIPH = {"nvme": (60.0, 1.0, 80.0), "mlx5": (80.0, 1.0, 105.0), "drivetemp": (40.0, 1.0, 60.0)}  # target, band, crit
DISK_ZONE = MID
WRITE_STEP = 25        # rpm; smaller changes are not written to the SMC
READ_FAILS = 3         # consecutive failed reads before a sensor raises an alarm
GPU_STALE = 10.0       # s; GPU readings older than this raise an alarm
FAN_TOL = (150, 0.25)  # a fan more than max(150 rpm, 25 %) off its setting ...
FAN_SETTLE = 20.0      # ... once it has had this long to get there ...
FAN_FAIL = 30.0        # ... for this long raises an alarm
OWNER_FAILS = 5        # consecutive loops with fan registers changed by someone else -> alarm
HANDOVER = 10.0        # s to get every fan to read back manual at its setting after t2fanrd stops
LOG_EVERY = 30.0       # s
DISCOVER = 240.0       # s to wait at start for applesmc and every die's hwmon (amdgpu binds ~57 s into boot, after
                       # resize-amdgpu-bars); the fans are not touched meanwhile. Dies still missing then: alarm.
OVERRIDE = "/run/macpro-fand/override"

# Test hooks (failure-path tests only): stall the GPU reader after N s; report fan N's tach as 0.
TEST_STALL_GPU = float(os.environ.get("MACPRO_FAND_TEST_STALL_GPU", "0") or 0)
TEST_DEAD_FAN = int(os.environ.get("MACPRO_FAND_TEST_DEAD_FAN", "0") or 0)


# --------------------------------------------------------------- helpers
def rds(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def rd(path, scale=1.0):
    try:
        v = float(rds(path))
    except (TypeError, ValueError):
        return None
    return v / scale if math.isfinite(v) else None


def wr(path, val):
    with open(path, "w") as f:
        f.write(f"{val}\n")


def temp_labels(d):
    """label -> sysfs prefix, e.g. 'junction' -> .../temp2"""
    return {rds(p): p[: -len("_label")] for p in glob.glob(d + "/temp*_label")}


def hwmon_named(name):
    return [h for h in sorted(glob.glob("/sys/class/hwmon/hwmon*")) if rds(h + "/name") == name]


def pci_vendor(addr):
    return rds(f"/sys/bus/pci/devices/{addr}/vendor")


def pci_chain(path):
    """PCI addresses (domain:bus:dev.fn) along a sysfs device path, root first."""
    return [c for c in os.path.realpath(path).split("/")
            if len(c) == 12 and c[4] == ":" and c[7] == ":" and c[10] == "."]


def pci_slot(path):
    """The physical slot ("1".."8") a device sits behind, from /sys/bus/pci/slots/*/address ("3-2" is a second link of
    slot 3); None for onboard devices."""
    slots = {rds(a): os.path.basename(os.path.dirname(a)) for a in glob.glob("/sys/bus/pci/slots/*/address")}
    for c in reversed(pci_chain(path)):
        name = (slots.get(c[:-2]) or "").split("-")[0]
        if name in ("1", "2", "3", "4", "5", "6", "7", "8"):
            return name
    return None


def die_map():
    """{short PCI address: MPX bay} of the GPU dies: DIES if configured, else every AMD display device (vendor 0x1002,
    class 0x03xxxx -- present on the bus before amdgpu binds), its bay from its slot (MPX_BAY; elsewhere: bay 2)."""
    if DIES:
        return dict(DIES)
    out = {}
    for d in sorted(glob.glob("/sys/bus/pci/devices/*")):
        if rds(d + "/vendor") == "0x1002" and (rds(d + "/class") or "").startswith("0x03"):
            out[os.path.basename(d)[5:]] = MPX_BAY.get(pci_slot(d), 2)
    return out


VEGA20 = {f"0x66a{i:x}" for i in range(16)}             # PCI device ids of Vega 20 (Radeon Pro Vega II, MI50/60)


def gpu_model_ok(addr):
    """True for a Vega 20 die: GPU_HW_MAX (200 W) and the calibration are Vega 20's."""
    devs = glob.glob("/sys/bus/pci/devices/*:" + addr)
    return bool(devs) and rds(devs[0] + "/device") in VEGA20


def devices():
    """Uncapped devices beyond the calibrated box: {'mellanox': cards, 'nvme': drives, 'disk': rotational disks}."""
    mlx = {a[:-2] for a in (os.path.basename(d) for d in glob.glob("/sys/bus/pci/devices/*"))
           if pci_vendor(a) == "0x15b3" and not os.path.exists(f"/sys/bus/pci/devices/{a}/physfn")}
    nvme = [n for n in glob.glob("/sys/class/nvme/nvme*")
            if pci_vendor((pci_chain(n + "/device") or ["?"])[-1]) != "0x106b"]
    disk = [b for b in glob.glob("/sys/block/sd*") if rds(b + "/queue/rotational") == "1"]
    return {"mellanox": len(mlx), "nvme": len(nvme), "disk": len(disk)}


def base():
    """BASE plus the worst case of every uncapped device found (cached: devices are counted once per process)."""
    global _BASE
    if _BASE is None:
        _BASE = BASE + sum(DEVICE_W[k] * n for k, n in devices().items())
    return _BASE


_BASE = None


def level(g, dies, amount, bound, down):
    """Water-fill `amount` W of die cap out of (down) or into these dies: the highest caps give first / the lowest
    caps get first, each stopping at its bound (a floor going down, a ceiling going up). Returns the W moved."""
    if not dies or amount <= 0:
        return 0.0
    moved = lambda L: sum(max(0.0, g[a] - max(L, bound[a])) if down else max(0.0, min(L, bound[a]) - g[a])
                          for a in dies)
    lo, hi = (min(bound[a] for a in dies), max(g[a] for a in dies)) if down else \
             (min(g[a] for a in dies), max(bound[a] for a in dies))
    if moved(lo if down else hi) <= amount:
        L = lo if down else hi
    else:
        a_, b_ = lo, hi
        for _ in range(60):
            mid = (a_ + b_) / 2
            if (moved(mid) > amount) == down:
                a_ = mid
            else:
                b_ = mid
        L = b_ if down else a_
    total = moved(L)
    for a in dies:
        g[a] = min(g[a], max(L, bound[a])) if down else max(g[a], min(L, bound[a]))
    return total


def open_lock(path):
    """A private lock file: created 0600 in RUN_DIR (root, 0755), never through a symlink, never truncated. Returns
    an fd, or raises OSError if the file is not a regular file owned by this process's user (root for the daemon)."""
    os.makedirs(RUN_DIR, mode=0o755, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    st = os.fstat(fd)
    if not (stat.S_ISREG(st.st_mode) and st.st_uid == os.geteuid()):
        os.close(fd)
        raise OSError(f"{path}: not a regular file owned by uid {os.geteuid()}")
    os.fchmod(fd, 0o600)
    return fd


def flock_within(fd, seconds):
    """Exclusive flock, polling for at most `seconds` (0: one try). True if held."""
    end = time.monotonic() + seconds
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return True
        except OSError:
            if time.monotonic() >= end:
                return False
            time.sleep(0.01)


def find_smc():
    cands = []
    for h in hwmon_named("applesmc"):
        cands += [h, h + "/device"]
    cands += glob.glob("/sys/bus/acpi/devices/APP0001:*") + ["/sys/devices/platform/applesmc.768"]
    return next((c for c in cands if os.path.exists(c + "/fan1_manual")), None)


def ema(old, new, dt, tau):
    return new if old is None else old + (new - old) * dt / (tau + dt)


def cpu_target(w):
    """CPU_TARGET at package power w (W): linear between the points, flat outside; the low end when w is unknown."""
    (w0, t0), (w1, t1) = CPU_TARGET
    if w is None or w <= w0:
        return t0
    return t1 if w >= w1 else t0 + (t1 - t0) * (w - w0) / (w1 - w0)


def energy_rate(prev, now, e, emax):
    """Package W between two RAPL energy readings: prev = [t, uJ]. None when either reading is missing; a counter
    that wrapped is unwrapped with emax (max_energy_range_uj)."""
    if e is None or prev[1] is None or prev[0] is None or now <= prev[0]:
        return None
    de = e - prev[1]
    if de < 0 and emax:
        de += emax
    return de / 1e6 / (now - prev[0]) if de >= 0 else None


def worst(vals):
    vals = [v for v in vals if v is not None]
    return max(vals) if vals else None


def sane(v, default):
    return v if v is not None and 60.0 <= v <= 130.0 else default


def r1(v):
    return None if v is None else round(v, 1)


def notify(msg):
    addr = os.environ.get("NOTIFY_SOCKET")
    if addr:
        if addr.startswith("@"):
            addr = "\0" + addr[1:]
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as s:
            s.connect(addr)
            s.sendall(msg.encode())


CHILD_ENV = {k: v for k, v in os.environ.items() if k != "NOTIFY_SOCKET"}   # children never notify for us


def systemctl(*a, **kw):
    return subprocess.run(["systemctl", *a], env=CHILD_ENV, timeout=10, **kw)


def under_systemd():
    """Running as a service's main process (systemd's child), not merely beside one (INVOCATION_ID is inherited by
    anything a unit starts)."""
    return "INVOCATION_ID" in os.environ and os.getppid() == 1


def unit_active(unit):
    return systemctl("is-active", "--quiet", unit).returncode == 0


def unit_exists(unit):
    return systemctl("cat", unit, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def own_unit_restarting():
    """True when this unit has a start/restart job pending (ExecStopPost of 'systemctl restart'): handing the fans to
    t2fanrd then would, through Conflicts=, cancel our own restart."""
    try:
        cg = open("/proc/self/cgroup").read().strip().split("/")
    except OSError:
        return False
    unit = next((c for c in reversed(cg) if c.endswith(".service")), None)
    if unit is None:
        return False
    jobs = systemctl("list-jobs", "--no-legend", "--no-pager", unit, capture_output=True, text=True).stdout.split()
    return any(t in jobs for t in ("start", "restart", "try-restart", "reload-or-start"))


def log(**kv):
    print(json.dumps(kv), flush=True)


# --------------------------------------------------------------- classes
class Governor:
    """One zone's fan, from its sensors' (reading - target, band) pairs, C. Any sensor over its band: push up at
    UP_GAIN per C of the largest excess. Every sensor under its band: ramp down at DOWN_GAIN per C of the smallest
    margin, clipped to DOWN_MIN..DOWN_MAX. Otherwise hold. The rpm is the only state."""

    def __init__(self, f):
        self.f, self.rpm, self.err, self.rate = f, float(ZONES[f]["init"]), None, 0.0

    def step(self, errs, dt):
        self.err = max((e for e, _ in errs), default=None)
        if not errs:                      # no reading: hold (a failed or stale sensor raises an alarm elsewhere)
            self.rate = 0.0
        elif any(e > b for e, b in errs):
            self.rate = min(UP_MAX, UP_GAIN * max(e for e, b in errs if e > b))
        elif all(e < -b for e, b in errs):
            self.rate = -min(DOWN_MAX, max(DOWN_MIN, DOWN_GAIN * min(-e for e, _ in errs)))
        else:
            self.rate = 0.0
        self.rpm = min(max(self.rpm + self.rate * dt, ZONES[self.f]["floor"]), CAP[self.f])
        return self.rpm


class Filter:
    """Filtered value that tolerates a few failed readings in a row."""

    def __init__(self, tau):
        self.tau = tau
        self.reset()

    def reset(self):
        self.raw = self.val = None
        self.fails = 0

    def feed(self, v, dt):
        if v is None or not math.isfinite(v):
            self.fails += 1
            return self.fails < READ_FAILS
        self.fails, self.raw = 0, v
        self.val = ema(self.val, v, dt, self.tau)
        return True


class Sensor(Filter):
    """Filter over a sysfs file read in the control loop (applesmc, coretemp: never amdgpu)."""

    def __init__(self, path, scale, tau):
        super().__init__(tau)
        self.path, self.scale = path, scale

    def update(self, dt):
        return self.feed(rd(self.path, self.scale), dt)


class Die:
    def __init__(self, addr, bay):
        devs = glob.glob("/sys/bus/pci/devices/*:" + addr)
        hws = glob.glob(devs[0] + "/hwmon/hwmon*") if devs else []
        if not hws:
            raise RuntimeError(f"no amdgpu hwmon for {addr}")
        self.addr, self.bay, self.dev = addr, bay, devs[0]
        lab = temp_labels(hws[0])
        pw = hws[0] + "/power1_average"
        if not os.path.exists(pw):
            pw = hws[0] + "/power1_input"
        self.paths = (lab["junction"] + "_input", lab["mem"] + "_input", pw)
        self.tj, self.tm, self.p = Filter(4.0), Filter(4.0), Filter(10.0)
        self.jcrit = sane(rd(lab["junction"] + "_crit", 1000), 105.0)
        self.mcrit = sane(rd(lab["mem"] + "_crit", 1000), 95.0)

    def read(self):
        """Worker thread only: (junction C, HBM C, power W), None where a read failed."""
        return (rd(self.paths[0], 1000), rd(self.paths[1], 1000), rd(self.paths[2], 1e6))


class GpuReader(threading.Thread):
    """Reads every die once per PERIOD. Each die's reading carries the time its read STARTED and a sequence number,
    so the control loop judges staleness per die (a slow scan cannot make old readings look fresh) and feeds each
    reading to its filters once."""

    def __init__(self, dies):
        super().__init__(daemon=True, name="gpu-reader")
        self.dies, self.lock, self.vals = dies, threading.Lock(), {}
        self.t0 = time.monotonic()

    def run(self):
        seq = 0
        while True:
            if TEST_STALL_GPU and time.monotonic() - self.t0 > TEST_STALL_GPU:
                threading.Event().wait()          # test hook: behave like a read stuck in the driver
            for d in self.dies:
                t = time.monotonic()
                v = d.read()
                seq += 1
                with self.lock:
                    self.vals[d.addr] = (v, t, seq)
            time.sleep(PERIOD)

    def latest(self):
        """{addr: (values, age s, seq)} for every die read so far."""
        with self.lock:
            now = time.monotonic()
            return {a: (v, now - t, q) for a, (v, t, q) in self.vals.items()}


class SmcKey:
    """One SMC key through applesmc-t2's two-step key_at_index interface (select an index, read name and data), under
    SMC_LOCK, which every tool reading SMC keys this way should take: an interleaved reader gets another key's data.
    Lock acquisition is bounded (2 s for the key lookup, 200 ms per read); read() -> float or None, never blocks for
    long. A lookup that failed (lock busy, SMC not answering) is retried in the background every RETRY s."""
    RETRY = 30.0

    def __init__(self, smc, key):
        self.smc, self.key, self.idx = smc, key, None
        self.next_try, self.finder = 0.0, None
        self._lookup()

    def _lookup(self):
        self.next_try = time.monotonic() + self.RETRY
        try:
            n = int(rds(self.smc + "/key_count"))
            fd = open_lock(SMC_LOCK)
        except (TypeError, ValueError, OSError):
            return
        try:
            if not flock_within(fd, 2.0):
                return
            for i in range(n):
                wr(self.smc + "/key_at_index", i)
                if rds(self.smc + "/key_at_index_name") == self.key:
                    self.idx = i
                    break
        except OSError:
            pass
        finally:
            os.close(fd)

    def read(self):
        if self.idx is None:
            if time.monotonic() >= self.next_try and not (self.finder and self.finder.is_alive()):
                self.next_try = time.monotonic() + self.RETRY
                self.finder = threading.Thread(target=self._lookup, daemon=True, name="smc-lookup")
                self.finder.start()
            return None
        try:
            fd = open_lock(SMC_LOCK)
        except OSError:
            return None
        try:
            if not flock_within(fd, 0.2):
                return None
            wr(self.smc + "/key_at_index", self.idx)
            if rds(self.smc + "/key_at_index_name") != self.key:
                return None
            data = open(self.smc + "/key_at_index_data", "rb").read()
            v = struct.unpack("<f", data[:4])[0] if len(data) >= 4 else None
            return v if v is not None and math.isfinite(v) else None
        except OSError:
            return None
        finally:
            os.close(fd)


def io_call(fn, end):
    """Run fn() in a daemon thread, waiting until monotonic `end`: (True, result) if it finished, (False, None) if it
    is still running (a driver call can block for good; the thread is left behind and nothing waits for it) or was
    never started because `end` had already passed."""
    if end - time.monotonic() <= 0:
        return False, None
    res = []
    t = threading.Thread(target=lambda: res.append(fn()), daemon=True)
    t.start()
    t.join(max(0.0, end - time.monotonic()))
    return (True, res[0]) if res else (False, None)


def write_uw(path, uw):
    try:
        wr(path, int(uw))
        return True
    except OSError:
        return False


def is_set(got, target):
    """A limit counts as set when it reads back at its target or up to POWER_TOL below it -- never above it."""
    return got is not None and target - POWER_TOL <= got <= target


def pl_order(lst, pl2_first):
    """PL2 right after PL1 (lowering) or right before it (raising), so PL2 >= PL1 throughout."""
    lst = list(lst)
    if RAPL_PL1 in lst and RAPL_PL2 in lst:
        first, second = (RAPL_PL2, RAPL_PL1) if pl2_first else (RAPL_PL1, RAPL_PL2)
        lst.remove(second)
        lst.insert(lst.index(first) + 1, second)
    return lst


def transact(want, read, write, raise_ok=True):
    """One power transaction in three phases, every write read back; the first write that fails, does not finish or
    does not read back (is_set) stops it, so nothing after it is written:
      1. limits known to be above their target come down (largest decrease first);
      2. limits whose present value could not be read go to LIMIT_MIN, the lowest value ever written -- whatever the
         configured floors are now or were before, so a limit of unknown value can only give headroom, never take it.
         PL2 goes no lower than PL1 now is (the CPU is charged the larger of the two, so that cannot raise its charge);
      3. with every limit confirmed at or below its target, the increases (PL2 before PL1) -- only if raise_ok: a plan
         that does not fit the budget may only lower limits (the transaction then reports "infeasible").
    No limit is raised while any limit is unknown, so no state on the way costs more than the larger of the state
    before and the target -- with one bounded exception: a limit somebody else set below LIMIT_MIN rises to it in
    phase 2 (at most 59.76 W of model with the default four-die calibration, inside the default 80 W margin; a custom
    margin, coefficients or topology need their own check). A target that is not finite or below LIMIT_MIN refuses
    the whole transaction before any write. read(p) -> uW or None; write(p, uW) -> bool. Returns (ok, failed path or None, {path: uW last read or None}).
    """
    bad = [p for p, v in want.items()
           if isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or v < LIMIT_MIN * 1e6]
    if bad:
        return False, bad[0], {}
    cur = {p: read(p) for p in want}
    seen = dict(cur)

    def put(p, v):
        if not write(p, v):
            return False
        seen[p] = cur[p] = read(p)
        return is_set(cur[p], v)
    for p in pl_order(sorted((p for p in want if cur[p] is not None and cur[p] > want[p]),
                             key=lambda p: want[p] - cur[p]), False):
        if not put(p, want[p]):
            return False, p, seen
    for p in pl_order([p for p in want if cur[p] is None], False):
        v = LIMIT_MIN * 1e6
        if p == RAPL_PL2 and cur.get(RAPL_PL1) is not None:
            v = min(want[p], max(v, cur[RAPL_PL1]))
        if not put(p, v):
            return False, p, seen
    up = pl_order([p for p in want if not is_set(cur[p], want[p])], True)
    if up and not raise_ok:
        return False, "infeasible", seen
    for p in up:
        if not put(p, want[p]):
            return False, p, seen
    return True, None, seen


class _Superseded(Exception):
    """A newer allocation, or a stop, arrived: the transaction in progress ends before its next operation."""


class PowerWorker(threading.Thread):
    """Owns every power-limit read and write (amdgpu power1_cap, RAPL PL1/PL2), so a stuck driver call stalls this
    thread only, never the fan loop. Applies the newest desired allocation with transact(); every RECONCILE s it reads
    the limits back and re-applies whenever the hardware differs, so a failed, unconfirmed or overwritten limit is
    retried until it reads back. One operation at a time: an allocation that arrives mid-transaction supersedes it
    before its next operation, and stop() ends all operations (the caller then owns the limits). Read-only paths in
    `watch` are read every cycle, with the time of each read -- PL2 always, whenever PL1 is managed and PL2 is not
    (an unheld PL2 is charged as it reads, so it must keep being read)."""
    live = set()                              # running workers: write_static_safe() takes the limits from them first

    def __init__(self):
        super().__init__(daemon=True, name="power-worker")
        self.cv = threading.Condition()
        self.desired, self.gen, self.applied_gen = None, 0, -1
        self.watch, self.raise_ok = set(), True
        self.seen, self.seen_t = {}, {}       # path -> uW last read (None: unreadable), monotonic time of that read
        self.ok, self.failed, self.fail_since = None, None, None
        self.pending = None                   # (path, "read"|"write", since) while an operation is outstanding
        self.halted = False                   # stop(): no operation is started any more
        self.notes = []
        self.pl2_bad = 0                      # consecutive transactions that failed on PL2

    def set(self, want, watch=(), raise_ok=True):
        """The desired limits; `raise_ok` False (the allocation does not fit the budget): only decreases are made."""
        with self.cv:
            if want != self.desired or set(watch) != self.watch or raise_ok != self.raise_ok:
                self.desired, self.watch, self.raise_ok, self.gen = dict(want), set(watch), raise_ok, self.gen + 1
                self.cv.notify_all()

    def status(self):
        with self.cv:
            notes, self.notes = self.notes, []
            return {"ok": self.ok, "failed": self.failed, "fail_since": self.fail_since, "pending": self.pending,
                    "seen": dict(self.seen), "seen_t": dict(self.seen_t), "notes": notes, "pl2_bad": self.pl2_bad}

    def stop(self, timeout):
        """Start no further operation and wait (at most `timeout` s) for the one in progress. True: the worker is idle
        for good and the caller owns the limits. False: an operation is still stuck in the driver -- the caller must
        not start a competing transaction (it could be overtaken by the stuck write)."""
        with self.cv:
            self.halted = True
            self.cv.notify_all()
            return self.cv.wait_for(lambda: self.pending is None, timeout)

    def _op(self, p, kind, gen, fn):
        with self.cv:                         # checked and marked under the lock: stop() cannot slip in between
            if self.halted or self.gen != gen:
                raise _Superseded
            self.pending = (p, kind, time.monotonic())
        try:
            return fn()
        finally:
            with self.cv:
                self.pending = None
                self.cv.notify_all()

    def run(self):
        PowerWorker.live.add(self)
        try:
            self._run()
        finally:
            PowerWorker.live.discard(self)

    def _run(self):
        while True:
            with self.cv:
                if self.applied_gen == self.gen and not self.halted:
                    self.cv.wait(timeout=RECONCILE)
                if self.halted:
                    return
                want, gen, watch = (dict(self.desired) if self.desired else None), self.gen, set(self.watch)
                raise_ok, before = self.raise_ok, dict(self.seen)
            if want is None:
                continue
            if RAPL_PL1 in want and RAPL_PL2 not in want:
                watch.add(RAPL_PL2)
            found, obs, read_t = {}, {}, {}

            def read(p):
                v = self._op(p, "read", gen, lambda: rd(p))
                read_t[p] = time.monotonic()          # each value carries the time it was actually read
                found.setdefault(p, v)                # transact() reads every limit before writing any
                return v
            try:
                ok, failed, seen = transact(want, read, lambda p, uw: self._op(p, "write", gen, lambda: write_uw(p, uw)),
                                            raise_ok)
                for p in watch - set(want):
                    obs[p] = read(p)
            except _Superseded:
                continue
            now = time.monotonic()
            with self.cv:
                if gen == self.applied_gen:           # nothing new was asked: a difference is someone else's write
                    ext = [p for p in want if before.get(p) is not None and found.get(p) is not None
                           and is_set(before[p], want[p]) and not is_set(found[p], want[p])]
                    if ext:
                        self.notes.append(f"limits changed by someone else, ours restored: {ext}")
                for p, v in list(seen.items()) + list(obs.items()):
                    if p in read_t:
                        self.seen[p], self.seen_t[p] = v, read_t[p]
                self.ok, self.failed, self.applied_gen = ok, failed, gen
                self.fail_since = None if ok else (self.fail_since or now)
                self.pl2_bad = self.pl2_bad + 1 if failed == RAPL_PL2 else 0


def pl2_observed(st, now):
    """PL2 (uW) as the power worker last read it, if that read is at most PL2_FRESH s old; otherwise None (an unheld
    PL2 is then charged as CPU_HW_MAX)."""
    t = st["seen_t"].get(RAPL_PL2) if st else None
    return st["seen"].get(RAPL_PL2) if t is not None and now - t <= PL2_FRESH else None


def rapl_state():
    """(enforcing, pl2 present): the package zone is enabled and its constraints are long_term / short_term."""
    enabled = rds(RAPL_ZONE + "/enabled") == "1"
    names = (rds(RAPL_ZONE + "/constraint_0_name"), rds(RAPL_ZONE + "/constraint_1_name"))
    return enabled and names[0] == "long_term", names[1] == "short_term"


def budget_ready():
    return None not in (BASE, K_GPU, K_CPU)


def static_safe(n_managed=None, n_unmanaged=0, cpu_min=0.0):
    """{'gpu': W per managed die, 'pl1': W, 'fits': bool}: the limits used when the daemon is not running (stop hook),
    whole watts, for the dies actually present: unmanaged dies are charged GPU_HW_MAX, and the CPU max(PL1, cpu_min)
    -- CPU_HW_MAX while RAPL does not enforce, PL2 as read when PL2 cannot be held at PL1. The managed dies come down
    from STATIC_SAFE's cap (to their floor at the lowest) until the CPU's floor fits beside them."""
    if n_managed is None:
        n_managed = len(die_map())
    lim = BUDGET_LIMIT - BUDGET_MARGIN
    fixed = base() + K_GPU * GPU_HW_MAX * n_unmanaged
    cost = lambda g, pl1: fixed + K_GPU * n_managed * g + K_CPU * max(pl1, cpu_min)
    g = float(math.floor(min(STATIC_SAFE["gpu"], GPU_CAP["max"])))
    while n_managed and cost(g, CPU_PL1["floor"]) > lim and g > GPU_CAP["floor"]:
        g -= 1.0
    pl1 = min(CPU_PL1["max"], (lim - fixed - K_GPU * n_managed * g) / K_CPU)
    if STATIC_SAFE["pl1"] is not None:
        pl1 = min(pl1, float(STATIC_SAFE["pl1"]))
    pl1 = float(math.floor(max(CPU_PL1["floor"], pl1)))
    return {"gpu": g, "pl1": pl1, "fits": cost(g, pl1) <= lim + 1e-6}


def static_want(cap_paths, ss, pl2):
    """The static-safe allocation as desired limits (uW); PL2 = PL1 when PL2 is managed."""
    w = {p: ss["gpu"] * 1e6 for p in cap_paths}
    w[RAPL_PL1] = ss["pl1"] * 1e6
    if pl2:
        w[RAPL_PL2] = w[RAPL_PL1]
    return w


def cap_paths_now():
    """{die: power1_cap path} for every die of die_map() whose hwmon is present, and the dies without one."""
    found, missing = {}, []
    for a in die_map():
        caps = glob.glob(f"/sys/bus/pci/devices/*:{a}/hwmon/hwmon*/power1_cap")
        if caps:
            found[a] = caps[0]
        else:
            missing.append(a)
    return found, missing


def write_static_safe():
    """Stop hook: GPU caps and PL1/PL2 to their static-safe values by the transaction rule (transact()), every read
    and write bounded and none started after STOP_DEADLINE. The CPU is charged the same way as in normal operation:
    CPU_HW_MAX while RAPL does not enforce or has no short-term limit; if PL2 cannot be set to PL1, PL2 as read back
    (the dies come down for it). Operations that did not finish are logged. -> True when every limit is confirmed and
    the allocation fits; None without a calibration."""
    if not budget_ready():
        return None
    end = time.monotonic() + STOP_DEADLINE
    for w in list(PowerWorker.live):                     # one writer: a live worker in this process stops first
        if not w.stop(max(0.0, end - time.monotonic())):
            log(static_safe={"ok": False, "failed": "power worker stuck", "pending": w.status()["pending"]})
            return False
    unresolved = []

    def call(fn, what):
        done, v = io_call(fn, end)
        if not done:
            unresolved.append(what)
        return v if done else None
    found, missing = call(cap_paths_now, "find power caps") or ({}, list(die_map()))
    enforcing, pl2_present = call(rapl_state, "RAPL state") or (False, False)
    pl2_present = pl2_present and os.path.exists(RAPL_PL2)
    read = lambda p: call(lambda: rd(p), "read " + p)
    write = lambda p, uw: bool(call(lambda: write_uw(p, uw), "write " + p))
    cpu_min = 0.0 if enforcing and pl2_present else CPU_HW_MAX
    ss = static_safe(len(found), len(missing), cpu_min)          # a plan that does not fit only lowers limits
    ok, failed, seen = transact(static_want(found.values(), ss, pl2_present), read, write, ss["fits"])
    if not ok and failed == RAPL_PL2:                    # PL2 will not follow PL1: charge it as it reads, without it
        pl2 = seen.get(RAPL_PL2)
        cpu_min = max(cpu_min, CPU_HW_MAX if pl2 is None else min(CPU_HW_MAX, pl2 / 1e6))
        ss = static_safe(len(found), len(missing), cpu_min)
        ok, failed, seen2 = transact(static_want(found.values(), ss, False), read, write, ss["fits"])
        seen.update(seen2)
    log(static_safe={"gpu": ss["gpu"], "pl1": ss["pl1"], "cpu_charged_w": max(ss["pl1"], cpu_min), "fits": ss["fits"],
                     "ok": ok, "failed": failed, "unmanaged_dies": missing, "unresolved": unresolved,
                     "read_back_w": {p: (None if v is None else round(v / 1e6, 1)) for p, v in seen.items()}})
    return bool(ok and ss["fits"])


class Budget:
    """The 1228 W invariant over the GPU die caps and the CPU's PL1 (see the module docstring). The planner only:
    PowerWorker writes the limits. Charged at their worst case: a die whose cap cannot be managed (GPU_HW_MAX), the
    CPU at the larger of PL1 and a PL2 that could not be held at PL1, and at CPU_HW_MAX while RAPL does not enforce."""

    def __init__(self, cap_paths, n_unmanaged=0):
        self.paths = cap_paths                                # {die addr: power1_cap path}
        self.n_unmanaged = n_unmanaged
        self.limit = TEST_BUDGET or (BUDGET_LIMIT - BUDGET_MARGIN)
        self.backstop = TEST_BACKSTOP or BACKSTOP
        self.gpu = {a: float(min(GPU_CAP["init"], GPU_CAP["max"])) for a in cap_paths}
        self.pl1 = float(CPU_PL1["max"])                      # fit() brings it down to what the invariant allows
        self.pin = None                                       # (GPU W or None, PL1 W or None) while pinned
        self.reset_pending = False                            # a pin ended during the backstop hold
        self.tcap = {a: min(float(GPU_CAP["max"]), GPU_CAP["init"] + CAP_LEAD) for a in cap_paths}   # see thermal()
        self.cpu_min = 0.0                                    # W the CPU is charged at least (unheld PL2, no RAPL)
        self.pl2_held = True
        self.degraded = False                                 # no measured DC: demand-driven moves pause
        self.over_n, self.floor_until = 0, 0.0
        self.dc = None
        self.fit()

    def reset(self):
        """Back to the default allocation (GPUs at their init cap, PL1 as large as the invariant allows): at start and
        when an override pin ends (without demand the rebalancer would leave the pinned limits in place)."""
        self.gpu = {a: float(min(GPU_CAP["init"], GPU_CAP["max"])) for a in self.paths}
        self.pl1 = float(CPU_PL1["max"])
        self.fit()

    def fixed(self):
        return base() + K_GPU * GPU_HW_MAX * self.n_unmanaged

    def cost(self, gpu=None, pl1=None):
        gpu = self.gpu if gpu is None else gpu
        return self.fixed() + K_GPU * sum(gpu.values()) + K_CPU * max(self.pl1 if pl1 is None else pl1, self.cpu_min)

    def cpu_w(self):
        """What the CPU is charged: PL1, or more while it can draw above PL1 (see cpu_charge())."""
        return max(self.pl1, self.cpu_min)

    def room(self, gpu):
        """Largest PL1 the invariant allows beside these GPU caps (clipped to the CPU's range)."""
        return min(CPU_PL1["max"], (self.limit - self.fixed() - K_GPU * sum(gpu.values())) / K_CPU)

    def fit(self, pl1_floor=None):
        """Make the current allocation satisfy the invariant, the CPU charged max(PL1, cpu_min): PL1 first down to its
        floor (or a pin's lower value), then the GPUs evenly. True if it fits; False if even every floor does not (the
        caller alarms)."""
        self.pl1 = max(CPU_PL1["floor"] if pl1_floor is None else pl1_floor, min(self.pl1, self.room(self.gpu)))
        while self.cost() > self.limit + 1e-6:
            hi = [a for a in self.gpu if self.gpu[a] > GPU_CAP["floor"]]
            if not hi:
                return False
            for a in hi:
                self.gpu[a] = max(GPU_CAP["floor"], self.gpu[a] - 1)
        return True

    def cpu_charge(self, pl2_uw, enforcing):
        """Charge the CPU for what it can actually draw, whatever PL1 is set to: PL2 as read back when PL2 is not held
        at PL1, CPU_HW_MAX when RAPL does not enforce (or PL2 is neither held nor readable). The GPUs make room for it.
        Returns True if the charge changed (the allocation was refitted)."""
        if not enforcing or (not self.pl2_held and pl2_uw is None):
            low = CPU_HW_MAX
        elif not self.pl2_held:
            low = min(CPU_HW_MAX, pl2_uw / 1e6)
        else:
            low = 0.0
        if abs(low - self.cpu_min) < 0.5:
            return False
        self.cpu_min = low
        self.fit()
        return True

    def want(self):
        """The desired limits, uW, for PowerWorker: whole watts, rounded down, so the limits written never cost more than
        the allocation the invariant was checked on."""
        w = {p: math.floor(self.gpu[a]) * 1e6 for a, p in self.paths.items()}
        w[RAPL_PL1] = math.floor(self.pl1) * 1e6
        if self.pl2_held:
            w[RAPL_PL2] = w[RAPL_PL1]
        return w

    def end_pin(self, now):
        """An override pin ended: back to the default allocation, but never during the backstop hold."""
        if now < self.floor_until:
            self.reset_pending = True
        else:
            self.reset()

    def thermal(self, dies, fan_full, dt):
        """Working-temperature ceilings: over T_WORK (or HBM_WORK) + band a die's ceiling comes down from its present
        draw -- once its bay fan is at full speed, or at once CAP_EARLY over; under - band it goes back up slowly
        (to at most CAP_LEAD over the allocation); inside the band it holds.
        dies: {addr: (junction C, HBM C, W, bay)}."""
        for a, (tj, tm, w, bay) in dies.items():
            if tj is None or a not in self.tcap:
                continue
            err = max(tj - T_WORK[0], (tm - HBM_WORK) if tm is not None else tj - T_WORK[0])
            if err > T_WORK[1] and (fan_full.get(bay) or err > T_WORK[1] + CAP_EARLY):
                start = min(self.tcap[a], w + 5.0) if w is not None else self.tcap[a]
                self.tcap[a] = start - min(CAP_DOWN[1], CAP_DOWN[0] * (err - T_WORK[1])) * dt
            elif err < -T_WORK[1] and self.tcap[a] < self.gpu[a] + CAP_LEAD:    # no windup: a ceiling only rises
                self.tcap[a] = min(self.gpu[a] + CAP_LEAD,                       # while it is near the allocation
                                   self.tcap[a] + min(CAP_UP[1], CAP_UP[0] * (-err - T_WORK[1])) * dt)
            self.tcap[a] = min(float(GPU_CAP["max"]), max(float(GPU_CAP["floor"]), self.tcap[a]))

    def backstop_check(self, dc, now):
        """Measured DC over BACKSTOP on BACKSTOP_N samples in a row: every limit to its floor, hold BACKSTOP_HOLD s."""
        self.dc = dc
        self.over_n = self.over_n + 1 if (dc is not None and dc > self.backstop) else 0
        if self.over_n >= BACKSTOP_N:
            self.gpu = {a: float(GPU_CAP["floor"]) for a in self.gpu}
            self.pl1 = float(CPU_PL1["floor"])
            self.floor_until = now + BACKSTOP_HOLD
            self.over_n = 0
            return True
        return False

    def rebalance(self, now, die_w, cpu_w):
        """CPU first. A saturated CPU (>= SAT of PL1) gets at least its default share at once, then
        STEP_FRAC more per move, funded from free budget, then idle dies (toward their use + 30 %), then every die
        evenly. Saturated dies get free budget, idle siblings' budget, and the CPU's PL1 above CPU_KEEP of its use --
        never more, so a busy CPU keeps its headroom. Returns a short log string or None."""
        if now < self.floor_until:                            # the backstop hold outranks pins and resets
            return None
        if self.reset_pending:
            self.reset_pending = False
            self.reset()
            return "backstop hold over: default allocation restored"
        if self.pin is not None:                              # (GPU W or None, PL1 W or None): fixed limits, fitted
            gp, pp = self.pin                                 # like any allocation (a pin that does not fit shrinks;
            self.gpu = {a: float(gp) for a in self.gpu} if gp else dict(self.gpu)   # the CPU's charge counts)
            self.pl1 = float(pp) if pp else float(CPU_PL1["max"])
            self.fit(min(pp, CPU_PL1["floor"]) if pp else None)
            return None
        if self.degraded:                                     # no measured DC: demand-driven moves pause
            return None
        lo = GPU_CAP["floor"]
        top = {a: min(self.tcap[a], float(GPU_CAP["max"])) for a in self.gpu}   # thermal ceiling, never over the max
        g = {a: min(v, top[a]) for a, v in self.gpu.items()}   # a hot die's ceiling binds at once
        pl1 = self.pl1
        free = lambda: self.limit - self.cost(g, pl1)
        idle = [a for a in g if die_w.get(a) is not None and die_w[a] <= IDLE * g[a] and g[a] > lo]
        cpu_sat = cpu_w is not None and cpu_w >= SAT_CPU * pl1 and pl1 < CPU_PL1["max"]
        if cpu_sat:
            dflt = max(CPU_PL1["floor"], self.room({a: float(GPU_CAP["init"]) for a in g}))
            target = min(CPU_PL1["max"], max(dflt, pl1 + max(10.0, STEP_FRAC * pl1)))
            need = (K_CPU * (target - pl1) - max(0.0, free())) / K_GPU     # die W to give
            need -= level(g, idle, need, {a: max(lo, die_w[a] * 1.3) for a in idle}, True)
            level(g, list(g), need, {a: lo for a in g}, True)             # then every die evenly
            pl1 = min(target, pl1 + max(0.0, free()) / K_CPU)
        else:
            want = [a for a in g if die_w.get(a) is not None and die_w[a] >= SAT * g[a] and g[a] < top[a] - 0.5]
            for a in idle if want else []:
                g[a] = max(lo, die_w[a] * 1.3, g[a] - max(5.0, STEP_FRAC * g[a]))
            keep = pl1 if cpu_w is None else max(CPU_PL1["floor"], cpu_w * CPU_KEEP[0] + CPU_KEEP[1])
            ceil = {a: min(top[a], g[a] + max(5.0, STEP_FRAC * g[a])) for a in want}   # (empty: no-op)
            need = K_GPU * sum(ceil[a] - g[a] for a in want)
            short = need - max(0.0, free())
            if short > 0 and pl1 > keep:                       # the CPU lends only what it does not use
                pl1 -= min(pl1 - keep, short / K_CPU)
            level(g, want, max(0.0, free()) / K_GPU, ceil, False)            # the lowest caps first
        pl1 = min(CPU_PL1["max"], pl1 + max(0.0, free()) / K_CPU)     # whatever the dies do not hold: the CPU's
        if self.cost(g, pl1) > self.limit + 1e-6:              # never: guard against arithmetic slips
            return "rebalance skipped: invariant"
        changed = abs(pl1 - self.pl1) >= 1 or any(abs(g[a] - self.gpu[a]) >= 1 for a in g)
        self.gpu, self.pl1 = g, pl1
        why = "cpu wants" if cpu_sat else ("gpu wants" if want else "thermal ceilings")
        return f"rebalance ({why}): gpu {[round(v) for v in g.values()]} pl1 {round(pl1)}" if changed else None


def finite(word):
    """float(word), ValueError unless finite: NaN passes every min()/max() clamp unchanged."""
    v = float(word)
    if not math.isfinite(v):
        raise ValueError(word)
    return v


def parse_override():
    """-> (mode, rpm dict or None, note) from OVERRIDE; ('auto', None, '') when absent, expired or malformed."""
    try:
        st = os.stat(OVERRIDE)
        with open(OVERRIDE, encoding="utf-8") as f:
            words = f.read().split()
    except FileNotFoundError:
        return "auto", None, "", None
    except (OSError, ValueError):                          # unreadable, or not UTF-8 text
        return "auto", None, "override ignored (unreadable or not text)", None
    ttl = [w for w in words if w.startswith("ttl=")]
    pin = [w for w in words if w.startswith("gpu=")]
    ppin = [w for w in words if w.startswith("pl1=")]
    words = [w for w in words if not w.startswith(("ttl=", "gpu=", "pl1="))]
    try:
        pin = min(max(finite(pin[0][4:]), GPU_CAP["floor"]), GPU_CAP["max"]) if pin else None
    except ValueError:
        pin = None
    try:
        ppin = min(max(finite(ppin[0][4:]), 10.0), CPU_PL1["max"]) if ppin else None   # may go under the floor
    except ValueError:
        ppin = None
    pin = (pin, ppin) if (pin is not None or ppin is not None) else None
    try:
        if ttl and time.time() - st.st_mtime > finite(ttl[0][4:]):
            return "auto", None, "override expired", None
        pn = (f" gpu={pin[0]:.0f}" if pin and pin[0] else "") + (f" pl1={pin[1]:.0f}" if pin and pin[1] else "")
        if not words or words[0] == "auto":
            return "auto", None, ("override" + pn) if pin else "", pin
        if words[0] == "max" and len(words) == 1:
            return "max", None, "override max" + pn, pin
        if words[0] == "hold" and len(words) == 1 + len(CAP):
            rpm = {f: min(max(finite(w), MIN_RPM[f]), CAP[f]) for f, w in zip(sorted(CAP), words[1:])}
            return "hold", rpm, "override hold" + pn, pin
    except ValueError:
        pass
    return "auto", None, f"override ignored (malformed: {' '.join(words)[:60]})", None


# ------------------------------------------------------------------ main
CONFIGURABLE = ("ZONES", "BAY_TARGET", "HBM_TARGET", "CPU_TARGET", "CPU_BAND", "CPU_TARGET_TAU", "BAND", "SMC", "NVME",
                "BUDGET_LIMIT", "BUDGET_MARGIN", "BASE", "K_GPU", "K_CPU", "GPU_CAP", "CPU_PL1", "STATIC_SAFE",
                "CPU_KEEP", "T_WORK", "HBM_WORK", "DEVICE_W", "SLOT_ZONE", "PERIPH", "DISK_ZONE", "DIES", "MPX_BAY",
                "BACKSTOP")


def read_config(path):
    """NAME = value settings (Python literals) for the names in CONFIGURABLE; '#' starts a comment and an indented line
    continues the setting above it. A dict setting updates the default entry by entry (ZONES = {4: {"floor": 700}}
    changes one floor). -> (candidate {name: value}, errors); nothing is committed."""
    import ast
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        return {}, []
    except (OSError, ValueError) as e:                     # unreadable, or not UTF-8
        return {}, [f"{path}: {e}"]
    errors, cand, items = [], {}, []
    for n, line in enumerate(lines, 1):                    # an indented line continues the setting above it
        text = "" if line.lstrip().startswith("#") else line.split("#", 1)[0].rstrip()
        if text.strip() and text[0] in " \t" and items:
            items[-1][1] += " " + text.strip()
        elif text.strip():
            items.append([n, text.strip()])
    for n, line in items:
        name, eq, val = line.partition("=")
        name = name.strip()
        try:
            if not eq or name not in CONFIGURABLE:
                raise ValueError(f"unknown setting {name!r}")
            v = ast.literal_eval(val.strip())
            old = cand.get(name, globals()[name])
            if isinstance(old, dict):
                if not isinstance(v, dict):
                    raise ValueError(f"{name} must be a dict")
                v = {**old, **{k: ({**old[k], **x} if isinstance(old.get(k), dict) and isinstance(x, dict) else x)
                               for k, x in v.items()}}
            cand[name] = v
        except Exception as e:                              # noqa: BLE001 -- any malformed literal is an error, not
            errors.append(f"{path}:{n}: {type(e).__name__}: {e}")   # a crash (the stop hook reads this file too)
    return cand, errors


def _num(v, lo=-math.inf, hi=math.inf):
    try:
        return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v) and lo <= v <= hi
    except (OverflowError, TypeError):                     # an int too large for a float
        return False


def _fan(v):
    """A fan number (checked by type first: an unhashable value must not raise in a membership test)."""
    return isinstance(v, int) and not isinstance(v, bool) and v in CAP


def check_config(cand):
    """Every setting's shape and range, and the budget's feasibility, for the candidate merged over the defaults.
    -> errors (empty: safe to commit). Never raises: an unexpected shape is an error."""
    try:
        return _check_config(cand)
    except Exception as e:                                 # noqa: BLE001
        return [f"configuration rejected: {type(e).__name__}: {e}"]


def _check_config(cand):
    c = {k: cand.get(k, globals()[k]) for k in CONFIGURABLE}
    e = []
    fans = set(CAP)

    def need(ok, msg):
        if not ok:
            e.append(msg)
    z = c["ZONES"]
    need(isinstance(z, dict) and all(_fan(f) for f in z) and set(z) == fans, f"ZONES must define fans {sorted(fans)}")
    if isinstance(z, dict):
        for f, d in z.items():
            ok = (_fan(f) and isinstance(d, dict) and isinstance(d.get("name", ""), str)
                  and _num(d.get("floor"), MIN_RPM.get(f, 0), CAP.get(f, 0))
                  and _num(d.get("init"), MIN_RPM.get(f, 0), CAP.get(f, 0)))
            need(ok, f"ZONES[{f!r}]: floor and init in rpm, {MIN_RPM.get(f)}..{CAP.get(f)}")
    for k, lo, hi in (("BAY_TARGET", 20, 90), ("HBM_TARGET", 20, 95), ("BAND", 0.1, 5), ("CPU_BAND", 0.1, 5),
                      ("CPU_TARGET_TAU", 1, 600), ("HBM_WORK", 40, 95), ("BUDGET_LIMIT", 100, 2000),
                      ("K_GPU", 0.5, 2.0), ("K_CPU", 0.5, 2.0)):
        need(_num(c[k], lo, hi), f"{k} must be a number in {lo}..{hi}")
    lim = c["BUDGET_LIMIT"] if _num(c["BUDGET_LIMIT"]) else 0
    need(_num(c["BUDGET_MARGIN"], 0, lim / 2), "BUDGET_MARGIN must be 0..BUDGET_LIMIT/2")
    need(_num(c["BASE"], 0, lim), "BASE must be 0..BUDGET_LIMIT")
    need(_num(c["BACKSTOP"], 100, lim), "BACKSTOP must be 100..BUDGET_LIMIT")
    t = c["CPU_TARGET"]
    need(isinstance(t, tuple) and len(t) == 2 and all(isinstance(p, tuple) and len(p) == 2 and _num(p[0], 0, 500)
                                                       and _num(p[1], 20, 90) for p in t) and t[0][0] < t[1][0],
         "CPU_TARGET must be ((W, C), (W, C)) with W rising, C 20..90")
    t = c["T_WORK"]
    need(isinstance(t, tuple) and len(t) == 2 and _num(t[0], 40, 95) and _num(t[1], 0.1, 5),
         "T_WORK must be (C 40..95, band 0.1..5)")
    t = c["CPU_KEEP"]
    need(isinstance(t, tuple) and len(t) == 2 and _num(t[0], 1, 3) and _num(t[1], 0, 200),
         "CPU_KEEP must be (factor 1..3, W 0..200)")
    whole = lambda v: _num(v) and float(v).is_integer()   # limits are written in whole watts
    g = c["GPU_CAP"]
    need(not isinstance(g, dict) or all(whole(g.get(k)) for k in ("floor", "max", "init") if _num(g.get(k))),
         "GPU_CAP values must be whole watts")
    gf = g.get("floor") if isinstance(g, dict) and _num(g.get("floor")) else GPU_HW_MAX
    need(isinstance(g, dict) and set(g) == {"floor", "max", "init"} and _num(g["floor"], 50, GPU_HW_MAX)
         and _num(g["max"], gf, GPU_HW_MAX) and _num(g["init"], gf, GPU_HW_MAX),
         f"GPU_CAP must be {{floor, max, init}} W with 50 <= floor <= max <= {GPU_HW_MAX:.0f} and floor <= init "
         f"(init above max is lowered to max)")
    p = c["CPU_PL1"]
    need(not isinstance(p, dict) or all(whole(p.get(k)) for k in ("floor", "max") if _num(p.get(k))),
         "CPU_PL1 values must be whole watts")
    need(isinstance(p, dict) and set(p) == {"floor", "max"} and _num(p["floor"], LIMIT_MIN, CPU_HW_MAX)
         and _num(p["max"], p["floor"] if _num(p["floor"]) else CPU_HW_MAX, CPU_HW_MAX),
         f"CPU_PL1 must be {{floor, max}} W with {LIMIT_MIN:.0f} <= floor <= max <= {CPU_HW_MAX:.0f}")
    pf = p.get("floor") if isinstance(p, dict) and _num(p.get("floor")) else CPU_HW_MAX
    ss = c["STATIC_SAFE"]
    need(not isinstance(ss, dict) or all(whole(ss.get(k)) for k in ("gpu", "pl1") if _num(ss.get(k))),
         "STATIC_SAFE values must be whole watts")
    need(isinstance(ss, dict) and set(ss) == {"gpu", "pl1"} and _num(ss["gpu"], gf, GPU_HW_MAX)
         and (ss["pl1"] is None or _num(ss["pl1"], pf, CPU_HW_MAX)),
         "STATIC_SAFE must be {gpu: W, pl1: W or None}, gpu no lower than GPU_CAP's floor, pl1 no lower than CPU_PL1's "
         "(gpu above GPU_CAP's max is lowered to it)")
    sm = c["SMC"]
    need(isinstance(sm, dict) and all(isinstance(k, str) and isinstance(v, tuple) and len(v) == 4 and _fan(v[0])
                                      and _num(v[1], 0, 120) and _num(v[2], v[1] if _num(v[1]) else 0, 130)
                                      and _num(v[3], 0.1, 5) for k, v in sm.items()),
         "SMC entries must be key: (fan, target C, emergency C > target, band)")
    nv = c["NVME"]
    need(isinstance(nv, tuple) and len(nv) == 3 and _fan(nv[0]) and _num(nv[1], 0, 100) and _num(nv[2], 0.1, 5),
         "NVME must be (fan, target C, band)")
    pe = c["PERIPH"]
    need(isinstance(pe, dict) and all(isinstance(kk, str) and isinstance(v, tuple) and len(v) == 3 and _num(v[0], 0, 120) and _num(v[1], 0.1, 5)
                                      and _num(v[2], 30, 150) for kk, v in pe.items()),
         "PERIPH entries must be driver: (target C, band, critical C)")
    dw = c["DEVICE_W"]
    need(isinstance(dw, dict) and all(isinstance(k, str) for k in dw) and set(dw) >= {"mellanox", "nvme", "disk"}
         and all(_num(v, 0, 500) for v in dw.values()),
         "DEVICE_W must give W >= 0 for mellanox, nvme and disk")
    need(isinstance(c["SLOT_ZONE"], dict) and all(isinstance(k, str) and _fan(v) for k, v in c["SLOT_ZONE"].items()),
         "SLOT_ZONE must map slot names to fans")
    need(_fan(c["DISK_ZONE"]), "DISK_ZONE must be a fan")
    for k in ("DIES", "MPX_BAY"):
        need(isinstance(c[k], dict) and all(isinstance(a, str) and b in (1, 2) and not isinstance(b, bool)
                                            for a, b in c[k].items()), f"{k} values must be MPX bay 1 or 2")
    if not e:                                   # feasibility: every floor together fits the budget
        n = len(c["DIES"]) if c["DIES"] else len(die_map())
        fixed = c["BASE"] + sum(c["DEVICE_W"].get(k, 0) * v for k, v in devices().items())
        floor_cost = fixed + c["K_GPU"] * n * c["GPU_CAP"]["floor"] + c["K_CPU"] * c["CPU_PL1"]["floor"]
        need(floor_cost <= c["BUDGET_LIMIT"] - c["BUDGET_MARGIN"],
             f"budget infeasible: every limit at its floor costs {floor_cost:.0f} W > "
             f"{c['BUDGET_LIMIT'] - c['BUDGET_MARGIN']:.0f} W")
    return e


def load_config(path):
    """Read, validate and -- only if there is no error at all -- commit the settings. -> errors. Never raises: the
    stop hook must reach its recovery with the built-in defaults whatever the file holds."""
    try:
        cand, errors = read_config(path)
        errors += check_config(cand)
    except Exception as e:                                 # noqa: BLE001
        return [f"{path}: {type(e).__name__}: {e}"]
    if not errors:
        globals().update(cand)
    return errors


def fans_full(smc):
    """Every fan to its cap. Never the SMC's automatic mode."""
    for f in CAP:
        try:
            wr(f"{smc}/fan{f}_manual", 1)
            wr(f"{smc}/fan{f}_output", CAP[f])
        except OSError as e:
            log(error=f"fan{f} write failed: {e}")


def stop_hook(smc):
    """--stopped (ExecStopPost). Only a stop of the hardware owner recovers: the hook takes the owner lock, and if
    another macpro-fand holds it (this run was refused because that one owns the fans and limits), it leaves them
    alone. Otherwise, whatever the configuration file holds: every fan to its cap, the static-safe limits, and after a
    clean stop t2fanrd (unless this stop is half of a restart)."""
    try:
        fd = open_lock(OWNER_LOCK)
        owned = flock_within(fd, 3.0)
    except OSError as e:
        log(error=f"stop hook: {OWNER_LOCK}: {e}; recovering anyway")
        owned = True
    if not owned:
        log(note="stop hook: another macpro-fand owns the fans and power limits: left alone")
        return
    if smc:
        fans_full(smc)
    try:
        ok = write_static_safe()
    except Exception as e:                                 # noqa: BLE001 -- the fans are already at their caps
        log(error=f"stop hook: power recovery failed: {type(e).__name__}: {e}")
        ok = False
    if ok is False:
        log(error="stop hook: static-safe power limits not confirmed (fans at their caps)")
    if smc and ok is not False and os.environ.get("SERVICE_RESULT") == "success":
        if unit_exists("t2fanrd.service") and not own_unit_restarting():
            systemctl("start", "--no-block", "t2fanrd.service")


def main():
    ap = argparse.ArgumentParser(prog="macpro-fand", description="Mac Pro 7,1 zone fan control and power budget")
    mode_arg = ap.add_mutually_exclusive_group()
    mode_arg.add_argument("--dry-run", action="store_true", help="compute and log, write no fan or power limit")
    mode_arg.add_argument("--release", action="store_true", help="hand the fans to t2fanrd (every fan to its cap first)")
    mode_arg.add_argument("--stopped", action="store_true", help="systemd ExecStopPost hook")
    mode_arg.add_argument("--check-config", action="store_true", help="validate the configuration and exit")
    ap.add_argument("--config", default=CONFIG, help=f"configuration file (default {CONFIG})")
    ap.add_argument("--version", action="version", version=f"%(prog)s {VERSION}")
    args = ap.parse_args()
    cfg_errors = load_config(args.config)
    for e in cfg_errors:
        log(error=f"configuration: {e}")
    if args.check_config:
        print("configuration OK" if not cfg_errors else f"{len(cfg_errors)} configuration error(s); nothing applied")
        sys.exit(EX_NOHW if cfg_errors else 0)
    if cfg_errors and not args.stopped:        # the stop hook carries on with the built-in defaults
        log(error="invalid configuration: not starting (fix it, then: macpro-fand --check-config)")
        sys.exit(EX_NOHW)
    global T_WORK
    if float(os.environ.get("MACPRO_FAND_TEST_TWORK", "0") or 0):            # test hook: working temperature, C
        T_WORK = (float(os.environ["MACPRO_FAND_TEST_TWORK"]), T_WORK[1])
    if "macpro7,1" not in (rds("/sys/class/dmi/id/product_name") or "").lower() and not args.stopped:
        log(error="not a MacPro7,1: exiting")
        sys.exit(EX_NOHW)

    smc = find_smc()
    if args.stopped:  # ExecStopPost (systemd sets SERVICE_RESULT): fans first, then the power transaction
        return stop_hook(smc)
    if smc is None and not args.release:
        end = time.monotonic() + DISCOVER
        while smc is None and time.monotonic() < end:
            time.sleep(2)
            smc = find_smc()
    if smc is None:
        log(error="applesmc fan attributes not found (applesmc-t2 driver loaded?)")
        sys.exit(EX_NOHW)

    def fan(f, key):
        return f"{smc}/fan{f}_{key}"

    def full():
        fans_full(smc)

    def handback():
        """Clean stop: cap first, then t2fanrd (if installed) takes over with its own curve, unless this stop is
        half of a restart."""
        full()
        if unit_exists("t2fanrd.service") and not own_unit_restarting():
            systemctl("start", "--no-block", "t2fanrd.service")

    # One hardware owner (a second instance, by hand or as a service, refuses before touching anything).
    owner_fd = None
    if not args.dry_run:
        try:
            owner_fd = open_lock(OWNER_LOCK)
        except OSError as e:
            sys.exit(f"cannot create {OWNER_LOCK}: {e}")
        if not flock_within(owner_fd, 0):
            sys.exit("another macpro-fand owns the fans and power limits (stop it first)")
    if args.release:
        full()
        if write_static_safe() is False:
            log(error="--release: static-safe power limits not confirmed; fans left at their caps, t2fanrd not started")
            sys.exit(1)
        return handback()

    def make_die(a, b):
        try:
            return Die(a, b)
        except (RuntimeError, KeyError, OSError, IndexError):
            return None

    def discover():
        """Every die of die_map() whose hwmon answers within 10 s (amdgpu reads can block); the rest are missing."""
        found, missing = [], []
        for a, b in die_map().items():
            done, d = io_call(lambda: make_die(a, b), time.monotonic() + 10.0)
            (found if done and d else missing).append(d if done and d else a)
        return found, missing

    dies, missing_dies = discover()
    end = time.monotonic() + DISCOVER
    while missing_dies and time.monotonic() < end:
        notify(f"STATUS=waiting for amdgpu: {' '.join(missing_dies)}")
        time.sleep(2)
        dies, missing_dies = discover()
    if missing_dies:
        log(error=f"dies missing after {DISCOVER:.0f} s: {missing_dies}; charged {GPU_HW_MAX:.0f} W each, "
                  f"every fan at its cap")
    odd = [a for a in die_map() if not gpu_model_ok(a)]
    if odd:
        log(error=f"GPU dies that are not Vega 20: {odd}; the power budget's bounds and calibration are Vega 20's "
                  f"(unsupported)")
    core = hwmon_named("coretemp")
    pkg = temp_labels(core[0]).get("Package id 0") if core else None
    if pkg is None:
        sys.exit("coretemp 'Package id 0' not found")
    cpu = Sensor(pkg + "_input", 1000, 4.0)
    cpu_crit = sane(rd(pkg + "_crit", 1000), 92.0)
    smc_lab = temp_labels(smc)
    missing = [k for k in SMC if k not in smc_lab]
    if missing:
        sys.exit(f"SMC sensors not found: {missing}")
    smc_s = {k: (z, t, e, b, Sensor(smc_lab[k] + "_input", 1000, 10.0)) for k, (z, t, e, b) in SMC.items()}
    # internal flash (Apple vendor id) on the blower; production devices by slot (PERIPH), each with its own target
    if (not args.dry_run and not hwmon_named("drivetemp")
            and any(rds(b + "/queue/rotational") == "1" for b in glob.glob("/sys/block/sd*"))):
        subprocess.run(["modprobe", "drivetemp"], capture_output=True)
    for name, (tgt, band, crit0) in PERIPH.items():
        for h in hwmon_named(name):
            if not os.path.exists(h + "/temp1_input"):
                continue
            pci = (pci_chain(h + "/device") or ["?"])[-1]
            crit = rd(h + "/temp1_crit", 1000)
            crit = crit if crit is not None and 50.0 <= crit <= 130.0 else crit0
            if name == "nvme" and pci_vendor(pci) == "0x106b":
                smc_s["nvme"] = (NVME[0], NVME[1], crit - EMERG_MARGIN, NVME[2], Sensor(h + "/temp1_input", 1000, 10.0))
                continue
            slot = pci_slot(h + "/device")
            zone = DISK_ZONE if name == "drivetemp" else SLOT_ZONE.get(slot, MID)
            smc_s[f"{name}:{os.path.basename(os.path.realpath(h + '/device'))}"] = (
                zone, tgt, crit - EMERG_MARGIN, band, Sensor(h + "/temp1_input", 1000, 10.0))
    fan_in = {f: Sensor(fan(f, "input"), 1, 0.0) for f in CAP}

    log(fans={f: rds(fan(f, "label")) for f in CAP},
        crit={**{d.addr: [d.jcrit, d.mcrit] for d in dies}, "cpu": cpu_crit},
        zones={f: dict(z, cap=CAP[f]) for f, z in ZONES.items()}, bay_target=BAY_TARGET, band=BAND,
        smc=sorted(SMC), extra={k: v[:3] for k, v in smc_s.items() if k not in SMC}, dry_run=args.dry_run)

    # Exclusive ownership. Under systemd, Conflicts=/After= stop t2fanrd before we start; by hand, refuse.
    if not args.dry_run:
        end = time.monotonic() + (15 if under_systemd() else 0)
        while unit_active("t2fanrd.service") and time.monotonic() < end:
            time.sleep(0.5)
        if unit_active("t2fanrd.service"):
            sys.exit("t2fanrd is running: stop it first (the unit file has Conflicts=t2fanrd.service)")

    def stop(*_):
        raise SystemExit(0)

    # From the first hardware write on, every exit path below runs the cleanup in `finally`.
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    gov = {f: Governor(f) for f in CAP}
    cmd = {f: gov[f].rpm for f in CAP}
    written = {f: None for f in CAP}
    budget, worker, smc_dc = None, None, None
    try:
        if budget_ready():
            budget = Budget({d.addr: os.path.dirname(d.paths[0]) + "/power1_cap" for d in dies}, len(missing_dies))
            enforcing, pl2_present = rapl_state()
            budget.pl2_held = pl2_present and os.path.exists(RAPL_PL2)
            budget.cpu_charge(rd(RAPL_PL2), enforcing)
            fits = budget.fit()
            smc_dc = SmcKey(smc, "PZ0G")
            if not args.dry_run:
                worker = PowerWorker()
                worker.start()
                worker.set(budget.want(), () if budget.pl2_held else (RAPL_PL2,), budget.cost() <= budget.limit + 1e-6)
            log(budget={"limit": budget.limit, "backstop": budget.backstop,
                        "gpu": [round(v) for v in budget.gpu.values()], "pl1": round(budget.pl1),
                        "cost": round(budget.cost()), "fits": fits, "dc_key": smc_dc.idx is not None,
                        "base": base(), "devices": devices(), "unmanaged_dies": missing_dies,
                        "rapl_enforcing": enforcing, "pl2_held": budget.pl2_held,
                        "static_safe": static_safe(len(dies), len(missing_dies))})
        else:
            log(budget="disabled: BASE/K_GPU/K_CPU not calibrated")
        rapl_e, pkg_w = [None, None], None
        rapl_max = rd(RAPL_ZONE + "/max_energy_range_uj")
        last_budget, dc_missing_since, was_feasible = 0.0, None, True
        last_seq = {d.addr: None for d in dies}
        last_t = {d.addr: None for d in dies}     # when each die's last fed reading was taken
        ref = {f: (None, 0.0) for f in CAP}       # (setting the fan is converging on, since when)
        bad_since = {f: None for f in CAP}
        good_since = {f: None for f in CAP}
        failed = {f: False for f in CAP}          # latched until the fan follows its setting again for FAN_FAIL
        owner_fails = 0
        gpu = GpuReader(dies)
        gpu.start()

        def put(f, rpm):
            if args.dry_run:
                return
            wr(fan(f, "manual"), 1)
            wr(fan(f, "output"), rpm)

        def take_over():
            """Write the floors until every fan reads back manual at its setting (t2fanrd's exit can reset a fan to
            automatic after our first write)."""
            end = time.monotonic() + HANDOVER
            while time.monotonic() < end:
                for f in CAP:
                    put(f, int(cmd[f]))
                time.sleep(1.0)
                if all(rds(fan(f, "manual")) == "1" and abs((rd(fan(f, "output")) or 0) - int(cmd[f])) <= 5
                       for f in CAP):
                    for f in CAP:
                        written[f] = int(cmd[f])
                    return True
            return False

        if not args.dry_run and not take_over():
            full()
            sys.exit("fans do not hold manual settings after the handover: left at the cap")
        notify("READY=1")
        last = time.monotonic()
        last_log = -LOG_EVERY
        last_note = ""
        while True:
            time.sleep(PERIOD)
            now = time.monotonic()
            dt, last = min(now - last, 5 * PERIOD), now
            alarms = []
            ez = {f: [] for f in CAP}              # per zone: (reading - target) of every sensor it answers to
            mode, hold, note, pin = parse_override()

            # GPU dies: each die's own reading and age; temperatures checked on their own, power separately
            snap = gpu.latest()
            alarms += [f"die {a} missing" for a in missing_dies]
            tj, tm = {1: [], 2: []}, {1: [], 2: []}
            gpu_ok = bool(dies)
            ages = []
            for d in dies:                         # (Vega 20 here has no runtime PM: the dies never suspend)
                ent = snap.get(d.addr)
                if ent is None or ent[1] > GPU_STALE:
                    alarms.append(f"gpu stale {d.addr}" if ent else f"gpu no data {d.addr}")
                    gpu_ok = False
                    continue
                vals, age, seq = ent
                ages.append(age)
                if seq != last_seq[d.addr]:        # a new reading: feed it once, over the time since the last one
                    taken = now - age
                    gap = PERIOD if last_t[d.addr] is None else min(GPU_STALE, max(0.0, taken - last_t[d.addr]))
                    last_seq[d.addr], last_t[d.addr] = seq, taken
                    for flt, v in zip((d.tj, d.tm, d.p), vals):
                        flt.feed(v, gap)
                if any(flt.fails >= READ_FAILS for flt in (d.tj, d.tm, d.p)):   # every loop, until a good reading
                    alarms.append("read " + d.addr)
                if (d.tj.raw is not None and d.tj.raw >= d.jcrit - EMERG_MARGIN) or \
                        (d.tm.raw is not None and d.tm.raw >= d.mcrit - EMERG_MARGIN):
                    alarms.append("hot " + d.addr)
                if d.tj.val is not None:
                    tj[d.bay].append(d.tj.val)
                if d.tm.val is not None:
                    tm[d.bay].append(d.tm.val)
            age = max(ages) if ages else None
            bay_t = {b: (sum(v) / len(v) if v else None) for b, v in tj.items()}
            if budget and gpu_ok:
                budget.thermal({d.addr: (d.tj.val, d.tm.val, d.p.val, d.bay) for d in dies},
                               {b: gov[f].rpm >= CAP[f] - WRITE_STEP for b, f in BAY_FAN.items()}, dt)
            for b, f in BAY_FAN.items():
                if bay_t[b] is not None:
                    ez[f].append((bay_t[b] - BAY_TARGET, BAND))
                if tm[b]:
                    ez[f].append((max(tm[b]) - HBM_TARGET, BAND))

            # CPU package -> top front fan, at a target that follows package power
            e = rd(RAPL_E)
            cpu_w = energy_rate(rapl_e, now, e, rapl_max)
            if cpu_w is not None:
                pkg_w = ema(pkg_w, cpu_w, dt, CPU_TARGET_TAU)
            rapl_e[:] = [now, e]
            if not cpu.update(dt):
                alarms.append("read cpu")
            if cpu.raw is not None:
                if cpu.raw >= cpu_crit - 3.0:
                    alarms.append("hot cpu")
                ez[TOP].append((cpu.val - cpu_target(pkg_w), CPU_BAND))

            # SMC (and flash) sensors
            for k, (zone, tgt, emerg, band, s) in smc_s.items():
                if not s.update(dt):
                    alarms.append("read " + k)
                if s.raw is None:
                    continue
                if s.raw >= emerg:
                    alarms.append("hot " + k)
                ez[zone].append((s.val - tgt, band))

            # power budget: backstop every loop, reallocation every BUDGET_PERIOD (never on stale GPU data); the
            # power worker applies and confirms, the loop only plans
            if budget:
                dc = smc_dc.read()
                dc_missing_since = None if dc is not None else (dc_missing_since or now)
                degraded = dc_missing_since is not None and now - dc_missing_since > DC_DEGRADED
                if degraded != budget.degraded:
                    budget.degraded = degraded
                    log(budget_note="no measured DC: demand-driven reallocation paused (pins and the CPU's charge "
                                    "still apply, inside the invariant)" if degraded
                        else "measured DC back: reallocation resumes")
                if budget.backstop_check(dc, now):
                    alarms.append(f"power backstop: DC {dc:.0f} W > {budget.backstop:.0f} W, every limit to its floor")
                if budget.pin is not None and pin is None:
                    budget.end_pin(now)
                    log(budget_note="override pin ended: default allocation " +
                        ("after the backstop hold" if budget.reset_pending else "restored"))
                budget.pin = pin
                st = worker.status() if worker else None
                if st:
                    for n in st["notes"]:
                        log(budget_note=n)
                    if st["pending"] and now - st["pending"][2] > POWER_STUCK:
                        alarms.append(f"power limit {st['pending'][1]} stuck: {st['pending'][0]}")
                    if st["fail_since"] and now - st["fail_since"] > POWER_FAILING:
                        alarms.append(f"power limits not confirmed: {st['failed']}")
                    if budget.pl2_held and st["pl2_bad"] >= 3:
                        budget.pl2_held = False
                        log(budget_note="PL2 cannot be held at PL1: the CPU is charged at PL2 as read back")
                if now - last_budget >= BUDGET_PERIOD:
                    last_budget = now
                    pl2_now = pl2_observed(st, now) if worker else rd(RAPL_PL2)     # (dry run: no worker)
                    if budget.cpu_charge(pl2_now, rapl_state()[0]):
                        log(budget_note=f"CPU charged at {budget.cpu_w():.0f} W (PL1 {budget.pl1:.0f} W)" +
                            (", RAPL not enforcing or PL2 not held" if budget.cpu_min else ""))
                    if gpu_ok:
                        msg = budget.rebalance(now, {d.addr: d.p.raw for d in dies}, cpu_w)
                        if msg:
                            log(budget_note=msg, dc=r1(dc), cpu_w=r1(cpu_w))
                feasible = budget.cost() <= budget.limit + 1e-6     # now, with the CPU's present charge -- not the
                if not feasible:                                    # start's: an infeasible allocation alarms (fans
                    alarms.append(f"budget infeasible: {budget.cost():.0f} W > {budget.limit:.0f} W even at the "
                                  f"floors (CPU charged {budget.cpu_w():.0f} W)")   # to their caps) for as long as it lasts
                if feasible != was_feasible:
                    log(budget_note="budget infeasible: only lowering limits, every fan at its cap" if not feasible
                        else "budget feasible again")
                    was_feasible = feasible
                if worker:
                    worker.set(budget.want(), () if budget.pl2_held else (RAPL_PL2,), feasible)

            # fans: following their settings, and nobody else writing them
            for f in CAP:
                if not fan_in[f].update(dt):
                    alarms.append(f"read fan{f}")
                tach = 0.0 if f == TEST_DEAD_FAN else fan_in[f].raw
                if written[f] is None or args.dry_run:
                    continue
                if ref[f][0] is None or abs(written[f] - ref[f][0]) > 100:
                    ref[f] = (written[f], now)
                off = tach is None or abs(tach - written[f]) > max(FAN_TOL[0], FAN_TOL[1] * written[f])
                stopped = tach is None or tach < 0.5 * MIN_RPM[f]   # a stopped fan counts at once, ramp or not
                if off and (failed[f] or stopped or now - ref[f][1] > FAN_SETTLE):
                    good_since[f] = None
                    bad_since[f] = bad_since[f] or now
                    if now - bad_since[f] > FAN_FAIL:
                        failed[f] = True
                elif not off:
                    bad_since[f] = None
                    good_since[f] = good_since[f] or now
                    if failed[f] and now - good_since[f] > FAN_FAIL:
                        failed[f] = False
                if failed[f]:
                    alarms.append(f"fan{f} at {tach} rpm, set {written[f]}")
            if not args.dry_run:
                changed = [f for f in CAP if written[f] is not None and (
                    rds(fan(f, "manual")) != "1" or abs((rd(fan(f, "output")) or 0) - written[f]) > 5)]
                owner_fails = owner_fails + 1 if changed else 0
                if changed:
                    for f in changed:                # re-assert; persistent changes mean a second writer
                        try:
                            put(f, written[f])
                        except OSError as e:
                            alarms.append(f"write fan{f}: {e}")
                    if owner_fails >= OWNER_FAILS:
                        alarms.append(f"fans {changed} changed by another writer")

            # governors, override, emergency. Under an alarm or "max" every fan goes to its cap at once and the
            # governors restart from there, so they ramp down slowly afterwards; a hold sets each fan exactly and
            # the governors continue from the held rpm.
            for f in CAP:
                gov[f].step(ez[f], dt)
                if alarms or mode == "max":
                    gov[f].rpm = float(CAP[f])
                elif mode == "hold":
                    gov[f].rpm = hold[f]
                cmd[f] = gov[f].rpm

            # quantise, write
            for f in CAP:
                rpm = int(round(cmd[f]))
                if written[f] is None or abs(rpm - written[f]) >= WRITE_STEP or (
                        gov[f].rate == 0.0 and rpm != written[f]):
                    try:
                        put(f, rpm)
                        written[f] = rpm
                    except OSError as e:
                        alarms.append(f"write fan{f}: {e}")

            notify("WATCHDOG=1")
            if alarms or note != last_note or now - last_log >= LOG_EVERY:
                last_log, last_note = now, note
                log(alarms=alarms, mode=mode, note=note or None,
                    dies={d.addr: [r1(d.tj.val), r1(d.tm.val), r1(d.p.val)] for d in dies},
                    gpu_age=r1(age), cpu=[r1(cpu.val), r1(cpu_target(pkg_w)), r1(pkg_w)],
                    bays={b: r1(t) for b, t in bay_t.items()},
                    smc={k: r1(s[-1].val) for k, s in smc_s.items()},
                    zones={f: [r1(gov[f].err), round(gov[f].rate, 2), int(gov[f].rpm)] for f in CAP}, rpm=written,
                    budget=(None if not budget else {"gpu": [round(v) for v in budget.gpu.values()],
                            "pl1": round(budget.pl1), "cost": round(budget.cost()), "dc": r1(budget.dc),
                            "tcap": [round(v) for v in budget.tcap.values()], "cpu_w": round(budget.cpu_w()),
                            "degraded": budget.degraded, "feasible": budget.cost() <= budget.limit + 1e-6,
                            "confirmed": (st["ok"] if st else None)}),
                    tach={f: fan_in[f].raw for f in CAP})
    finally:
        # Fans to their caps at once; then the power worker starts nothing more (under systemd ExecStopPost then
        # recovers). Run by hand: only once the worker is idle for good, so no older write can land after it, the
        # static-safe transaction, then t2fanrd. A worker stuck in the driver means no competing transaction: fans stay at their
        # caps and the exit status says the recovery is incomplete.
        if not args.dry_run:
            full()                                # cooling first: the control loop has stopped
        idle = worker.stop(2.0 if under_systemd() else STOP_DEADLINE) if worker else True
        if not args.dry_run and not under_systemd():
            if not idle:
                log(error="power recovery incomplete: a power-limit operation is stuck in the driver; fans left at "
                          "their caps, t2fanrd not started")
                raise SystemExit(1)
            if write_static_safe() is False:
                log(error="power recovery incomplete: static-safe limits not confirmed; fans left at their caps, "
                          "t2fanrd not started")
                raise SystemExit(1)
            handback()


if __name__ == "__main__":
    main()
