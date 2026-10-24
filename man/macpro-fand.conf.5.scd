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
together do not fit the power budget are rejected; the error is reported, and *macpro-fand*
refuses to start (exit status 78) with nothing applied. The stop hook alone
carries on, with the built-in defaults.

Version 2 implements this syntax with a bounded, non-executing Rust parser;
Python is not needed at runtime. Supported literals are finite numbers,
single- or double-quoted strings with ordinary character escapes, tuples,
lists, dicts, *True*, *False* and *None*. Expressions, comprehensions, bytes,
raw strings and triple-quoted strings are not supported. The file must be
a regular UTF-8 file no larger than 128 KiB, with at most 32 levels of
literal nesting, 16384 literal nodes per setting, and 8 KiB per string.

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
	Devices beyond the stock machine, by hwmon driver name (or *TEMP_COMMAND*
	class): *(target, band, critical)*; the critical temperature is used when
	the device reports none, and the emergency is 5 °C below it. Defaults:
	*nvme* (60, 1, 80), *mlx5* (80, 1, 105), *drivetemp* (40, 1, 60),
	*mellanox* (80, 1, 105). Only devices in a *SLOT_ZONE* slot or on an
	internal SATA port steer a fan; USB and Thunderbolt devices are not
	cooled by the box's fans and are left out.

*PERIPH_POLL*
	Read period and staleness limit, s, per sensor class: *(period, stale)*,
	stale at least two periods. Defaults: *nvme* (5, 30), *mlx5* (5, 30),
	*drivetemp* (30, 120), *mellanox* (15, 60); any other class (5, 30). Each
	sensor has its own reader thread, so a drive that holds a temperature
	command (spin-up, error recovery) only makes its own reading stale. A
	reading older than its limit counts as a failed read (alarm: every fan
	to its cap). A sensor counts only from its first reading. One that
	stops reading is retired, with a note, when its driver has been gone
	for 20 s (before its readings go stale: a driver reload raises no
	alarm) or it has read nothing for 600 s; its zone then keeps the
	device's *UNMONITORED* floor. A retired sensor that reads again comes
	back. Sensors are rediscovered once a minute (driver reloads renumber
	hwmon).

*TEMP_COMMAND* = _{}_
	A program that prints a device's temperature, °C, for slot cards whose
	driver gives the kernel no sensor, by *DEVICE_W* class: a tuple of
	strings, the program an absolute path, *{pci}* replaced by the card's
	function-0 PCI address. It runs on the sensor's own thread every
	*PERIPH_POLL* period, killed at that period (at most 10 s); the first
	number on its standard output is the reading. A program that does not
	exist at start adds nothing. For Mellanox cards with MFT installed:
	*TEMP_COMMAND = {"mellanox": ("/usr/bin/mget_temp", "-d", "{pci}")}*.

*UNMONITORED* = _{"mellanox": (1000, 4), "pcie-card": (1000, 4)}_
	A floor for the fan of a device's zone while the device has no live
	temperature sensor, by *DEVICE_W* class (a card with no hwmon and no
	*TEMP_COMMAND*, or a retired sensor): an rpm, or *(rpm, fan)*, the zone
	fan then also never slower than that fan. Automatic mode only. The
	default comes from ConnectX-4 cards in slots 5 and 6 at idle
	(2026-10-23, one fan raised 600 rpm at a time to thermal equilibrium):
	42.5 °C at 1150 rpm of the middle fan uncabled, 53.4 °C with both
	InfiniBand links up and no traffic, 5.3 °C (uncabled) to 8 °C cooler
	at 1750, following 1/airflow, which at the middle fan's 500 rpm floor
	predicts about 68 °C idle uncabled and over 90 °C under traffic. No
	baffle separates the top fan's air from the slots: 600 rpm more on the
	top fan alone left the cabled cards wandering 4 to 9 °C hotter for 100
	minutes without settling, and with the middle fan raised as well they
	settled 7 °C cooler. So the middle fan follows the top fan.

*SLOT_ZONE* = _{"5": 3, "6": 3, "7": 3}_
	The fan for a device in each PCI slot. Only these slots hold
	peripherals: devices elsewhere are neither charged to the budget nor
	governed (slot 8 is the Apple I/O card, slots 1-4 the MPX bays).

*DISK_ZONE* = _4_
	The fan for internal SATA disks (*drivetemp*; loaded automatically when
	a SATA disk is present).

# POWER BUDGET

*BUDGET_LIMIT* = _1228.0_, *BUDGET_MARGIN* = _80.0_
	The DC envelope, W, and the part of it kept free for model error.

*BASE* = _127.0_, *K_GPU* = _1.205_, *K_CPU* = _1.156_
	The calibration: worst-case DC = BASE + K_GPU × (sum of die caps) +
	K_CPU × PL1. BASE is the machine with every fan at full speed plus an
	allowance for memory and storage traffic.

*DEVICE_W*
	Worst case added to BASE for each peripheral found at start, W, by class:
	*mellanox* 25 (a Mellanox card, both ports), *nvme* 25 (an NVMe drive
	directly in a slot), *nvme-switched* 9 (an NVMe drive behind a switch on a
	carrier card, M.2), *pcie-switch* 10 (a switch on a card), *pcie-card* 25
	(any other card in a *SLOT_ZONE* slot), *disk* 25 (a spinning disk,
	spin-up included), *ssd* 8 (a SATA/SAS SSD), *usb-disk* 10 (a USB disk
	that reports no rotation rate). A disk is a spinning disk or an SSD by the
	rotation rate it reports itself (SCSI VPD page B1); the kernel's
	*rotational* flag is used only when it reports none, and never for USB
	disks, which the kernel marks rotational whatever they are.
	*macpro-fand --inventory* lists what is found and charged.

*DEVICE_PCI_W* = _{}_
	An allowance, W, by PCI id *"vvvv:dddd"* (lower-case hex), in place of the
	class's *DEVICE_W* for every card with that id.

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
