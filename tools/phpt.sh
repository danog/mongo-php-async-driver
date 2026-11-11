#!/bin/bash
# Runs the carried-over PHPT suite (tests/phpt) against one extension build.
#   phpt.sh <label> <extension .so or "mongodb"> <MONGODB_URI or ""> [test dirs/files...]
# Results: $OUT/<label>.txt (one "STATUS path" line per test) and run-tests output in $OUT/<label>.log.
# Env: PHP (default php8.4), OUT (default /zf/mdrop/results), JOBS (default 16), RT (run-tests.php), TIMEOUT
#      (seconds per test, default 120; a hung test process is killed shortly after),
#      EXTRA (more run-tests.php arguments, e.g. "-m" for leak checks).
set -e
label=$1; ext=$2; uri=$3; shift 3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PHP=${PHP:-php8.4}
OUT=${OUT:-/zf/mdrop/results}
mkdir -p "$OUT"
# run-tests.php of that PHP: Debian (lib/php/<api>/build) or Homebrew (lib/php/build); RT overrides
PHPDIR=$(cd "$(dirname "$($PHP -n -r 'echo PHP_BINARY;')")/.." && pwd -P)
for c in "$RT" "$PHPDIR/lib/php/$($PHP -n -r 'echo PHP_EXTENSION_DIR;' | xargs basename)/build/run-tests.php" "$PHPDIR/lib/php/build/run-tests.php"; do
  [ -n "$c" ] && [ -f "$c" ] && { RT=$c; break; }
done
[ -f "$RT" ] || RT=$(ls /usr/lib/php/*/build/run-tests.php | head -1)
targets=("$@"); [ ${#targets[@]} = 0 ] && targets=("$ROOT/tests/phpt")
TIMEOUT=${TIMEOUT:-120}
# run-tests' timeout kills only the shell it started a test through: a hung test's PHP would
# outlive the run. Each PHP it starts gets SIGALRM (terminates) a little after the timeout.
mkdir -p "$OUT/bin"
cat > "$OUT/bin/php-$label" <<EOF
#!/bin/sh
exec perl -e 'alarm shift; exec { \$ARGV[0] } @ARGV or die "exec: \$!\n"' $((TIMEOUT + 5)) "$(command -v $PHP)" "\$@"
EOF
chmod +x "$OUT/bin/php-$label"
export MONGODB_URI=$uri NO_INTERACTION=1 REPORT_EXIT_STATUS=0 TEST_PHP_EXECUTABLE=$OUT/bin/php-$label
$PHP "$RT" -n -d extension=$ext -d date.timezone=UTC -j${JOBS:-16} -q --no-color --set-timeout $TIMEOUT $EXTRA \
  "${targets[@]}" > "$OUT/$label.log" 2>&1 || true
# with -q, progress text precedes each result on the same line: match results anywhere
grep -oE '(PASS|FAIL|SKIP|BORK|XFAIL|LEAK|WARN) .*?\[[^]]*\.phpt\]' "$OUT/$label.log" \
  | sed -E 's/^(PASS|FAIL|SKIP|BORK|XFAIL|LEAK|WARN) .*\[([^]]*\.phpt)\]$/\1 \2/' | sed "s#$ROOT/##" | sort -u -k2 > "$OUT/$label.txt"
awk '{print $1}' "$OUT/$label.txt" | sort | uniq -c | tr '\n' ' '; echo
