#!/bin/bash
# failover-tests.sh -- the 2.0.1 ownership rules on the INSTALLED package (macpro-fand.service, macpro-fand-backup.service,
# t2fanrd's drop-in), with the fans held at full speed throughout: macpro-fand runs under the "max" override, t2fanrd runs
# with always_full_speed=true for the duration (except ~4 s of its normal curve in F7, to show the stop raising the
# fans), and the failsafe is full speed. Safe beside a GPU job (unlike
# run-tests.sh, which moves the fans and burns the GPUs). Restores the override, /etc/t2fand.conf, the unit state and
# t2fanrd's enablement on exit.
set -u
S=/sys/bus/acpi/devices/APP0001:00; OV=/run/macpro-fand/override; T2C=/etc/t2fand.conf
DROP=/run/systemd/system/macpro-fand.service.d/zz-failover-test.conf
pass=0; fail=0
ok()  { echo "PASS  $*"; pass=$((pass + 1)); }
bad() { echo "FAIL  $*"; fail=$((fail + 1)); }
outs() { for i in 1 2 3 4; do printf '%s ' "$(cat $S/fan${i}_output)"; done; }
mans() { for i in 1 2 3 4; do printf '%s' "$(cat $S/fan${i}_manual)"; done; }
CAPS="1200 2500 2500 2500 "
wait_active() { local end=$((SECONDS + $3)); while [ $SECONDS -lt $end ]; do [ "$(systemctl is-active $1)" = "$2" ] && return 0; sleep 0.5; done; return 1; }
idle_jobs() { local end=$((SECONDS + 30)); while [ -n "$(systemctl list-jobs --no-legend 2>/dev/null)" ] && [ $SECONDS -lt $end ]; do sleep 0.5; done; }
atcaps() { [ "$(outs)" = "$CAPS" ] && [ "$(mans)" = 1111 ]; }
# after a (re)start the daemon writes its zone start values, then applies the override on its first pass: wait for it
waitcaps() { local end=$((SECONDS + ${1:-30})); while [ $SECONDS -lt $end ]; do atcaps && return 0; sleep 1; done; return 1; }
ver=$(dpkg-query -W -f='${Version}' macpro-fand 2>/dev/null)
dpkg --compare-versions "$ver" ge 2.0.1 2>/dev/null || { echo "installed macpro-fand is $ver, not 2.0.1 or later; not testing"; exit 2; }
systemctl cat t2fanrd.service >/dev/null 2>&1 || { echo "t2fanrd is not installed; not testing"; exit 2; }
T2CSAVE=$(mktemp); cp -p $T2C "$T2CSAVE"; OVSAVE=$(cat $OV 2>/dev/null); T2EN=$(systemctl is-enabled t2fanrd 2>/dev/null)
restore() {
  trap - EXIT INT TERM
  rm -f $DROP; systemctl unmask --runtime t2fanrd.service >/dev/null 2>&1; systemctl daemon-reload
  systemctl reset-failed macpro-fand 2>/dev/null; idle_jobs
  mkdir -p /run/macpro-fand; if [ -n "$OVSAVE" ]; then echo "$OVSAVE" > $OV; else rm -f $OV; fi
  systemctl start macpro-fand; wait_active macpro-fand active 60; waitcaps 30
  cp -p "$T2CSAVE" $T2C; rm -f "$T2CSAVE"
  [ "$T2EN" = enabled ] || systemctl disable t2fanrd >/dev/null 2>&1
  echo "restored: macpro-fand $(systemctl is-active macpro-fand), t2fanrd $(systemctl is-active t2fanrd)/$(systemctl is-enabled t2fanrd 2>/dev/null), override '$(cat $OV 2>/dev/null)', fans $(outs)"
  echo "end: $pass passed, $fail failed"
  [ $fail = 0 ]
}
trap 'restore' EXIT
trap 'echo interrupted; fail=$((fail + 1)); exit 130' INT TERM
sed -i 's/always_full_speed=false/always_full_speed=true/' $T2C
mkdir -p /run/macpro-fand; echo "max gpu=125" > $OV
systemctl is-active --quiet macpro-fand || systemctl start macpro-fand; wait_active macpro-fand active 60; sleep 3

# F1 installed state: macpro-fand active, t2fanrd inactive and disabled, its drop-in loaded, no Conflicts= left
systemctl is-active --quiet macpro-fand && ! systemctl is-active --quiet t2fanrd && [ "$(systemctl is-enabled t2fanrd 2>/dev/null)" != enabled ] \
  && systemctl cat t2fanrd | grep -q 't2fanrd-guard may-start' && ! systemctl show -p Conflicts --value macpro-fand | grep -q t2fanrd \
  && [ "$(systemctl show -p OnFailure --value macpro-fand)" = macpro-fand-backup.service ] \
  && ok "F1 installed: macpro-fand owner, t2fanrd off at boot, drop-in loaded, no Conflicts" || bad "F1: fand $(systemctl is-active macpro-fand) t2 $(systemctl is-active t2fanrd)/$(systemctl is-enabled t2fanrd) conflicts '$(systemctl show -p Conflicts --value macpro-fand)'"

