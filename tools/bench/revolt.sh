#!/bin/bash
# The event loops compared (README, Performance): tools/bench/revolt.php in several configurations,
# alternating (the order rotates every round), many rounds; prints the medians per configuration.
#   revolt.sh <uri> <rounds> <new .so> [old .so]
# Env: PHP (default php), REVOLT_AUTOLOAD (revolt/event-loop's vendor/autoload.php: the PHP
# library configurations), EV (ev.so: the EvDriver configurations), MAIN_OPS (default 5000),
# FIBER_OPS (default 20000), CASES (default "ping findOne insertOne"), OUT (raw results, default
# revolt-bench.jsonl).
# Configurations ({main}: every one; fibers: those that have a loop):
#   old        [old .so] Revolt not loaded: its synchronous path
#   sync       mongodb.native_event_loop=0, no Revolt at all: the synchronous path
#   idle       the embedded loop registered but never used (EventLoop untouched): synchronous
#   native     the embedded loop in use
#   native-m0  the embedded loop in use, mongodb.main_event_loop=0
#   native-ev  the embedded loop in use, ext-ev loaded
#   lib        mongodb.native_event_loop=0 and revolt/event-loop (StreamSelectDriver)
#   lib-m0     the same, mongodb.main_event_loop=0
#   lib-ev     mongodb.native_event_loop=0 and revolt/event-loop with ext-ev (EvDriver)
set -e
uri=$1; rounds=$2; new=$3; old=${4:-}
PHP=${PHP:-php}
OUT=${OUT:-revolt-bench.jsonl}
DIR=$(cd "$(dirname "$0")" && pwd)
: > "$OUT"
args() { # config -> php arguments (and the <loop> argument last)
  case $1 in
    old) echo "-d extension=$old none" ;;
    sync) echo "-d extension=$new -d mongodb.native_event_loop=0 none" ;;
    idle) echo "-d extension=$new none" ;;
    native) echo "-d extension=$new used" ;;
    native-m0) echo "-d extension=$new -d mongodb.main_event_loop=0 used" ;;
    native-ev) echo "-d extension=$new -d extension=$EV used" ;;
    lib) echo "-d extension=$new -d mongodb.native_event_loop=0 used" ;;
    lib-m0) echo "-d extension=$new -d mongodb.native_event_loop=0 -d mongodb.main_event_loop=0 used" ;;
    lib-ev) echo "-d extension=$new -d mongodb.native_event_loop=0 -d extension=$EV used" ;;
  esac
}
main_cfgs=(sync idle native native-m0 lib lib-m0)
fiber_cfgs=(native lib)
[ -n "$old" ] && main_cfgs=(old "${main_cfgs[@]}")
[ -n "$EV" ] && { main_cfgs+=(native-ev lib-ev); fiber_cfgs+=(native-ev lib-ev); }
run() { # round config case fibers ops
  local a; a=($(args "$2"))
  local loop=${a[${#a[@]}-1]}; unset 'a[${#a[@]}-1]'
  local r; r=$(REVOLT_AUTOLOAD=$REVOLT_AUTOLOAD "$PHP" -n "${a[@]}" "$DIR/revolt.php" "$uri" "$loop" "$3" "$4" "$5")
  echo "{\"round\":$1,\"config\":\"$2\",\"case\":\"$3\",\"fibers\":$4,${r#\{}" >> "$OUT"
}
rotate() { # round, list... -> the list rotated by round
  local k=$1; shift; local n=$#; local l=("$@")
  for ((i = 0; i < n; i++)); do echo "${l[(i + k) % n]}"; done
}
for ((round = 0; round < rounds; round++)); do
  for c in ${CASES:-ping findOne insertOne}; do
    for cfg in $(rotate $round "${main_cfgs[@]}"); do run $round "$cfg" $c 0 "${MAIN_OPS:-5000}"; done
    for f in 1 10 50 200; do
      for cfg in $(rotate $round "${fiber_cfgs[@]}"); do run $round "$cfg" $c $f "${FIBER_OPS:-20000}"; done
    done
  done
  echo "round $((round + 1))/$rounds done" >&2
done
python3 - "$OUT" <<'PY'
import json, sys, statistics
rows = [json.loads(l) for l in open(sys.argv[1])]
groups = {}
for r in rows:
    groups.setdefault((r['fibers'], r['case'], r['config']), []).append(r)
print('%-8s %-10s %-10s %-18s %12s %12s' % ('fibers', 'case', 'config', 'driver', 'CPU ns/op', 'wall ns/op'))
for (f, c, cfg), rs in sorted(groups.items(), key=lambda kv: (kv[0][0], ['ping', 'findOne', 'insertOne'].index(kv[0][1]))):
    print('%-8s %-10s %-10s %-18s %12d %12d' % (f or '{main}', c, cfg, rs[0]['driver'], statistics.median(r['cpu_ns_per_op'] for r in rs), statistics.median(r['ns_per_op'] for r in rs)))
PY
