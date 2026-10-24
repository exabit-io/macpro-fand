#!/bin/bash
# run-tests.sh -- failure-path tests for macpro-fand on the real box, as a transient unit that mirrors
# macpro-fand.service (Type=notify, watchdog, Restart=always, ExecStartPre stops t2fanrd, OnFailure=macpro-fand-backup,
# ExecStopPost --stopped; t2fanrd is the backup governor and its drop-in refuses to start it beside macpro-fand).
# Needs the 2.0.1+ system files installed (macpro-fand-backup.service, the t2fanrd drop-in and /usr/lib/macpro-fand/*).
# Test numbers are stable across releases: T1-T12 service and failure paths, T13-T23 power budget, T24-T25 the
# 2.0.1 fail-safes (numbered T13/T14 in the 2.0.1 log), T26-T31 the 2.0.2 sensor threads and peripherals.
# Needs: idle box, no other fan experiment running, t2fanrd installed. Leaves t2fanrd in charge at the end.
# Each test prints PASS/FAIL; the fans are never left below their t2fanrd/floor settings.
# The machine's load and telemetry tools are parameters (no defaults outside this tree):
#   BIN       the macpro-fand executable under test (default: this tree's release build)
#   BURN      a GPU burner: BURN SECONDS loads the die HIP_VISIBLE_DEVICES selects, exit 0 when it ran its time
#   CPU_LOAD  a CPU load helper: CPU_LOAD THREADS SECONDS, exit 0 when every worker ran its time
#   SMC_LIB   a directory with smc.py, whose SMC().read(['PZ0G']) returns the measured DC power
#   CONF      the configuration for every test but T22 (default tests/hardware/test.conf); T22 uses ./macpro-fand.conf
: "${BURN:?set BURN}" "${CPU_LOAD:?set CPU_LOAD}" "${SMC_LIB:?set SMC_LIB}"
D=$(cd "$(dirname "$0")/../.." && pwd); BIN="${BIN:-$D/target/release/macpro-fand}"; U=macpro-fand-test
S=/sys/devices/LNXSYSTM:00/LNXSYBUS:00/PNP0A08:00/device:1f/APP0001:00
OV=/run/macpro-fand/override
pass=0; fail=0
ok()  { echo "PASS  $*"; pass=$((pass + 1)); }
bad() { echo "FAIL  $*"; fail=$((fail + 1)); }
outs() { for i in 1 2 3 4; do printf '%s ' "$(cat $S/fan${i}_output)"; done; }
mans() { for i in 1 2 3 4; do printf '%s' "$(cat $S/fan${i}_manual)"; done; }
# wait_outs "730 1150 1150 1150 " SECONDS -> 0 when the fan outputs read exactly that
wait_outs() { local end=$((SECONDS + $2)); while [ $SECONDS -lt $end ]; do [ "$(outs)" = "$1" ] && return 0; sleep 1; done; return 1; }
wait_active() { local end=$((SECONDS + $3)); while [ $SECONDS -lt $end ]; do [ "$(systemctl is-active $1)" = "$2" ] && return 0; sleep 0.5; done; return 1; }
want() { [ -z "${TESTS:-}" ] || [[ " $TESTS " == *" $1 "* ]]; }   # TESTS="T6 T10" runs only those
idle_jobs() { local end=$((SECONDS + 30)); while [ -n "$(systemctl list-jobs --no-legend 2>/dev/null)" ] && [ $SECONDS -lt $end ]; do sleep 0.5; done; }
start_unit() {  # start_unit [ENV=VAL ...]; waits out pending jobs (a queued t2fanrd start makes ours "destructive")
  idle_jobs
  local env=(); for e in "$@"; do env+=(-p "Environment=$e"); done
  systemd-run --unit=$U --collect -p Type=notify -p NotifyAccess=main -p WatchdogSec=20 -p Restart=${RESTART:-always} \
    -p RestartSec=2 -p TimeoutStopSec=15 -p OnFailure=macpro-fand-backup.service \
    -p "ExecStartPre=-/usr/bin/systemctl stop t2fanrd.service" \
    -p "ExecStopPost=$BIN --stopped" "${env[@]}" "$BIN" --config "${CONF:-$D/tests/hardware/test.conf}" >/dev/null 2>&1
}
stop_unit() { systemctl stop $U 2>/dev/null; wait_active $U inactive 20; systemctl reset-failed $U 2>/dev/null; }
CAPS="1200 2500 2500 2500 "
# Under the zone governors every fan follows its zone's thermal targets: manual, inside its own floor..cap
# (blower 600-1200, front fans 500-2500).
governed() { local o; o=($(outs)); [ "$(mans)" = 1111 ] && [ "${o[0]}" -ge 600 ] && [ "${o[0]}" -le 1200 ] && for i in 1 2 3; do [ "${o[$i]}" -ge 500 ] && [ "${o[$i]}" -le 2500 ] || return 1; done; }
wait_governed() { local end=$((SECONDS + $1)); while [ $SECONDS -lt $end ]; do governed && return 0; sleep 1; done; return 1; }
# Read-only preflight first: nothing is stopped or removed when the box is not ready.
[ -z "$(ls /sys/class/kfd/kfd/proc 2>/dev/null)" ] || { echo "a GPU job is running; not testing"; exit 2; }
systemctl cat t2fanrd.service >/dev/null 2>&1 || { echo "t2fanrd is not installed; not testing"; exit 2; }
systemctl cat t2fanrd.service 2>/dev/null | grep -q 't2fanrd-guard may-start' && systemctl cat macpro-fand-backup.service >/dev/null 2>&1 \
  || { echo "the macpro-fand 2.0.1 system files (t2fanrd drop-in, macpro-fand-backup.service) are not installed; not testing"; exit 2; }
