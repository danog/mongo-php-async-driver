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
