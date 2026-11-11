#!/bin/bash
# Runs the carried-over PHPLIB 2.5.1 PHPUnit suite (tests/phplib) against one extension build.
#   phplib.sh <label> <extension .so> <MONGODB_URI> [phpunit args...]
# The library sources and dev dependencies are installed once into $WORK (composer, mongodb/mongodb
# 2.5.1, from git so that examples/ is there); tests/phplib is then synced over it, so the carried-over tests are the ones that run.
# The PHP's own conf.d is used with any mongodb (and xdebug) extension line commented out, so only
# <extension .so> is loaded.
# Results: $OUT/<label>.xml (JUnit), $OUT/<label>.log; prints the summary line.
# Env: PHP (default php8.4), OUT (default /zf/mdrop/results), WORK (default /zf/mdrop/phplib).
set -e
label=$1; ext=$2; uri=$3; shift 3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PHP=${PHP:-php8.4}
OUT=${OUT:-/zf/mdrop/results}
WORK=${WORK:-/zf/mdrop/phplib}
VERSION=2.5.1
mkdir -p "$OUT" "$WORK"
LIB=$WORK/mongodb-$VERSION
if [ ! -f "$LIB/.installed" ]; then
  rm -rf "$LIB"
  composer create-project --prefer-source --no-install --no-scripts --no-interaction --ignore-platform-req=ext-mongodb "mongodb/mongodb:$VERSION" "$LIB" > "$WORK/composer.log" 2>&1
  cp "$ROOT/tests/phplib/composer.json" "$LIB/composer.json"
  (cd "$LIB" && COMPOSER_ROOT_VERSION=$VERSION composer install --no-scripts --no-interaction --no-progress --ignore-platform-req=ext-mongodb >> "$WORK/composer.log" 2>&1)
  touch "$LIB/.installed"
fi
rsync -a --delete --exclude specifications "$ROOT/tests/phplib/tests/" "$LIB/tests/"
rm -rf "$LIB/tests/specifications" && ln -s "$ROOT/tests/spec" "$LIB/tests/specifications"
cp "$ROOT/tests/phplib/phpunit.xml.dist" "$LIB/phpunit.xml.dist"
# conf.d of this PHP without mongodb/xdebug
ver=$($PHP -r 'echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION;')
scan=$($PHP -r 'echo PHP_CONFIG_FILE_SCAN_DIR;')
INI=$WORK/conf.d-$ver
rm -rf "$INI"; mkdir -p "$INI"
for f in "$scan"/*.ini; do
  [ -f "$f" ] || continue  # none (an empty or missing conf.d)
  sed -E 's/^[[:space:]]*(zend_)?extension[[:space:]]*=[[:space:]]*"?[^"]*(mongodb|xdebug)[^"]*"?[[:space:]]*$/; &/' "$f" > "$INI/$(basename "$f")"
done
# an ini file rather than -d, so that PHP subprocesses spawned by tests (examples, GridFS) load it too;
# zend.assertions can only be enabled at startup (phpunit.xml.dist asks for 1)
printf 'extension=%s\nmemory_limit=-1\nzend.assertions=1\n' "$ext" > "$INI/99-mongodb-under-test.ini"
cd "$LIB"
rm -f "$OUT/$label.xml"
MONGODB_URI=$uri PHP_INI_SCAN_DIR=$INI $PHP vendor/bin/phpunit \
  --log-junit "$OUT/$label.xml" "$@" > "$OUT/$label.log" 2>&1 || true
summary=$(grep -E "^(OK|Tests:)" "$OUT/$label.log" | tail -1)
echo "${summary:-no PHPUnit summary (run aborted): see $OUT/$label.log}"
