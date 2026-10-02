#!/bin/bash
R=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
H=$R/spikes/console-redraw-repro
D=${CONREDRAW_DIR:-$H/work.noindex}
for n in $(seq $1 $2); do
  $H/iter.sh $n ${SECS:-35} > /dev/null
  v=$(python3 $H/judge.py $D/i$n)
  echo "i$n $v"
  case "$v" in STUCK*|NOFRAMES*) echo "== stuck at i$n"; exit 0;; esac
done
echo "== no repro in $1..$2"
