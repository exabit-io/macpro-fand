macpro-fand.conf(5)

# NAME

macpro-fand.conf - settings for macpro-fand

# SYNOPSIS

_/etc/macpro-fand.conf_

# DESCRIPTION

*macpro-fand*(8) reads this file once at start. Every setting is optional:
the built-in defaults were measured on a Mac Pro 7,1 with a Xeon W-3275M and
two Radeon Pro Vega II Duo modules, and the installed file lists them all,
commented out. Check the file and restart the service after a change:

	macpro-fand --check-config && systemctl restart macpro-fand

# SYNTAX

One setting per line, *NAME = value*, where the value is a Python literal: a
number, a tuple *(a, b)* or a dict *{key: value}*. A *#* starts a comment. An
indented line continues the setting above it. A dict setting updates the
default entry by entry, so *ZONES = {4: {"floor": 700}}* changes one floor
and leaves the rest.

The file is checked as a whole before any of it is used. A setting that does
not parse or is not known, a value of the wrong type or shape or outside its
range (numbers must be finite; booleans are not numbers), and floors that
together do not fit the power budget are each reported, and *macpro-fand*
refuses to start (exit status 78) with nothing applied. The stop hook alone
carries on, with the built-in defaults.

Fans are numbered as the applesmc driver numbers them: *1* the blower (memory
side, 500-1200 rpm), *2* the bottom front fan (MPX bay 1 and the PCH), *3*
the middle front fan (MPX bay 2 and PCIe slots 5-8), *4* the top front fan
(CPU); the front fans run from 500 to 2500 rpm.

# ZONE TARGETS

*BAY_TARGET* = _37.8_
	Mean GPU junction temperature of each MPX bay, °C, held at any fan
	speed (100 °F).

*HBM_TARGET* = _80.0_
	Hottest HBM stack of each bay, °C.

*CPU_TARGET* = _((133.0, 48.0), (228.0, 57.5))_
	CPU package target, °C, against package power, W: two points, linear
	between them and flat outside. Package power is smoothed over
	*CPU_TARGET_TAU* = _30.0_ s.

*CPU_BAND* = _1.0_, *BAND* = _0.5_
	Hysteresis half-width, °C, of the CPU target and of the other targets.

*ZONES*
	Per fan: *floor*, the lowest speed its governor may choose, and *init*,
	its speed at start, rpm. Defaults: blower 600/600, bottom 500/1150,
	middle 500/1150, top 500/500.

*SMC*
	SMC temperature sensors by key: *(fan, target, emergency, band)*.
	Defaults: PCH (*TPCD*) on fan 2 at 45.7 °C, emergency 90 °C; DIMM
	proximity (*TM0P*-*TM8P*) on the blower at 50 °C, emergency 85 °C; power
	supply (*Tp0P*-*Tp2P*) on the blower at 55 °C, emergency 80 °C.

*NVME* = _(1, 55.0, 1.0)_
	The internal flash: fan, target, band; its emergency is its critical
	temperature less 5 °C.

*PERIPH*
	Devices beyond the stock machine, by hwmon driver name: *(target,
	band, critical)*, used when the device reports no critical temperature.
	Defaults: *nvme* (60, 1, 80), *mlx5* (80, 1, 105), *drivetemp* (40, 1, 60).

*SLOT_ZONE* = _{"5": 3, "6": 3, "7": 3, "8": 3}_
	The fan for a device in each PCI slot.

*DISK_ZONE* = _3_
	The fan for SATA disks (*drivetemp*; loaded automatically when a
	rotational disk is present).

# POWER BUDGET

*BUDGET_LIMIT* = _1228.0_, *BUDGET_MARGIN* = _80.0_
	The DC envelope, W, and the part of it kept free for model error.

*BASE* = _127.0_, *K_GPU* = _1.205_, *K_CPU* = _1.156_
	The calibration: worst-case DC = BASE + K_GPU × (sum of die caps) +
	K_CPU × PL1. BASE is the machine with every fan at full speed plus an
	allowance for memory and storage traffic.

*DEVICE_W* = _{"mellanox": 25.0, "nvme": 9.0, "disk": 25.0}_
	Worst case added to BASE for each Mellanox card, NVMe drive (other than
	the internal flash) and rotational disk found at start.

*GPU_CAP* = _{"floor": 85, "max": 200, "init": 125}_
	Range of a GPU die's cap and its default share, W; the built-in
	maximum is Vega 20's 200 W. The installed file sets
	*GPU_CAP = {"max": 125}*; comment that line out for the full range.
	Whole watts. Required: 50 ≤ *floor* ≤ *max* ≤ 200 and *floor* ≤ *init*. A maximum
	below *init* or the *STATIC_SAFE* GPU cap lowers those to it.

*CPU_PL1* = _{"floor": 80, "max": 413}_
	Range of the CPU's PL1, whole watts; the floor is at least 10 W.

*CPU_KEEP* = _(1.2, 20.0)_
	A CPU below saturation keeps *a* × its draw + *b* W of PL1; the dies may
	borrow only the rest.

*STATIC_SAFE* = _{"gpu": 125, "pl1": None}_
	The limits written when the daemon stops or fails; *None* is the
	largest PL1 the budget allows beside the GPU caps. A die without a power
	cap is charged at 200 W, and the GPU caps come down if the CPU's floor
	would not fit beside them (or what the CPU can draw, while RAPL does not
	enforce or PL2 will not follow PL1). Whole watts; the GPU value may not
	be below *GPU_CAP*'s floor, nor *pl1* below *CPU_PL1*'s.

*BACKSTOP* = _1190.0_
	Measured DC, W, that drops every limit to its floor for 60 s.

# GPU WORKING TEMPERATURE

*T_WORK* = _(65.0, 1.0)_
	Working junction temperature and band, °C. Over it, with the bay fan at
	full speed, a die's cap comes down until the die holds there.

*HBM_WORK* = _80.0_
	The same for the hottest HBM stack.

# HARDWARE MAP

*DIES* = _{}_
	GPU dies as *{"0b:00.0": bay, ...}*. Empty: every AMD display device,
	its MPX bay from its PCI slot through *MPX_BAY*.

*MPX_BAY* = _{"1": 1, "2": 1, "3": 2, "4": 2}_
	PCI slot to MPX bay; a die in another slot counts as bay 2.

# EXAMPLE

	# a warmer working temperature, and a slower CPU fan at idle
	T_WORK = (68.0, 1.0)
	ZONES = {4: {"floor": 600}}

# SEE ALSO

*macpro-fand*(8)

# AUTHORS

Exabit, Inc. <packages@exabit.io>
