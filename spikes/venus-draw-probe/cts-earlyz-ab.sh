#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# CTS A/B for the LIMINA_KK_EARLYZ knob (drop KK's injected FS depth-write +
# helper-quad sample-mask write, restoring early-Z/HSR).
#
# *** INERT since 2026-08-14: LIMINA_KK_EARLYZ was retired and early-Z is now
# *** unconditional in KK, so BOTH ARMS OF THIS A/B RUN THE SAME CODE. It would
# *** print a clean empty diff and that result would mean nothing — a check that
# *** passes by construction. It refuses to run rather than produce one.
# ***
# *** To use it again, reintroduce the gate in KK (kk_shader.c: the deleted
# *** msl_lower_static_sample_mask call for helper-invocation FS, and the
# *** msl_ensure_depth_write call) and delete this guard. The caselist and
# *** deqp-runner plumbing below are still correct and are why this is kept.
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

if [ -z "${EARLYZ_AB_GATE_RESTORED:-}" ]; then
    echo "cts-earlyz-ab.sh: INERT — LIMINA_KK_EARLYZ was retired 2026-08-14 and" >&2
    echo "  early-Z is unconditional in KK. Both arms would run identical code and" >&2
    echo "  the diff would be empty for that reason, not because nothing regressed." >&2
    echo "  Reintroduce the gate in kk_shader.c, then set EARLYZ_AB_GATE_RESTORED=1." >&2
    exit 2
fi

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
