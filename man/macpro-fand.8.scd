macpro-fand(8)

# NAME

macpro-fand - zone fan control and power budget for the Mac Pro 7,1

# SYNOPSIS

*macpro-fand* [*--config* _file_]++
*macpro-fand* *--dry-run* [*--config* _file_]++
*macpro-fand* *--release*++
*macpro-fand* *--stopped*++
*macpro-fand* *--check-config* [*--config* _file_]++
*macpro-fand* *--version* | *--help*

# DESCRIPTION

*macpro-fand* runs the four fans of a Mac Pro 7,1 (2019) under Linux through
the T2 applesmc driver, and keeps the whole machine under the 1228 W DC
envelope of its power supply by managing the power limits of the GPU dies
and the CPU. It replaces *t2fanrd*, which follows the CPU temperature only:
on a Mac Pro with MPX GPU modules, t2fanrd holds every fan at its 500 rpm
minimum while the GPU dies soak at 58 °C and above, and the leakage current of
the hot silicon costs more power than the fans would.

It normally runs as *macpro-fand.service*. Exactly one process may own the
fans: starting the service stops t2fanrd, and a clean stop hands the fans
back to t2fanrd.

## Zones

The case is divided into four airflow zones, each cooled mainly by one fan,
and each fan is driven by its own governor:

[[ *Fan*
:< *Zone*
:< *Sensors and default targets*
|  2, bottom front
:  MPX bay 1, PCH
:  mean GPU junction of the bay 37.8 °C (100 °F); hottest HBM stack 80 °C; PCH 45.7 °C
|  3, middle front
:  MPX bay 2, PCIe slots 5-8
:  mean GPU junction of the bay 37.8 °C; hottest HBM stack 80 °C; NVMe drives and Mellanox NICs in slots 5-8 (health targets)
|  4, top front
:  CPU
:  package temperature, 48 °C at 133 W package power rising to 57.5 °C at 228 W
|  1, blower
:  memory side
:  DIMM proximity 50 °C, power supply 55 °C, internal flash 55 °C

The targets come from measurements of the whole machine's DC power against
each fan's speed, one fan at a time, every point held until the machine was at
thermal equilibrium. Below the optimum the extra airflow costs more fan power
than it saves in silicon leakage; above it, leakage costs more than the fans
would. The MPX-bay target is the bay temperature at the idle optimum. The CPU's
optimum temperature rises with load (44-52 °C at 133 W, 56-59 °C at 228 W), so
its target follows package power. The memory side showed no leakage payoff and
holds component-health targets instead.

Each governor works with hysteresis. When any sensor of its zone is more than
its band (0.5 °C; 1 °C for sensors that report whole degrees) over its target,
the fan speeds up in proportion to the excess, 1 rpm/s per °C, up to
150 rpm/s. Inside the band the fan holds its speed. When every sensor is under
its band, the fan slows down at 0.5 rpm/s per °C, between 0.25 and 2 rpm/s, so
it never hunts. The MPX bays are held at their target at whatever speed that
takes: under GPU load the bay fans normally run at full speed.

## Alarms

Any alarm sends every fan to its full speed at once; the governors then come
back down slowly from there. Alarms are: a sensor at its emergency threshold
(GPU junction, HBM and NVMe within 5 °C of their critical limit, the CPU
package within 3 °C, SMC sensors at fixed thresholds: DIMMs 85 °C, power supply
80 °C, PCH 90 °C), a sensor that cannot be read three times in a row, a GPU
die whose latest reading started more than 10 s ago, a GPU die that is
missing, a fan that does not follow its setting, another process writing the
fans, a power limit that cannot be written or confirmed (see Writing the
limits), and a budget that does not fit even at every floor -- judged on every
loop with the CPU's present charge, so for instance RAPL ceasing to enforce
mid-run raises it; it clears once the allocation fits again. A GPU die's temperatures raise their alarm whether or not its power
can be read. The fans are never put in the SMC's automatic
mode, which has been seen holding every fan at its minimum regardless of GPU
heat.

## Power budget

Crossing the 1228 W DC envelope makes the SMC latch the GPU dies at 1000 MHz
until a cold power cycle. Temperatures cannot prevent that: every GPU die and
the CPU can spike to full power at the same moment while every zone is still
cool. *macpro-fand* therefore keeps an invariant over the power limits it
sets, independent of the fans: the worst case of every limit together never
exceeds the budget,

	BASE + K_GPU × (sum of die caps) + K_CPU × PL1 ≤ BUDGET_LIMIT − BUDGET_MARGIN

where the GPU die caps are the amdgpu *power1_cap* values, PL1 is the CPU's
long-term RAPL limit, and BASE, K_GPU and K_CPU come from a calibration of
the machine's measured DC power (1228 − 80 = 1148 W by default). PL2, the
CPU's short-term limit, is held at PL1, so the CPU cannot burst past what the
budget charges it for. If PL2 cannot be held there (three failed writes in a
row), the CPU is charged at PL2 as read back, and while the RAPL package zone is
disabled or not enforcing, at 413 W, the top of PL1's range; the GPU dies make
room. A GPU die in the hardware map whose power cap is not found within 240 s
of start is charged at Vega 20's 200 W maximum, and every fan runs at full
speed. Every uncapped device found at start that the calibration did not
include (Mellanox network cards, NVMe drives other than the internal flash,
rotational disks) adds its worst case to BASE.

The invariant is a model of sustained draw at those limits. K_GPU and K_CPU
were calibrated at thermal equilibrium, and the GPU firmware and RAPL enforce
their limits as averages, so a die or the CPU can briefly draw more than its
limit. *BUDGET_MARGIN* (80 W) covers model error and those transients; the
measured-DC backstop below covers whatever exceeds both.

Within the budget, power moves by demand, CPU first. A GPU die drawing at
least 93 % of its cap is saturated, and so is the CPU at 85 % of PL1 (with PL2
held at PL1, RAPL keeps the package near 90 % of it). A saturated CPU gets at least its default
share at once, taken from free budget first, then from idle dies, then evenly
from every die. Saturated dies get free budget, the budget of idle sibling
dies, and only the part of the CPU's limit that the CPU does not use (it keeps
1.2 × its draw + 20 W). Whatever the dies do not hold goes back to the CPU. A die cap never goes below 85 W (Vega 20 ignores lower caps)
and PL1 never below 80 W. A die cap never goes above the configured maximum
(*GPU_CAP*: 125 W in the installed configuration file, Vega 20's 200 W
without that line; see *macpro-fand.conf*(5)).

As a backstop, a measured DC power over 1190 W on two samples in a row drops
every limit to its floor for 60 s; an override pin that ends during that time
waits for it. Without a measured DC reading for 15 s, power stops moving
between the CPU and the dies by demand until the reading returns; override
pins, the end of a pin and changes in what the CPU is charged still apply,
always inside the invariant. When the daemon stops or fails,
the limits go to a statically safe allocation: 125 W per die (or the
configured maximum if lower), dies without a power cap charged at 200 W, and
the largest PL1 the invariant allows.

## Writing the limits

One thread does every power-limit read and write, so a driver call that
blocks stalls only that thread, never the fans. Each change of the limits is
one transaction in three phases. First, limits known to be above their
target come down (largest first). Then every limit whose present value could
not be read is set to 10 W, the lowest value ever written, whatever the floors
are or were configured to (PL2 no lower than PL1 is, which cannot raise what
the CPU is charged), so a limit of unknown value can only give headroom, never
take it. (The one exception: a limit somebody else set below 10 W rises to 10 W
in that step -- at most 59.76 W of model for four dies and the CPU together
with the default calibration, inside the default 80 W margin. A configuration
with a smaller margin, other coefficients or more dies needs its own check.)
Only then, with every limit confirmed at or below its target, do
limits go up -- and only if the allocation fits the budget: one that does not
(even every floor is too much) only lowers limits. Every write is read back and counts only at its target or up to 1 W below
it, never above. The first write that fails, does not finish or does not read
back stops the transaction, so nothing is raised on budget that was not
released and no step costs more than the larger of the state before and the
target. Limits are written in whole watts, rounded down, and never below
10 W (amdgpu takes a zero cap as "restore the default").

Every 2 s the limits are read back and written again wherever they differ: a
failed write is retried, and another process's write is logged and undone. A
PL2 that is not held at PL1 is still read every cycle; a reading taken more
than 10 s ago counts as 413 W. A power-limit operation stuck for 15 s, or limits not
confirmed for 30 s, is an alarm.

When the daemon stops, every fan goes to full speed at once; then the worker
starts no further operation and the one in progress (if any) is waited for,
so nothing it had started can land after the statically safe limits. Only
then are those written, by the same rule, every read and write bounded, none
started after 10 s. If an operation is still stuck in the driver, no competing
transaction is started: the fans stay at full speed and the stop reports the
recovery as incomplete. If the statically safe allocation does not fit (for
example GPU dies whose power cap has disappeared are charged 200 W each), the
stop only lowers limits, leaves the fans at full speed and reports it.

## Full auto: GPU working temperature

Within the budget, each die's cap also follows its temperature. Once a die's
junction is more than 1 °C over the working temperature *T_WORK* (or its
hottest HBM stack over *HBM_WORK*) and its bay fan is already at full speed
(or the excess passes 5 °C), the die's ceiling comes down from its present draw, by 1 W/s per °C of excess
(at most 10 W/s), until the die holds there. When the die is cooler, its
ceiling rises slowly, 0.1 W/s per °C (at most 1 W/s), and never more than 10 W
above the die's share of the budget. A hotter die simply runs at a lower cap.
Override pins (below) suspend the ceilings.

# OPTIONS

*--config* _file_
	Read settings from _file_ instead of _/etc/macpro-fand.conf_.

*--dry-run*
	Read every sensor, run the governors and the budget, and log what would
	be done, without writing a fan or a power limit and without loading a
	kernel module. Can run beside the service.

*--release*
	Set every fan to its full speed, write the statically safe power limits
	and start t2fanrd if it is installed. For use by hand after stopping the
	service; refused while another *macpro-fand* owns the hardware. If the
	power limits are not confirmed, the fans stay at full speed, t2fanrd is
	not started and the exit status is 1.

*--stopped*
	The service's ExecStopPost hook. After a clean stop: every fan to its
	full speed, then t2fanrd (unless the service itself is restarting).
	After a failure (crash, watchdog, kill): every fan to its full speed.
	In both cases the statically safe power limits are written. It takes
	the owner lock first: if another *macpro-fand* holds it (this run was
	refused because that one owns the hardware), it changes nothing. With
	an invalid or unreadable configuration file it uses the built-in
	defaults; t2fanrd is started only if the power limits were confirmed.

*--check-config*
	Check the configuration file (all of it; see *macpro-fand.conf*(5)),
	print the result and exit: 0 if it is valid, 78 if not.

*--version*
	Print the version and exit.

# OVERRIDE

Benchmarks and experiments can change the control for a while through
_/run/macpro-fand/override_, one line, re-read every two seconds:

*max*
	Every fan at full speed.

*hold* _F1_ _F2_ _F3_ _F4_
	Hold the fans at exactly these speeds, in rpm (blower, bottom, middle,
	top). An alarm still sends every fan to full speed.

*auto*
	Normal control, the same as no file.

Any of these may be followed by:

*gpu=*_W_
	Pin every GPU die cap at _W_ watts, from 85 up to the configured
	maximum (*GPU_CAP*: 125 W as installed, 200 W without that line).

*pl1=*_W_
	Pin the CPU's PL1 at _W_ watts (experiments may go below the floor).

*ttl=*_seconds_
	Ignore the file once it is older than this, so a forgotten override
	expires on its own. Rewrite the file to extend it.

Pins stay inside the power budget: a pin that does not fit is lowered. When
the file is removed, or expires, the default allocation is restored.

# FILES

_/usr/sbin/macpro-fand_
	The daemon.

_/etc/macpro-fand.conf_
	Settings; see *macpro-fand.conf*(5).

_/run/macpro-fand/override_
	The override line (see OVERRIDE).

_/run/macpro-fand/owner.lock_
	Held by the process that owns the fans and power limits. A second
	*macpro-fand* refuses to start while it is held.

_/run/macpro-fand/smc.lock_
	An flock taken around every two-step SMC key read (*key_at_index*
	then the key's value). Other tools that read SMC keys the same way
	should take it too. Both lock files are root-only (mode 0600) and
	never opened through a symbolic link; the SMC lock is waited for at
	most 200 ms per read (2 s at start), so a tool holding it costs a
	reading, never the fan loop.

# LOGGING

Everything goes to the journal as one JSON object per line: the
configuration at start, every alarm and override change, every budget
reallocation, and the full state every 30 s (per-die junction, HBM and power;
CPU package temperature, target and power; every zone's error, rate and rpm;
fan settings and tachometers; GPU caps, PL1, the budget's cost, the measured
DC power and the dies' working-temperature ceilings).

	journalctl -u macpro-fand -o cat | tail -1 | python3 -m json.tool

# EXIT STATUS

*0*
	Clean stop.

*1*
	Error (a required sensor missing, the fans would not take a manual
	setting, t2fanrd or another *macpro-fand* running when started by
	hand), or a stop by hand whose power recovery is incomplete (fans left
	at full speed).

*78*
	Not a Mac Pro 7,1, no applesmc fan attributes (the applesmc-t2 driver
	is not loaded), or an invalid configuration file: nothing was written.
	The service does not restart on this status.

# REQUIREMENTS

A Mac Pro 7,1 with the applesmc-t2 driver (fan control through the T2 chip),
the amdgpu driver for the GPU dies, coretemp and the intel-rapl powercap
interface. Must run as root.

Supported GPUs are Vega 20 dies (Radeon Pro Vega II and Vega II Duo MPX
modules): the 200 W charge for a die without a power cap and the power
calibration are Vega 20's. Other GPU models are logged as unsupported. GPU
dies and uncapped devices are counted at start; after adding or removing a
device, restart the service.

# EXAMPLES

Hold every fan at full speed for ten minutes during a benchmark:

	echo "max ttl=600" > /run/macpro-fand/override

Run a benchmark with every GPU die at 110 W and give control back (a pin above
the configured maximum, 125 W as installed, is lowered to it):

	echo "auto gpu=110 ttl=7200" > /run/macpro-fand/override++
	...++
	rm /run/macpro-fand/override

See what the daemon is doing:

	systemctl status macpro-fand++
	journalctl -u macpro-fand -o cat -f

Check a changed configuration before restarting:

	macpro-fand --check-config && systemctl restart macpro-fand

Hand the fans back to t2fanrd:

	systemctl stop macpro-fand

# SEE ALSO

*macpro-fand.conf*(5), *t2fanrd*(1), *systemd.service*(5),
*resize-amdgpu-bars*(8)

# AUTHORS

Exabit, Inc. <packages@exabit.io>. Report security issues to
<security@exabit.io>.
