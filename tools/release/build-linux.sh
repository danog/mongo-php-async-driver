#!/bin/sh
# Builds the extension inside an official php:<ver>-cli image (Debian for glibc, Alpine for musl),
# so that the binary needs only that image's glibc (2.36, bookworm) or musl.
#   build-linux.sh <output .so>
set -eu
out=$1
if command -v apk >/dev/null; then
  apk add --no-cache -q build-base clang-dev curl git
  # a cdylib on musl must link the C runtime dynamically
  export RUSTFLAGS="-C target-feature=-crt-static"
else
  apt-get update -qq && apt-get install -y -qq --no-install-recommends build-essential clang libclang-dev curl git ca-certificates >/dev/null
fi
curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
PHP=$(command -v php) PHP_CONFIG=$(command -v php-config) cargo build --release --locked
cp target/release/libmongodb.so "$out"
php -n -d "extension=$PWD/$out" -r 'echo "mongodb ", phpversion("mongodb"), "\n";'
# the image is the expected build (NTS or ZTS: EXPECT_ZTS=0/1; PHP_ZTS is a bool since PHP 8.4)
test "$(php -n -r 'echo (int) PHP_ZTS;')" = "${EXPECT_ZTS:-0}"
