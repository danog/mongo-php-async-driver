#!/bin/bash
# Runs the official driver's whole PHPT suite (upstream/driver/tests) against one extension build.
#   run-phpt.sh <extension file> <MONGODB_URI> <JUnit output>
# Uses php-src's run-tests.php of the running PHP's branch (fetched), one test at a time as the
# official CI does (tests share server state such as the key vault).
set -euo pipefail
ext=$1; uri=$2; junit=$3
php=${PHP:-php}
ver=$($php -n -r 'echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION;')
rt=$(mktemp -d)/run-tests.php
curl -fsSL --retry 3 "https://raw.githubusercontent.com/php/php-src/PHP-$ver/run-tests.php" -o "$rt"
export MONGODB_URI=$uri NO_INTERACTION=1 REPORT_EXIT_STATUS=0 TEST_PHP_EXECUTABLE=$(command -v "$php") TEST_PHP_JUNIT=$junit
# -n: only this extension is loaded (no ext-mongodb from the runner's php.ini)
$php "$rt" -n -d "extension=$ext" -d date.timezone=UTC -j1 -q --no-color --set-timeout 300 \
  --show-diff -g FAIL,BORK,LEAK,WARN ${PHPT_TESTS:-upstream/driver/tests} || true
