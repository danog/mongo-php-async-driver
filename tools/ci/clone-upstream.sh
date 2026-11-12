#!/bin/bash
# Clones the latest commit of the official driver and library (their default branch, resolved at
# run time) into $1 (default: upstream/), and prints the commits to $GITHUB_STEP_SUMMARY.
set -euo pipefail
dir=${1:-upstream}
mkdir -p "$dir"
clone() { # url dest [extra git args]
  local url=$1 dest=$2; shift 2
  rm -rf "$dest"
  # no CRLF conversion (Windows): tests compare source text with expected output byte for byte
  git -c core.autocrlf=false clone --quiet --depth 1 "$@" "$url" "$dest"
  local head branch
  head=$(git -C "$dest" rev-parse HEAD)
  branch=$(git -C "$dest" rev-parse --abbrev-ref HEAD)
  echo "$url $branch $head"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo "- \`$(basename "$url" .git)\` $branch @ [\`${head:0:10}\`]($url/commit/$head)" >> "$GITHUB_STEP_SUMMARY"
  fi
}
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && echo "### Upstream test sources" >> "$GITHUB_STEP_SUMMARY"
clone https://github.com/mongodb/mongo-php-driver "$dir/driver"
# the library's spec tests come from its submodule (mongodb/specifications)
clone https://github.com/mongodb/mongo-php-library "$dir/library" --recurse-submodules --shallow-submodules
# transactions/error-labels-blockConnection as mongodb/specifications fixed it (DRIVERS-3659,
# 94ab731), while the library's submodule predates the fix: the test configured its fail point
# through client0 (socketTimeoutMS=100), and the runner disables it through the same client. The
# server's configureFailPoint waits for the commitTransaction still blocked in the fail point
# (blockTimeMS=150), so the cleanup timed out ("Failed to read 4 bytes") whenever that wait
# passed 100 ms (on the macOS runners in 5 of 28 runs); the fixed test configures it
# through a client without timeout options. Applied only while the cloned file is the old one.
spec=94ab731194c348afedc49458b148bdc01c2c4115
f=$dir/library/tests/specifications/source/transactions/tests/unified/error-labels-blockConnection.json
if [ -f "$f" ] && ! grep -q failPointClient "$f"; then
  curl -fsSL --retry 3 -o "$f" "https://raw.githubusercontent.com/mongodb/specifications/$spec/source/transactions/tests/unified/error-labels-blockConnection.json"
  echo "patched $f (mongodb/specifications $spec)"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    echo "- \`transactions/error-labels-blockConnection\` from \`specifications\` @ [\`${spec:0:10}\`](https://github.com/mongodb/specifications/commit/$spec) (DRIVERS-3659)" >> "$GITHUB_STEP_SUMMARY"
  fi
fi