# F2 macpro-fand wins: a t2fanrd start beside it is refused (exec-condition) and macpro-fand keeps running
pid=$(systemctl show -p MainPID --value macpro-fand)
since=$(date '+%Y-%m-%d %H:%M:%S'); systemctl start t2fanrd; sleep 3
journalctl --since "$since" -o cat --no-pager | grep -q 't2fanrd not started: macpro-fand.service (active' && ! systemctl is-active --quiet t2fanrd \
  && systemctl is-active --quiet macpro-fand && [ "$(systemctl show -p MainPID --value macpro-fand)" = "$pid" ] && atcaps \
  && ok "F2 t2fanrd start beside macpro-fand refused; macpro-fand untouched (pid $pid)" || bad "F2: t2 result $(systemctl show -p Result --value t2fanrd) fand $(systemctl is-active macpro-fand) pid $(systemctl show -p MainPID --value macpro-fand) fans $(outs)"

# F3 the restart window: kill macpro-fand, start t2fanrd during the 2 s auto-restart wait -> refused, macpro-fand back
systemctl kill -s KILL macpro-fand; sleep 0.7
state=$(systemctl show -p ActiveState -p SubState --value macpro-fand | tr '\n' ' ')
systemctl start t2fanrd; sleep 1; t2=$(systemctl is-active t2fanrd)
wait_active macpro-fand active 60 && [ "$t2" != active ] && ! systemctl is-active --quiet t2fanrd && waitcaps 30 \
  && ok "F3 restart window ($state): t2fanrd refused, macpro-fand back" || bad "F3: window '$state' t2 $t2 fand $(systemctl is-active macpro-fand) fans $(outs)"

# F4 clean stop hands the fans to t2fanrd; F5 starting macpro-fand takes them back after t2fanrd has fully exited
systemctl stop macpro-fand; wait_active t2fanrd active 30 && ok "F4 clean stop -> t2fanrd" || bad "F4: t2fanrd $(systemctl is-active t2fanrd)"
systemctl start macpro-fand; wait_active macpro-fand active 60 && ! systemctl is-active --quiet t2fanrd && waitcaps 30 \
  && ok "F5 start stops t2fanrd first; macpro-fand in charge" || bad "F5: fand $(systemctl is-active macpro-fand) t2 $(systemctl is-active t2fanrd) fans $(outs)"

# F6 permanent failure: OnFailure -> macpro-fand-backup -> full speed, then t2fanrd; the next start takes the fans back
mkdir -p ${DROP%/*}; printf '[Service]\nRestart=no\n' > $DROP; systemctl daemon-reload
systemctl kill -s KILL macpro-fand
wait_active t2fanrd active 30 && [ "$(systemctl is-active macpro-fand)" = failed ] && waitcaps 10 \
  && journalctl -u macpro-fand-backup --since -60s -o cat --no-pager | grep -q 't2fanrd (backup governor) started' \
  && ok "F6 permanent failure -> backup: full speed, t2fanrd started" || bad "F6: fand $(systemctl is-active macpro-fand) t2 $(systemctl is-active t2fanrd) fans $(outs) backup '$(journalctl -u macpro-fand-backup --since -60s -o cat --no-pager | tail -1)'"
rm -f $DROP; systemctl daemon-reload; systemctl reset-failed macpro-fand; idle_jobs
systemctl start macpro-fand; wait_active macpro-fand active 60 && ! systemctl is-active --quiet t2fanrd \
  && ok "F6b next start takes the fans back from t2fanrd" || bad "F6b: fand $(systemctl is-active macpro-fand) t2 $(systemctl is-active t2fanrd)"

# F7 t2fanrd stopping with no macpro-fand -> every fan at full speed (its curve would otherwise be left in place)
systemctl stop macpro-fand; wait_active t2fanrd active 30
sed -i 's/always_full_speed=true/always_full_speed=false/' $T2C; systemctl restart t2fanrd; wait_active t2fanrd active 30; sleep 4
lowered=$(outs)
sed -i 's/always_full_speed=false/always_full_speed=true/' $T2C
systemctl stop t2fanrd
waitcaps 10 && ok "F7 t2fanrd stop with no owner -> full speed (curve was $lowered)" || bad "F7: $(outs) manual $(mans) (curve was $lowered)"
systemctl start macpro-fand; wait_active macpro-fand active 60

# F8 no backup installed (t2fanrd masked): permanent failure -> full speed, t2fanrd not started
mkdir -p ${DROP%/*}; printf '[Service]\nRestart=no\n' > $DROP; systemctl daemon-reload
systemctl mask --runtime t2fanrd.service >/dev/null 2>&1
systemctl kill -s KILL macpro-fand; sleep 8; idle_jobs
[ "$(systemctl is-active macpro-fand)" = failed ] && ! systemctl is-active --quiet t2fanrd && waitcaps 10 \
  && journalctl -u macpro-fand-backup --since -30s -o cat --no-pager | grep -q 'no t2fanrd backup installed' \
  && ok "F8 no t2fanrd: permanent failure -> full speed, nothing else" || bad "F8: fand $(systemctl is-active macpro-fand) t2 $(systemctl is-active t2fanrd) fans $(outs)"
