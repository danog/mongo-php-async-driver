#!/bin/bash
# Runs the official library's whole PHPUnit suite (upstream/library) against one extension build.
#   run-phplib.sh <extension file> <MONGODB_URI> <JUnit output>
set -euo pipefail
ext=$1; uri=$2; junit=$3
php=${PHP:-php}
cd upstream/library
# composer runs without the extension: its version constraint is the official extension's
COMPOSER_ROOT_VERSION=dev-ci composer install --no-interaction --no-progress --ignore-platform-req=ext-mongodb
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
export PHP_INI_SCAN_DIR="$sep$ini"
$php -r 'exit(extension_loaded("mongodb") ? 0 : 1);' || { echo "the extension did not load from $ini" >&2; exit 1; }
$php -d date.timezone=UTC -d memory_limit=-1 vendor/bin/phpunit --log-junit "$junit" ${PHPUNIT_ARGS:-} || true
