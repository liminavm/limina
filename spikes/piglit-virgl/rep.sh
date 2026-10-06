#!/bin/bash
# rep.sh <list> <n> — run each listed piglit test n times under the default (virgl) driver
cd ~/piglit
export PIGLIT_PLATFORM=gbm
while IFS=$'\t' read -r name cmd; do
  res=()
  for i in $(seq "$2"); do
    out=$(timeout 120 bash -c "bin/$cmd -auto -fbo" 2>&1 < /dev/null)
    rc=$?
    r=$(echo "$out" | grep -o '"result": "[a-z]*"' | tail -1 | cut -d'"' -f4)
    [ $rc = 124 ] && r=timeout
    [ -z "$r" ] && r="rc$rc"
    echo "$out" | grep -q "slow gpu" && r="$r(stall)"
    res+=("$r")
  done
  printf '%s\t%s\n' "${res[*]}" "$name"
done < "$1"
