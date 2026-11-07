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
$php -n -d "extension=$ext" -d date.timezone=UTC -d memory_limit=-1 vendor/bin/phpunit --log-junit "$junit" ${PHPUNIT_ARGS:-} || true