exec 9<>/run/macpro-fand-experiment.lock && flock -n 9 || { echo "another experiment holds the experiment lock; not testing"; exit 2; }
CONFIG_USED="${CONF:-$D/tests/hardware/test.conf}"
"$BIN" --check-config --config "$CONFIG_USED" >/dev/null 2>&1 || { echo "invalid test configuration $CONFIG_USED"; exit 2; }
echo "macpro-fand $("$BIN" --version 2>&1 | awk '{print $2}') ($BIN -> $(readlink -f "$BIN")), config $CONFIG_USED"
echo "tested daemon sha256 $(sha256sum < "$(readlink -f "$BIN")" | cut -c1-64); installed $(dpkg-query -W -f='${Version}' macpro-fand 2>/dev/null) sha256 $(sha256sum < /usr/sbin/macpro-fand 2>/dev/null | cut -c1-64)"
# The state to give back on any exit: which fan owner was running, and the override file's exact content.
INSTALLED=0; systemctl is-active --quiet macpro-fand && INSTALLED=1

OVSAVE=$(mktemp); OVHAD=0
[ -f $OV ] && { cp -p $OV "$OVSAVE" || { echo "cannot snapshot $OV; not testing"; rm -f "$OVSAVE"; exit 2; }; OVHAD=1; }
LOGD=$(mktemp -d) || { echo "cannot create a private log directory; not testing"; rm -f "$OVSAVE"; exit 2; }
restore() {
  trap - EXIT INT TERM
  local kids; kids=$(jobs -p); [ -n "$kids" ] && kill $kids 2>/dev/null; wait 2>/dev/null
  systemctl stop $U 2>/dev/null; systemctl reset-failed $U 2>/dev/null
  if [ $OVHAD = 1 ]; then
    cp -p "$OVSAVE" $OV && cmp -s "$OVSAVE" $OV && echo "override restored: $(cat $OV)" \
      || { echo "RESTORE FAILED: override"; fail=$((fail + 1)); }
  else rm -f $OV; fi
  rm -f "$OVSAVE"; echo "logs: $LOGD"
  systemctl unmask --runtime t2fanrd.service >/dev/null 2>&1
  idle_jobs
  if [ $INSTALLED = 1 ]; then
    systemctl start macpro-fand; wait_active macpro-fand active 60
    echo "restored: installed macpro-fand $(systemctl is-active macpro-fand)"
    systemctl is-active --quiet macpro-fand || { echo "RESTORE FAILED: macpro-fand is not running"; fail=$((fail + 1)); }
  else
    systemctl is-active --quiet t2fanrd || systemctl start t2fanrd
    wait_active t2fanrd active 30
    echo "restored: t2fanrd $(systemctl is-active t2fanrd)"
    systemctl is-active --quiet t2fanrd || { echo "RESTORE FAILED: t2fanrd is not running"; fail=$((fail + 1)); }
  fi
  echo "end: t2fanrd $(systemctl is-active t2fanrd), outputs $(outs), manual $(mans); $pass passed, $fail failed"
  [ $fail = 0 ]
}
trap 'rc=$?; restore; r=$?; [ $rc -ne 0 ] && exit $rc; exit $r' EXIT   # a failure stays a failure, restored or not
trap 'echo "interrupted"; fail=$((fail + 1)); exit 130' INT TERM
# an installed macpro-fand would see the test unit as a second writer: stop it (its stop hands over to t2fanrd);
# restore() starts it again on any exit
[ $INSTALLED = 1 ] && { systemctl stop macpro-fand; wait_active t2fanrd active 30; }
rm -f $OV; mkdir -p /run/macpro-fand
systemctl is-active --quiet t2fanrd || systemctl start t2fanrd
echo "start: t2fanrd $(systemctl is-active t2fanrd), outputs $(outs), manual $(mans)"

if want T1; then
# T1 dry run: computes and logs, writes nothing, leaves t2fanrd alone
before="$(outs)$(mans)"; timeout 25 "$BIN" --dry-run > $LOGD/dry.log 2>&1
grep -q '"dry_run": true' $LOGD/dry.log && grep -q '"rpm"' $LOGD/dry.log && systemctl is-active --quiet t2fanrd \
  && [ "$(outs)$(mans)" = "$before" ] \
  && ok "T1 dry run logs, writes no fan and leaves t2fanrd running" || bad "T1 dry run (fans $before -> $(outs)$(mans); $(tail -2 $LOGD/dry.log))"
# by hand while t2fanrd runs: must refuse and touch nothing
timeout 10 "$BIN" > $LOGD/refuse.log 2>&1; rc=$?
[ $rc -ne 0 ] && grep -q 't2fanrd is running' $LOGD/refuse.log && systemctl is-active --quiet t2fanrd \
  && ok "T1b refuses to run by hand beside t2fanrd" || bad "T1b refuse (rc $rc: $(tail -1 $LOGD/refuse.log))"
fi

if want T2; then
# T2 takeover: ExecStartPre stops t2fanrd, floors verified, READY
start_unit
wait_active $U active 30 && ! systemctl is-active --quiet t2fanrd && wait_governed 20 \
  && ok "T2 takeover: t2fanrd stopped, governed: $(outs)" || bad "T2 takeover: unit $(systemctl is-active $U), t2fanrd $(systemctl is-active t2fanrd), $(outs) manual $(mans)"
