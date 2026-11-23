#!/bin/bash
# Writes, as Markdown, what failed in a run of the Tests workflow (for autoupdate.yml's fix job):
# for each matrix job, the unexpected failures of its JUnit results as tools/ci/junit-summary.py
# reports them (expected-failures.txt applied with the job's OS, PHP version and setup); then, for
# every failed job, the end of its log up to its last error (a build, a setup or a non-PHPT job).
#   failures.sh <run id> <directory of the results-* artifacts> > failures.md
# Needs GH_TOKEN (actions: read) and GITHUB_REPOSITORY.
set -euo pipefail
run=$1; dir=$2
here=$(cd "$(dirname "$0")" && pwd)

echo "# Failures of run $run"
echo
echo "## Test results"
for a in "$dir"/results-*; do
  [ -d "$a" ] || continue
  name=$(basename "$a")
  # results-<os>-php<X.Y>-<nts|zts>-<mongodb>-<topology>[-<setup>]
  if ! [[ $name =~ ^results-(ubuntu-[0-9.]+|macos-[0-9]+|windows-[0-9]+)-php([0-9.]+)-(nts|zts)-(.+)-(server|replica_set|sharded_cluster)(-(.+))?$ ]]; then
    echo "- unrecognised artifact \`$name\`"; continue
  fi
  case ${BASH_REMATCH[1]} in ubuntu-*) os=Linux ;; macos-*) os=macOS ;; *) os=Windows ;; esac
  php=${BASH_REMATCH[2]} setup=${BASH_REMATCH[7]}
  suites=()
  for f in "$a"/*.xml; do [ -f "$f" ] && suites+=("$(basename "$f" .xml)" "$f"); done
  [ ${#suites[@]} = 0 ] && { echo; echo "### $name"; echo; echo "no results (the job failed before its tests)"; continue; }
  if ! out=$(env -u GITHUB_STEP_SUMMARY RUNNER_OS=$os PHP_VERSION_MINOR=$php CI_SETUP=$setup python3 "$here/junit-summary.py" "${suites[@]}"); then
    echo; echo "### $name"; echo; echo "$out"
  fi
done

echo
echo "## Failed jobs"
gh api --paginate "repos/$GITHUB_REPOSITORY/actions/runs/$run/jobs" \
  --jq '.jobs[] | select(.conclusion == "failure") | [.id, .name] | @tsv' |
while IFS=$'\t' read -r id job; do
  echo
  echo "### $job"
  echo
  echo '```'
  # the timestamps and colours off; the 200 lines before the last error
  gh api --allow-escape-sequences "repos/$GITHUB_REPOSITORY/actions/jobs/$id/logs" |
    sed -E 's/^[0-9T:.-]+Z //; s/\x1b\[[0-9;]*m//g' |
    awk '{ l[NR] = $0 } /##\[error\]/ { last = NR } END { if (!last) last = NR; for (i = (last > 200 ? last - 199 : 1); i <= last; i++) print l[i] }'
  echo '```'
done
