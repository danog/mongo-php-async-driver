#!/bin/bash
# Runs the official driver's whole PHPT suite (upstream/driver/tests) against one extension build.
#   run-phpt.sh <extension file> <MONGODB_URI> <JUnit output>
# Env: PHPT_TESTS (the tests to run instead), PHPT_EXTRA (more run-tests.php arguments, e.g.
# "-d extension=<file>" for a second extension, loaded after).
# Uses php-src's run-tests.php of the running PHP's branch (fetched), one test at a time as the
# official CI does (tests share server state such as the key vault).
set -euo pipefail
ext=$1; uri=$2; junit=$3
php=${PHP:-php}
# paths as PHP sees them (Windows: C:/... instead of the shell's /c/..., in the long form: a "~" of
# an 8.3 short name breaks unquoted INI values)
win() { if [ "${OS:-}" = Windows_NT ]; then cygpath -ml "$1"; else printf '%s' "$1"; fi; }
ver=$($php -n -r 'echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION;')
rt=$(mktemp -d)/run-tests.php
curl -fsSL --retry 3 "https://raw.githubusercontent.com/php/php-src/PHP-$ver/run-tests.php" -o "$rt"
export MONGODB_URI=$uri NO_INTERACTION=1 REPORT_EXIT_STATUS=0 TEST_PHP_EXECUTABLE=$($php -n -r 'echo PHP_BINARY;') TEST_PHP_JUNIT=$(win "$junit")
# -n: only this extension is loaded (no ext-mongodb from the runner's php.ini). An empty
# extension_dir: run-tests.php otherwise dl()s every extension it finds there in one process to
# list them, and the runner's set of PECL extensions crashes PHP that way even without this one
# (no upstream test declares --EXTENSIONS--)
noext=$(win "$(mktemp -d)")
$php "$(win "$rt")" -n -d "extension=$ext" -d "extension_dir=$noext" -d date.timezone=UTC ${PHPT_EXTRA:-} -j1 -q --no-color --set-timeout 300 \
  --show-diff -g FAIL,BORK,LEAK,WARN ${PHPT_TESTS:-upstream/driver/tests} || true