fi

if want T3; then
# T3 override max / hold / auto, ttl expiry
echo max > $OV; wait_outs "$CAPS" 10 && ok "T3a override max -> caps" || bad "T3a max: $(outs)"
echo "hold 600 900 900 900" > $OV; wait_outs "600 900 900 900 " 400 && ok "T3b hold 600/900 (blower not coupled)" || bad "T3b hold: $(outs)"
# an expired hold: the daemon notes it and the governors take over from the held rpm, toward their own targets
echo "hold 700 1400 1400 1400 ttl=5" > $OV; sleep 12
journalctl -u $U --since -15s --no-pager | grep -q '"mode": "auto", "note": "override expired"' && governed \
  && ok "T3c expired hold ignored -> governors in control ($(outs))" || bad "T3c ttl: $(outs)"
echo "hold banana" > $OV; sleep 5; journalctl -u $U --since -10s --no-pager | grep -q 'malformed' && ok "T3d malformed override logged and ignored" || bad "T3d malformed"
rm -f $OV
fi

if want T4; then
# T4 a second writer on fan 2 -> alarm -> caps
end=$((SECONDS + 20)); while [ $SECONDS -lt $end ]; do echo 1 > $S/fan2_manual; echo 1900 > $S/fan2_output; sleep 0.5; done
journalctl -u $U --since -30s --no-pager | grep -q 'another writer' && ok "T4 second writer detected (alarm)" || bad "T4 second writer"
stop_unit; start_unit; wait_active $U active 30   # back to the start points instead of a slow ramp down from the caps
fi

if want T5; then
# T5 kill -9 -> ExecStopPost full speed -> restart -> floors
pid=$(systemctl show -p MainPID --value $U); kill -9 "$pid"
sleep 1.5; o=$(outs)
[ "$o" = "$CAPS" ] && wait_active $U active 20 && wait_governed 30 && ok "T5 kill -9: caps ($o) then restarted under the governors ($(outs))" || bad "T5 kill -9: after-kill $o, now $(outs), unit $(systemctl is-active $U)"
stop_unit
fi

if want T6; then
# T6 GPU reader stalls (as on a stuck amdgpu read) -> 'gpu stale' -> caps, while the loop keeps running
start_unit MACPRO_FAND_TEST_STALL_GPU=15; wait_active $U active 30
wait_outs "$CAPS" 60 && journalctl -u $U --since -70s --no-pager | grep -q 'gpu stale' && systemctl is-active --quiet $U \
  && ok "T6 stalled GPU reader -> caps, loop alive" || bad "T6 gpu stall: $(outs)"
stop_unit
fi

if want T7; then
# T7 dead fan tach (fan 3) -> alarm after settle + fail time, latched at caps
start_unit MACPRO_FAND_TEST_DEAD_FAN=3; wait_active $U active 30
wait_outs "$CAPS" 90 && sleep 60 && [ "$(outs)" = "$CAPS" ] && journalctl -u $U --since -150s --no-pager | grep -q 'fan3 at 0' \
  && ok "T7 dead fan -> caps, latched for 60 s" || bad "T7 dead fan: $(outs)"
stop_unit
fi

if want T8; then
# T8 clean stop -> caps then t2fanrd
start_unit; wait_active $U active 30; wait_governed 30
systemctl stop $U; wait_active t2fanrd active 30 && ok "T8 clean stop hands the fans to t2fanrd" || bad "T8 clean stop: t2fanrd $(systemctl is-active t2fanrd), $(outs)"
systemctl reset-failed $U 2>/dev/null
fi

if want T9; then
# T9 macpro-fand wins: starting t2fanrd beside a running macpro-fand is refused (ExecCondition) and changes nothing
start_unit; wait_active $U active 30; wait_governed 30
systemctl start t2fanrd; sleep 5
wait_active $U active 5 && ! systemctl is-active --quiet t2fanrd && governed \
  && ok "T9 t2fanrd start beside macpro-fand is refused" || bad "T9: unit $(systemctl is-active $U), t2fanrd $(systemctl is-active t2fanrd), $(outs)"
stop_unit
fi

# T10 'systemctl restart' keeps macpro-fand in charge (the stop hook must not hand the fans to t2fanrd mid-restart)
if want T10; then
start_unit; wait_active $U active 30; wait_governed 30
systemctl restart $U; sleep 3; idle_jobs
wait_active $U active 30 && ! systemctl is-active --quiet t2fanrd && wait_governed 30 \
  && ok "T10 restart keeps macpro-fand in charge" || bad "T10 restart: unit $(systemctl is-active $U), t2fanrd $(systemctl is-active t2fanrd), $(outs)"
stop_unit
fi

# T11 the restart window belongs to macpro-fand: after a crash (watchdog/SIGKILL), t2fanrd started during the 2 s
# auto-restart wait is refused, and macpro-fand comes back in charge
if want T11; then
start_unit; wait_active $U active 30; wait_governed 30
systemctl kill -s KILL $U; sleep 0.5
systemctl start t2fanrd; sleep 1
refused=1; systemctl is-active --quiet t2fanrd && refused=0
wait_active $U active 30 && ! systemctl is-active --quiet t2fanrd && wait_governed 30 && [ $refused = 1 ] \
  && ok "T11 t2fanrd refused in the restart window; macpro-fand back in charge" || bad "T11: refused=$refused unit $(systemctl is-active $U), t2fanrd $(systemctl is-active t2fanrd), $(outs)"
stop_unit
fi

