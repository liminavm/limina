#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# CTS A/B for the LIMINA_KK_EARLYZ knob (drop KK's injected FS depth-write +
# helper-quad sample-mask write, restoring early-Z/HSR).
#
# Runs the early-Z-sensitive dEQP-VK groups twice on host KK — baseline
# (knob off) vs LIMINA_KK_EARLYZ=1 — and diffs the per-test results. Knob-on
# regressions identify exactly which semantics the blanket late-Z hammer
# protects, so it can be replaced by a narrow condition (e.g. only shaders
# with side effects / discard / explicit sample-mask).
#
# Needs: third_party/VK-GL-CTS built (deqp-vk), deqp-runner (cargo install),
# /tmp/earlyz-cases.txt caselist (see RESULTS.md round-18 for the grep), and
# a quiesced GPU (stop the VM first — KK CTS and the guest contend).
#
# Usage: ./cts-earlyz-ab.sh [baseline|earlyz|diff]   (no arg = all three)
set -eu

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
DEQP="$REPO/third_party/VK-GL-CTS/build/external/vulkancts/modules/vulkan/deqp-vk"
ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
CASELIST=${CASELIST:-/tmp/earlyz-cases.txt}
OUT=${OUT:-/tmp/cts-earlyz}
RUNNER=${RUNNER:-$HOME/.cargo/bin/deqp-runner}

export DYLD_LIBRARY_PATH=/opt/homebrew/lib
export VK_ICD_FILENAMES=$ICD

run_leg() { # $1 = leg name
  rm -rf "$OUT/$1"
  "$RUNNER" run \
    --deqp "$DEQP" \
    --caselist "$CASELIST" \
    --output "$OUT/$1" \
    --timeout 120 \
    -- --deqp-surface-type=fbo
}

leg=${1:-all}

if [ "$leg" = baseline ] || [ "$leg" = all ]; then
  unset LIMINA_KK_EARLYZ
  run_leg baseline
fi

if [ "$leg" = earlyz ] || [ "$leg" = all ]; then
  export LIMINA_KK_EARLYZ=1
  run_leg earlyz
fi

if [ "$leg" = diff ] || [ "$leg" = all ]; then
  # results.csv lines: testname,Status
  sort "$OUT/baseline/results.csv" > /tmp/cts-earlyz-base.csv
  sort "$OUT/earlyz/results.csv" > /tmp/cts-earlyz-on.csv
  echo "=== status changes (baseline -> earlyz) ==="
  join -t, /tmp/cts-earlyz-base.csv /tmp/cts-earlyz-on.csv -o 0,1.2,2.2 \
    | awk -F, '$2 != $3 { print }'
  echo "=== summary ==="
  for l in baseline earlyz; do
    printf '%-9s ' "$l"
    awk -F, '{ n[$2]++ } END { for (s in n) printf "%s=%d ", s, n[s]; print "" }' \
      "$OUT/$l/results.csv"
  done
fi
