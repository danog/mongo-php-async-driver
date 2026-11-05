#!/bin/bash
# AddressSanitizer runs of the PHPT suite (memory-safety gate; valgrind does not run on macOS arm64).
#   asan.sh build                         nightly build with -Zsanitizer=address
#   asan.sh phpt <label> <uri|""> [dirs]  PHPT run of that build under ASan (results via tools/phpt.sh)
#   asan.sh php [args]                    one php process under ASan
# Env: PHP / PHP_CONFIG (the PHP to build for and run; required), CARGO_TARGET_DIR, LIBCLANG_PATH, OUT, JOBS.
#
# How: the extension is instrumented, PHP is not; ASan's runtime must be in the process from the
# start, so it is preloaded (DYLD_INSERT_LIBRARIES on macOS, LD_PRELOAD on Linux), and
# USE_ZEND_ALLOC=0 sends emalloc/efree to the (intercepted) system allocator, so ASan sees every
# Zend allocation too. macOS drops DYLD_* when it starts the system shell that run-tests.php uses,
# so run-tests gets a wrapper script that sets the variables itself and execs PHP.
# Reported: invalid reads/writes, use-after-free, double free (the process stops at the first one;
# reports land in $OUT/asan-<label>/asan.*).
# LEAKS=1 also runs LeakSanitizer and lists the leaks whose allocation stack goes through this
# extension outside MINIT and fatal-error bailouts (neither is a request leak). It works
# on uninstrumented code too, but the C reference crashes at exit under LSan on macOS (libmongoc's
# logging), so there the comparison is "no leak of ours"; PHP's own leak report needs a debug PHP.
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
: "${PHP:?set PHP}" "${PHP_CONFIG:?set PHP_CONFIG}"
HOST=$(rustc +nightly -vV | sed -n 's/^host: //p')
TD=${CARGO_TARGET_DIR:-$ROOT/target}
SO_DIR=$TD/$HOST/fast
case $(uname) in
  Darwin) SO=$SO_DIR/libmongodb.dylib; PRE=DYLD_INSERT_LIBRARIES
          RT=$(rustc +nightly --print sysroot)/lib/rustlib/$HOST/lib/librustc-nightly_rt.asan.dylib ;;
  *) SO=$SO_DIR/libmongodb.so; PRE=LD_PRELOAD
     RT=$(rustc +nightly --print sysroot)/lib/rustlib/$HOST/lib/librustc-nightly_rt.asan.so ;;
esac
OUT=${OUT:-$ROOT/target/asan-results}

wrapper() { # $1: dir for the wrapper and the reports
  mkdir -p "$1"
  cat > "$1/php" <<EOF
#!/bin/bash
export $PRE=$RT USE_ZEND_ALLOC=0 ZEND_DONT_UNLOAD_MODULES=1
export ASAN_OPTIONS=detect_leaks=${LEAKS:-0}:abort_on_error=0:halt_on_error=1:log_path=$1/asan
exec $PHP "\$@"
EOF
  chmod +x "$1/php"
}

case $1 in
  build)
    # --target: build scripts and proc macros stay uninstrumented
    cd "$ROOT" && RUSTFLAGS="-Zsanitizer=address" cargo +nightly build --profile fast --target "$HOST"
    echo "$SO" ;;
  phpt)
    label=$2; uri=$3; shift 3
    W=$OUT/asan-$label; rm -rf "$W"; wrapper "$W"
    PHP=$W/php OUT=$OUT "$ROOT/tools/phpt.sh" "$label" "$SO" "$uri" "$@"
    rc=0
    if grep -l "ERROR: AddressSanitizer" "$W"/asan.* >/dev/null 2>&1; then
      echo "ASan reports:"; grep -h "ERROR: AddressSanitizer" "$W"/asan.* | sort | uniq -c
      rc=1
    else
      echo "ASan: no reports"
    fi
    if [ "${LEAKS:-0}" = 1 ]; then
      python3 - "$W" <<'PY' || rc=1
import glob, re, sys
ours = []
for f in glob.glob(sys.argv[1] + '/asan.*'):
    log = open(f, errors='replace').read()
    forked = 'atfork_child' in log
    for block in log.split('\n\n'):
        if 'leak of' not in block:
            continue
        frames = [l for l in block.splitlines() if re.match(r'\s+#\d+ ', l)]
        mine = [l for l in frames if '7mongodb' in l or 'mongo_core' in l]
        # not request leaks: MINIT allocations (process lifetime); a fatal error (zend_error_noreturn)
        # bails out past the message it formatted, as with the C extension; the system resolver's
        # own caches (getaddrinfo); in a fork() child, the connections (and their buffers) inherited
        # from the parent, forgotten on purpose (check_fork: never closed or written to)
        skip = ['5minit', 'register_all', 'zend_error_noreturn', 'getaddrinfo'] + (['4Conn'] if forked else [])
        if mine and not any(k in l for l in frames for k in skip):
            ours.append(block.splitlines()[0] + ' | ' + mine[0].split(' in ', 1)[-1])
print('LSan: %d leak(s) through this extension' % len(ours))
for l in sorted(set(ours))[:20]:
    print('  ' + l)
sys.exit(1 if ours else 0)
PY
    fi
    exit $rc ;;
  php)
    shift; W=$OUT/asan-php; rm -rf "$W"; wrapper "$W"
    "$W/php" -n -d extension="$SO" "$@"
    ls "$W"/asan.* 2>/dev/null && cat "$W"/asan.* ;;
  *) sed -n '2,5p' "$0"; exit 1 ;;
esac