# T12 a permanent failure goes through macpro-fand-backup (full speed, then t2fanrd), and the next start takes the fans back
if want T12; then
RESTART=no start_unit; wait_active $U active 30; wait_governed 30
systemctl kill -s KILL $U
wait_active t2fanrd active 30 && [ "$(systemctl is-active $U)" != active ] && t12a=1 || t12a=0
systemctl reset-failed $U 2>/dev/null; idle_jobs
start_unit; wait_active $U active 30
[ $t12a = 1 ] && ! systemctl is-active --quiet t2fanrd && wait_governed 30 \
  && ok "T12 permanent failure -> t2fanrd; next start takes the fans back" || bad "T12: first=$t12a unit $(systemctl is-active $U), t2fanrd $(systemctl is-active t2fanrd), $(outs)"
stop_unit
fi

# T24 fail-safe: t2fanrd stopping while no macpro-fand owns the fans leaves every fan at full speed
if want T24; then
idle_jobs; systemctl is-active --quiet t2fanrd || systemctl start t2fanrd; wait_active t2fanrd active 30; sleep 5
systemctl stop t2fanrd
wait_outs "$CAPS" 30 && [ "$(mans)" = 1111 ] && ok "T24 t2fanrd stop with no owner -> full speed: $(outs)" || bad "T24: $(outs) manual $(mans)"
systemctl start t2fanrd; wait_active t2fanrd active 30
fi

# T25 fail-safe without the backup: t2fanrd unavailable (masked) and macpro-fand failed for good -> full speed, no t2fanrd
if want T25; then
RESTART=no start_unit; wait_active $U active 30; wait_governed 30
systemctl mask --runtime t2fanrd.service >/dev/null 2>&1
systemctl kill -s KILL $U; sleep 8; idle_jobs
wait_outs "$CAPS" 30 && [ "$(mans)" = 1111 ] && ! systemctl is-active --quiet t2fanrd \
  && ok "T25 no backup: fans stay at full speed" || bad "T25: $(outs) manual $(mans), t2fanrd $(systemctl is-active t2fanrd)"
systemctl unmask --runtime t2fanrd.service >/dev/null 2>&1; systemctl reset-failed $U 2>/dev/null
systemctl start t2fanrd; wait_active t2fanrd active 30
fi

# ---- power budget (needs the calibrated model in the daemon; loads stay well under 1228 W: GPU burn ~770 W) ----
# The Vega 20 dies by bus number, in the daemon's order (the numbers move when slot cards change: 1b/1e became 19/1c
# when a card went into slot 5 on 2026-10-23).
DIES=$(for d in /sys/bus/pci/drivers/amdgpu/0000:*; do [ -d "$d/hwmon" ] && basename "$d" | cut -c6-7; done | sort | tr '\n' ' ')
D0=${DIES%% *}
caps() { for d in $DIES; do printf '%s ' $(( $(cat /sys/bus/pci/devices/0000:$d:00.0/hwmon/hwmon*/power1_cap) / 1000000 )); done; printf 'pl1 %s' $(( $(cat /sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw) / 1000000 )); }
pl2() { echo $(( $(cat /sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw) / 1000000 )); }
BP=()
burn() { BP=(); for i in 0 1 2 3; do HIP_VISIBLE_DEVICES=$i timeout $(( $1 + 5 )) $BURN $1 >/dev/null 2>&1 & BP+=($!); done; }
burn_ok() { local p r=0; for p in "${BP[@]}"; do wait "$p" || r=1; done; BP=(); return $r; }   # every burner exited 0
dcnow() { python3 -c "import sys; sys.path.insert(0,'$SMC_LIB'); from smc import SMC; print(round(SMC().read(['PZ0G'])['PZ0G']))"; }
budget_line() { journalctl -u $U --since "$1" -o cat --no-pager | grep '"budget": {"limit"' | tail -1; }
# The static-safe expectation comes from the pinned 1.0.6 reference's formula; only the base (the measured box plus
# every peripheral's allowance, which 2.0.2 classifies per device) is the daemon's own, from --inventory.
BUDGET_BASE=$("$BIN" --inventory --config "$CONFIG_USED" | python3 -c 'import json,sys; print(json.load(sys.stdin)["budget_base"])')
# The budget tests' test budget: 900 W on the bare test box (base 127 W), raised by whatever the peripherals add to the
# base, so the dies keep the same headroom above the CPU's share (900 W with a 262 W base leaves the dies none).
TB=$(python3 -c "print(round(900 + $BUDGET_BASE - 127))")
SAFE=$(python3 -c "
import importlib.machinery as m, types; l=m.SourceFileLoader('mf','$D/reference/v1.0.6/macpro-fand.py'); mod=types.ModuleType('mf'); l.exec_module(mod)
mod.load_config('$CONFIG_USED'); mod.base = lambda: $BUDGET_BASE; s = mod.static_safe() if mod.budget_ready() else None
print(f\"{int(s['gpu'])} {int(s['gpu'])} {int(s['gpu'])} {int(s['gpu'])} pl1 {int(s['pl1'])}\" if s else 'uncalibrated')" 2>/dev/null)

if want T13; then
stop_unit   # a unit left running by an earlier test (T2 run alone) would make this start a no-op
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit; wait_active $U active 60; sleep 6; C1=$(caps); L=$(budget_line "$T0")
BP1=$(echo "$L" | grep -o '"pl1": [0-9]*' | head -1 | grep -o '[0-9]*$')
echo "$L" | grep -q '"gpu": \[125, 125, 125, 125\]' && [ "${C1% pl1*}" = "125 125 125 125" ] && [ -n "$BP1" ] && [ "${C1##*pl1 }" = "$BP1" ] && [ "$(pl2)" = "${C1##*pl1 }" ] && ok "T13a start: GPUs 125 W, $C1, PL2 $(pl2) ($(echo $L | grep -o '"cost": [0-9]*'))" || bad "T13a start: $C1 PL2 $(pl2) / $L"
stop_unit; sleep 3; C2=$(caps)
[ "$C2" = "$SAFE" ] && [ "$(pl2)" = "${C2##*pl1 }" ] && ok "T13b clean stop -> static-safe limits ($C2, PL2 $(pl2); $SAFE)" || bad "T13b stop: $C2 PL2 $(pl2) (want $SAFE)"
fi

if want T14; then
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_BUDGET=$TB; wait_active $U active 60; sleep 6
burn 150; mx=0; end=$((SECONDS + 140)); while [ $SECONDS -lt $end ]; do d=$(dcnow); [ "$d" -gt "$mx" ] && mx=$d; sleep 2; done; C1=$(caps)
burn_ok; bok=$?; G=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep -c 'rebalance (gpu wants)')
[ $bok = 0 ] && [ "$mx" -ge 500 ] && [ "$G" -gt 0 ] && [ "$mx" -le $((TB + 30)) ] && ok "T14 GPU-only under a $TB W test budget: $G rebalances, caps now $C1, max DC $mx W" || bad "T14: rebalances $G, caps $C1, max DC $mx, burn ok $bok"
stop_unit
fi

