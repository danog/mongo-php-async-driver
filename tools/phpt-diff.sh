#!/bin/bash
# Compares two phpt.sh result files: per-directory counts and the tests whose status differs.
#   phpt-diff.sh <reference.txt> <ours.txt>
join -1 2 -2 2 -a 1 -a 2 -e MISSING -o 0,1.1,2.1 <(sort -k2 "$1") <(sort -k2 "$2") > /tmp/phpt-join.$$
echo "== per directory (reference PASS/FAIL/SKIP | ours PASS/FAIL/SKIP)"
awk '{split($1,p,"/"); d=p[3]; r[d","$2]++; o[d","$3]++; ds[d]=1}
  END {for (d in ds) printf "%-28s %5d %5d %5d | %5d %5d %5d\n", d, r[d",PASS"], r[d",FAIL"], r[d",SKIP"], o[d",PASS"], o[d",FAIL"], o[d",SKIP"]}' /tmp/phpt-join.$$ | sort
echo "== status differences"
awk '$2 != $3 {print $2 " -> " $3 "  " $1}' /tmp/phpt-join.$$
rm -f /tmp/phpt-join.$$
