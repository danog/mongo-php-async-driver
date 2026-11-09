#!/bin/bash
# Runs the official library's whole PHPUnit suite (upstream/library) against one extension build.
#   run-phplib.sh <extension file> <MONGODB_URI> <JUnit output>
set -euo pipefail
ext=$1; uri=$2; junit=$3
php=${PHP:-php}
# paths as PHP sees them (Windows: C:/... instead of the shell's /c/..., in the long form: a "~" of
# an 8.3 short name breaks unquoted INI values)
win() { if [ "${OS:-}" = Windows_NT ]; then cygpath -ml "$1"; else printf '%s' "$1"; fi; }
cd upstream/library
# composer runs without the extension: its version constraint is the official extension's. The
# PHP constraint is ignored too (as the official library's CI does for new PHP versions): its dev
# tools (e.g. psalm) may not allow a PHP version this extension already supports
COMPOSER_ROOT_VERSION=dev-ci composer install --no-interaction --no-progress --ignore-platform-req=ext-mongodb --ignore-platform-req=php+
export MONGODB_URI=$uri
# the runner's php.ini stays (PHPUnit needs dom, mbstring, ... which are shared on Linux), but it
# must not load ext-mongodb itself (the workflow disables it in setup-php)
if $php -r 'exit(extension_loaded("mongodb") ? 0 : 1);'; then
  echo "ext-mongodb is already loaded by the runner's php.ini" >&2
  exit 1
fi
# loaded from an extra ini directory, so that the PHP processes the tests start load it too
ini=$(mktemp -d)
printf 'extension="%s"\n' "$ext" > "$ini/zz-mongodb.ini"
sep=:; [ "${OS:-}" = Windows_NT ] && sep=';'
export PHP_INI_SCAN_DIR="$sep$(win "$ini")"
$php -r 'exit(extension_loaded("mongodb") ? 0 : 1);' || { echo "the extension did not load from $ini" >&2; exit 1; }
$php -d date.timezone=UTC -d memory_limit=-1 vendor/bin/phpunit --log-junit "$(win "$junit")" ${PHPUNIT_ARGS:-} || true