if want T15; then
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_BUDGET=$TB; wait_active $U active 60; sleep 6
burn 150; "$CPU_LOAD" 56 150 & CL=$!; mx=0; end=$((SECONDS + 140)); while [ $SECONDS -lt $end ]; do d=$(dcnow); [ "$d" -gt "$mx" ] && mx=$d; sleep 2; done; C1=$(caps)
burn_ok; bok=$?; wait $CL; cok=$?
[ $bok = 0 ] && [ $cok = 0 ] && [ "$mx" -ge 500 ] && [ "$mx" -le $((TB + 30)) ] && ok "T15 GPU + CPU spike under a $TB W test budget: max DC $mx W, caps $C1" || bad "T15 spike: max DC $mx W (budget $TB, loaded >= 500), caps $C1, burn ok $bok, cpu load ok $cok"
stop_unit
fi

if want T16; then
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_BACKSTOP=500; wait_active $U active 60; sleep 6
burn 60; end=$((SECONDS + 40)); while [ $SECONDS -lt $end ] && ! journalctl -u $U --since "$T0" -o cat --no-pager | grep -q 'power backstop'; do sleep 1; done; sleep 3; C1=$(caps); P2=$(pl2); burn_ok; bok=$?
[ $bok = 0 ] && journalctl -u $U --since "$T0" -o cat --no-pager | grep -q 'power backstop' && [ "$C1" = "85 85 85 85 pl1 80" ] && [ "$P2" = 80 ] && ok "T16 backstop: DC over 500 W (test) -> every limit to its floor ($C1, PL2 $P2)" || bad "T16 backstop: $C1, PL2 $P2, burn ok $bok"
stop_unit
fi

if want T17; then
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit; wait_active $U active 60; sleep 6
echo 200000000 > /sys/bus/pci/devices/0000:$D0:00.0/hwmon/hwmon*/power1_cap; sleep 6; C1=$(caps)
journalctl -u $U --since "$T0" -o cat --no-pager | grep -q 'changed by someone else' && [ "${C1%% *}" = 125 ] && ok "T17 outside write ($D0 -> 200 W) overwritten ($C1)" || bad "T17 outside write: $C1"
stop_unit
fi

if want T18; then   # the governor moves the caps away from 125 W under GPU load, then dies: the stop hook restores static-safe
start_unit MACPRO_FAND_TEST_BUDGET=$TB; wait_active $U active 60; sleep 6; burn 90
end=$((SECONDS + 80)); while [ $SECONDS -lt $end ] && [ "$(caps | cut -d' ' -f1-4)" = "125 125 125 125" ]; do sleep 2; done; C0=$(caps)
pid=$(systemctl show -p MainPID --value $U); kill -9 "$pid"; sleep 1.5; C1=$(caps); burn_ok; bok=$?
[ $bok = 0 ] && [ "$C0" != "$C1" ] && [ "${C1% pl1*}" = "125 125 125 125" ] && ok "T18 kill -9 -> static-safe limits before the restart ($C0 -> $C1)" || bad "T18 kill -9: $C0 -> $C1"
stop_unit
fi

if want T19; then   # override pins: pl1=60 (experiments may go below the floor) -> PL1 60 W; pin removed -> back to the budget's share
stop_unit; start_unit; wait_active $U active 60; sleep 6; P0=$(caps)
echo "auto pl1=60" > $OV; sleep 10; P1=$(caps); rm -f $OV; sleep 10; P2=$(caps)
[ "${P1##*pl1 }" = 60 ] && [ "$(pl2)" = "${P2##*pl1 }" ] && [ "${P2##*pl1 }" = "${P0##*pl1 }" ] && ok "T19 pl1 pin: $P0 -> $P1 -> $P2 (PL2 follows)" || bad "T19 pl1 pin: $P0 -> $P1 -> $P2, PL2 $(pl2)"
stop_unit
fi

