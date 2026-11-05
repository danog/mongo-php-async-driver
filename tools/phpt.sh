#!/bin/bash
# Runs the carried-over PHPT suite (tests/phpt) against one extension build.
#   phpt.sh <label> <extension .so or "mongodb"> <MONGODB_URI or ""> [test dirs/files...]
# Results: $OUT/<label>.txt (one "STATUS path" line per test) and run-tests output in $OUT/<label>.log.
# Env: PHP (default php8.4), OUT (default /zf/mdrop/results), JOBS (default 16).
set -e
label=$1; ext=$2; uri=$3; shift 3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PHP=${PHP:-php8.4}
OUT=${OUT:-/zf/mdrop/results}
mkdir -p "$OUT"
RT=$(dirname "$($PHP -r 'echo PHP_BINARY;')")/../lib/php/$($PHP -r 'echo PHP_EXTENSION_DIR;' | xargs basename)/build/run-tests.php
[ -f "$RT" ] || RT=$(ls /usr/lib/php/*/build/run-tests.php | head -1)
targets=("$@"); [ ${#targets[@]} = 0 ] && targets=("$ROOT/tests/phpt")
export MONGODB_URI=$uri NO_INTERACTION=1 REPORT_EXIT_STATUS=0 TEST_PHP_EXECUTABLE=$(command -v $PHP)
$PHP "$RT" -n -d extension=$ext -d date.timezone=UTC -j${JOBS:-16} -q --no-color --set-timeout 120 \
  "${targets[@]}" > "$OUT/$label.log" 2>&1 || true
grep -E '^(PASS|FAIL|SKIP|BORK|XFAIL|LEAK|WARN)' "$OUT/$label.log" \
  | sed -E 's/^(PASS|FAIL|SKIP|BORK|XFAIL|LEAK|WARN)[^[]*\[([^]]*)\].*/\1 \2/' | sed "s#$ROOT/##" | sort -k2 > "$OUT/$label.txt"
awk '{print $1}' "$OUT/$label.txt" | sort | uniq -c | tr '\n' ' '; echo