if want T20; then   # CPU first: dies borrow the idle CPU's budget, then a CPU load takes it back at once, evenly from the dies
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_BUDGET=$TB; wait_active $U active 60; sleep 6
burn 200; end=$((SECONDS + 90)); while [ $SECONDS -lt $end ]; do sleep 2; done; C0=$(caps)
T1=$(date '+%Y-%m-%d %H:%M:%S'); "$CPU_LOAD" 56 90 & CL=$!; mx=0; end=$((SECONDS + 30))
while [ $SECONDS -lt $end ]; do d=$(dcnow); [ "$d" -gt "$mx" ] && mx=$d; sleep 2; done; C1=$(caps); burn_ok; bok=$?; wait $CL; cok=$?
CW=$(journalctl -u $U --since "$T1" -o cat --no-pager | grep -c 'rebalance (cpu wants)'); g0=(${C0% pl1*}); g=(${C1% pl1*}); gave=1
for i in 0 1 2 3; do [ "${g[$i]}" -le $(( ${g0[$i]} + 1 )) ] || gave=0; done   # every die gave (or held), none gained
[ $bok = 0 ] && [ $cok = 0 ] && [ "$CW" -gt 0 ] && [ "${C1##*pl1 }" -gt "${C0##*pl1 }" ] && [ $gave = 1 ] && [ "$mx" -le $((TB + 30)) ] \
  && ok "T20 CPU first: $C0 -> CPU load -> $C1, max DC $mx W" || bad "T20 CPU first: $C0 -> $C1, cpu-wants $CW, max DC $mx"
stop_unit
fi

if want T21; then   # working-temperature ceilings: a 45 C test target under GPU burn brings every die's cap down
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_TWORK=45; wait_active $U active 60; sleep 6
burn 150; end=$((SECONDS + 140)); while [ $SECONDS -lt $end ]; do sleep 2; done; C1=$(caps); burn_ok; bok=$?
L=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep '"tcap"' | tail -1 | grep -o '"tcap": \[[0-9, ]*\]'); g=(${C1% pl1*})
W=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep -c 'another writer')
[ $bok = 0 ] && [ "$W" = 0 ] && [ ${#g[@]} = 4 ] && [ "${g[0]}" -lt 125 ] && [ "${g[1]}" -lt 125 ] && [ "${g[2]}" -lt 125 ] && [ "${g[3]}" -lt 125 ] && ok "T21 working-temperature ceilings (45 C test): caps $C1, $L" || bad "T21 ceilings: caps $C1, $L, second-writer alarms $W"
stop_unit
fi

if want T22; then   # the shipped /etc/macpro-fand.conf (GPU_CAP max 125): no cap above 125 W under a 4-die burn, CPU idle
T0=$(date '+%Y-%m-%d %H:%M:%S'); CONF=$D/macpro-fand.conf start_unit; wait_active $U active 60; sleep 6
burn 90; mx=0; n=0; end=$((SECONDS + 80)); while [ $SECONDS -lt $end ]; do cs=($(caps | cut -d' ' -f1-4)); [ ${#cs[@]} = 4 ] && n=$((n + 1)); for c in "${cs[@]}"; do [ "$c" -gt "$mx" ] && mx=$c; done; sleep 2; done; C1=$(caps); burn_ok; bok=$?
W=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep -c 'another writer')
[ $bok = 0 ] && [ "$W" = 0 ] && [ $n -ge 30 ] && [ "$mx" -ge 85 ] && [ "$mx" -le 125 ] && ok "T22 shipped config ceiling: caps never above 125 W under a 4-die burn (max $mx, now $C1, $n samples)" || bad "T22 shipped config ceiling: max cap $mx, $n samples, burn ok $bok, second-writer alarms $W"
stop_unit
fi

if want T23; then   # owner transitions: a unit started beside a foreground owner is refused and its stop hook leaves
                    # the owner alone; the owner's own stop (by hand) confirms the static-safe limits, then t2fanrd
stop_unit; systemctl stop t2fanrd; wait_active t2fanrd inactive 20
"$BIN" --config "$CONFIG_USED" > $LOGD/fg.log 2>&1 & FG=$!
end=$((SECONDS + 90)); until grep -q '"confirmed": true' $LOGD/fg.log || [ $SECONDS -ge $end ]; do sleep 1; done
C0=$(caps); o0=$(outs); T0=$(date '+%Y-%m-%d %H:%M:%S')
start_unit; sleep 10
J=$(journalctl -u $U --since "$T0" -o cat --no-pager)
refused=$(echo "$J" | grep -c 'another macpro-fand owns'); left=$(echo "$J" | grep -c 'left alone')
wrote=$(echo "$J" | grep -c '"static_safe"'); C1=$(caps); o1=$(outs)       # the hook logs static_safe whenever it acts
stop_unit
kill -TERM $FG; wait $FG; frc=$?; sleep 2; C2=$(caps)
[ "$refused" -ge 1 ] && [ "$left" -ge 1 ] && [ "$wrote" = 0 ] && [ "$C1" = "$C0" ] && [ "$o1" != "$CAPS" ] \
  && ok "T23a unit beside a foreground owner: refused ($refused), its stop hook left the owner alone ($left x, no power transaction), limits $C1, fans $o1" \
  || bad "T23a owner conflict: refused $refused, left alone $left, hook transactions $wrote, limits $C0 -> $C1, fans $o0 -> $o1"
grep -q '"static_safe": {.*"ok": true' $LOGD/fg.log && [ $frc = 0 ] && [ "$C2" = "$SAFE" ] && wait_active t2fanrd active 30 \
  && ok "T23b foreground stop: worker stopped, static-safe confirmed ($C2), t2fanrd in charge (exit $frc)" \
  || bad "T23b foreground stop: exit $frc, limits $C2 (want $SAFE), t2fanrd $(systemctl is-active t2fanrd), $(grep static_safe $LOGD/fg.log | tail -1 | cut -c1-200)"
fi

# ---- 2.0.2: sensor I/O off the control thread, peripheral inventory and sensors ----
status_line() { journalctl -u $U --since "$1" -o cat --no-pager | grep '"alarms"' | tail -1; }
start_line() { journalctl -u $U --since "$1" -o cat --no-pager | grep '"version"' | tail -1; }
nrestarts() { systemctl show -p NRestarts --value $U; }

if want T26; then   # inventory: what the daemon charges matches what the kernel shows, and the budget base adds up
INV=$("$BIN" --inventory --config "$CONFIG_USED")
r=$(echo "$INV" | python3 -c '
import json, sys, glob, os
inv = json.load(sys.stdin); P = inv["peripherals"]; bad = []
def cls(k): return [p for p in P if p["class"] == k]
mlx = {os.path.basename(d)[:-2] for d in glob.glob("/sys/bus/pci/devices/*.0") if open(d + "/vendor").read().strip() == "0x15b3" and not os.path.exists(d + "/physfn")}
if {p["key"] for p in cls("mellanox")} != mlx: bad.append(f"mellanox {mlx}")
nv = [n for n in glob.glob("/sys/class/nvme/nvme*") if open(os.path.realpath(n + "/device") + "/vendor").read().strip() != "0x106b"]
if len(cls("nvme")) + len(cls("nvme-switched")) != len(nv): bad.append(f"nvme {len(nv)}")
for b in glob.glob("/sys/block/sd*"):
    k = os.path.basename(b); usb = "/usb" in os.path.realpath(b + "/device")
    want = "usb-disk" if usb and not os.path.exists(b + "/device/vpd_pgb1") else None
    if not usb: want = "disk" if open(b + "/queue/rotational").read().strip() == "1" else "ssd"
    got = [p for p in P if p["key"] == k]
    if want and (not got or got[0]["class"] != want or (usb and got[0]["fan"] is not None)): bad.append(f"{k} want {want} got {got}")
if abs(inv["budget_base"] - inv["base"] - sum(p["w"] for p in P)) > 1e-6: bad.append("base sum")
print("ok" if not bad else "; ".join(bad), len(P), inv["budget_base"])')
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit; wait_active $U active 60; sleep 4; L=$(start_line "$T0"); stop_unit
[ "${r%% *}" = ok ] && echo "$L" | grep -q "\"base\": ${r##* }" && ok "T26 inventory matches the kernel ($r): budget base ${r##* } W" \
  || bad "T26 inventory: $r / $(echo "$L" | grep -o '"base": [0-9]*')"
fi

if want T27; then   # a temperature read that hangs: stale -> read alarms -> caps, written by the fan thread; the
                    # keepalive continues (the fans are safe), so no watchdog restart
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_STALL_TEMPS=20; wait_active $U active 60; wait_governed 30
n0=$(nrestarts); wait_outs "$CAPS" 60; c=$?; sleep 70
J=$(journalctl -u $U --since "$T0" -o cat --no-pager)
[ $c = 0 ] && [ "$(outs)" = "$CAPS" ] && [ "$(nrestarts)" = "$n0" ] && systemctl is-active --quiet $U && echo "$J" | grep -q '"read cpu"' \
  && ! journalctl -u $U --since "$T0" --no-pager | grep -q 'Watchdog timeout' \
  && ok "T27 stalled temperature reads -> caps by the fan thread, no restart after 100 s ($(status_line "$T0" | grep -o '"alarms": \[[^]]*' | cut -c1-80))" \
  || bad "T27 stalled temps: caps rc $c, $(outs), restarts $n0 -> $(nrestarts), unit $(systemctl is-active $U)"
stop_unit
fi

if want T28; then   # the fan thread hangs: keepalive withheld after IO_STALL (60 s) -> watchdog restart (20 s) ->
                    # ExecStopPost full speed -> the restarted daemon governs again
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit MACPRO_FAND_TEST_STALL_FANS=20; wait_active $U active 60; wait_governed 30
n0=$(nrestarts); end=$((SECONDS + 150)); until [ "$(nrestarts)" != "$n0" ] || [ $SECONDS -ge $end ]; do sleep 1; done
J=$(journalctl -u $U --since "$T0" --no-pager)
[ "$(nrestarts)" != "$n0" ] && echo "$J" | grep -q 'keepalive withheld' && echo "$J" | grep -q 'Watchdog timeout' \
  && wait_active $U active 30 && wait_governed 30 \
  && ok "T28 stalled fan thread -> keepalive withheld -> watchdog restart -> governed ($(outs))" \
  || bad "T28 stalled fan thread: restarts $n0 -> $(nrestarts), withheld $(echo "$J" | grep -c 'keepalive withheld'), watchdog $(echo "$J" | grep -c 'Watchdog timeout'), $(outs)"
stop_unit
fi

if want T29 && [ -n "$(ls /sys/class/hwmon/*/name 2>/dev/null | xargs grep -l drivetemp 2>/dev/null)" ]; then
# a peripheral driver unloaded: its sensors retire after RETIRE_UNBOUND (120 s) and the governors go on; loaded again,
# the same sensors read again
T0=$(date '+%Y-%m-%d %H:%M:%S'); start_unit; wait_active $U active 60; wait_governed 30
modprobe -r drivetemp; end=$((SECONDS + 240))
until journalctl -u $U --since "$T0" -o cat --no-pager | grep -q 'retired (no driver' || [ $SECONDS -ge $end ]; do sleep 2; done
sleep 10; r1=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep -c 'retired (no driver'); g1=0; wait_governed 30 && g1=1
a1=$(journalctl -u $U --since "$T0" -o cat --no-pager | grep -c '"read drivetemp')   # retired before going stale
T1=$(date '+%Y-%m-%d %H:%M:%S'); modprobe drivetemp; end=$((SECONDS + 120))
until journalctl -u $U --since "$T1" -o cat --no-pager | grep -q 'reads again' || [ $SECONDS -ge $end ]; do sleep 2; done
r2=$(journalctl -u $U --since "$T1" -o cat --no-pager | grep -c 'reads again'); T2=$(date '+%Y-%m-%d %H:%M:%S')
end=$((SECONDS + 45)); until status_line "$T2" | grep -q '"drivetemp:' || [ $SECONDS -ge $end ]; do sleep 2; done
[ "$r1" -ge 1 ] && [ $g1 = 1 ] && [ "$a1" = 0 ] && [ "$r2" -ge 1 ] && status_line "$T2" | grep -q '"drivetemp:' \
  && ok "T29 drivetemp unloaded -> $r1 sensors retired without an alarm, governed ($(outs)); loaded -> $r2 read again" \
  || bad "T29 driver lost/found: retired $r1, stale alarms $a1, governed $g1, back $r2, $(outs)"
modprobe drivetemp; stop_unit
fi

if want T30 && [ -x /usr/bin/mget_temp ]; then   # TEMP_COMMAND: the Mellanox cards' temperatures through MFT
cp "$CONFIG_USED" $LOGD/t30.conf; echo "TEMP_COMMAND = {'mellanox': ('/usr/bin/mget_temp', '-d', '{pci}')}" >> $LOGD/t30.conf
T0=$(date '+%Y-%m-%d %H:%M:%S'); CONF=$LOGD/t30.conf start_unit; wait_active $U active 60; sleep 35
n=$(start_line "$T0" | grep -o '"sensor": "mellanox:[^"]*' | wc -l); L=$(status_line "$T0")
v=$(echo "$L" | grep -o '"mellanox:[^"]*": [0-9.]*' | tr '\n' ' ')
[ "$n" -ge 1 ] && [ "$(echo "$v" | wc -w)" = $((2 * n)) ] && echo "$L" | grep -q '"alarms": \[\]' \
  && ok "T30 TEMP_COMMAND sensors: $v" || bad "T30 TEMP_COMMAND: $n sensors, values '$v', $(echo "$L" | cut -c1-120)"
stop_unit
fi

if want T31; then   # UNMONITORED floor: a Mellanox card with no sensor holds its slot zone's fan at the floor
Z=$("$BIN" --inventory --config "$CONFIG_USED" | python3 -c 'import json,sys; P=[p for p in json.load(sys.stdin)["peripherals"] if p["class"]=="mellanox" and p["fan"]]; print(P[0]["fan"] if P else "")')
if [ -n "$Z" ]; then
cp "$CONFIG_USED" $LOGD/t31.conf; echo "UNMONITORED = {'mellanox': 1400}" >> $LOGD/t31.conf
T0=$(date '+%Y-%m-%d %H:%M:%S'); CONF=$LOGD/t31.conf start_unit; wait_active $U active 60; sleep 20
o=($(outs)); fl=$(start_line "$T0" | grep -o '"unmonitored_floors": \[[^]]*')
[ "${o[$((Z - 1))]}" -ge 1400 ] && governed && ok "T31 UNMONITORED mellanox 1400 rpm -> fan $Z at ${o[$((Z - 1))]} ($fl)" \
  || bad "T31 UNMONITORED: fan $Z at ${o[$((Z - 1))]}, $(outs), $fl"
stop_unit
cp "$CONFIG_USED" $LOGD/t31b.conf; echo "UNMONITORED = {'mellanox': (500, 4)}" >> $LOGD/t31b.conf   # follow fan 4
echo "CPU_TARGET = ((133, 30), (228, 31))" >> $LOGD/t31b.conf   # a low CPU target: the load raises fan 4 at once
T0=$(date '+%Y-%m-%d %H:%M:%S'); CONF=$LOGD/t31b.conf start_unit; wait_active $U active 60; sleep 10
"$CPU_LOAD" 56 120 & CL=$!; worst=0; top=0; end=$((SECONDS + 110))
while [ $SECONDS -lt $end ]; do o=($(outs)); f=${o[3]}; m=${o[$((Z - 1))]}; [ "$f" -gt "$top" ] && top=$f
  [ $((f - m)) -gt "$worst" ] && worst=$((f - m)); sleep 2; done; wait $CL; cok=$?
[ $cok = 0 ] && [ "$top" -ge 900 ] && [ "$worst" -le 150 ] && ok "T31b fan $Z follows fan 4 for a sensorless card (fan 4 up to $top, fan $Z never more than $worst rpm below)" \
  || bad "T31b follow: fan 4 max $top, fan $Z lag up to $worst rpm, cpu load ok $cok"
stop_unit
else echo "SKIP  T31 (no Mellanox card in a governed slot)"; fi
fi

: # the exit status is the failure count's (restore), never the last test's cleanup command
# restore() (the EXIT trap) gives the box back: the installed macpro-fand or t2fanrd, and the original override
