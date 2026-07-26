#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# A/B the timestamp cost benchmark on a REMOTE Mac — dogfood-mac (M4 Pro) by default.
# Same /tmp-confined rig as run-remote-m4.sh; see that script's header for the
# bootstrap it needs (loader, SPIRV-Tools, headers).
#
#   ./tsbench-remote.sh [host] [iters] [baseline.dylib]
#
# With a baseline dylib it runs both and prints them together, which is the only
# way to read these numbers: the absolute values are dominated by submit
# overhead, the DIFFERENCE is the timestamp machinery.
set -euo pipefail
cd "$(dirname "$0")"
HOST="${1:-dogfood-mac}"
ITERS="${2:-3000}"
BASE="${3:-}"
DYLIB="${KK_DYLIB:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib}"

scp -q "$DYLIB" "$HOST:/tmp/libkk_bench_new.dylib"
scp -q tsbench.c "$HOST:/tmp/tsbench.c"
[ -n "$BASE" ] && scp -q "$BASE" "$HOST:/tmp/libkk_bench_base.dylib"

ssh "$HOST" "cat > /tmp/tsbench-run.sh" <<'REMOTE'
#!/bin/bash
set -euo pipefail
for v in base new; do
  lib=/tmp/libkk_bench_${v}.dylib
  [ -f "$lib" ] || continue
  install_name_tool -change /opt/homebrew/opt/spirv-tools/lib/libSPIRV-Tools.dylib \
    /tmp/libSPIRV-Tools.dylib "$lib" 2>/dev/null || true
  codesign -f -s - "$lib" >/dev/null 2>&1 || true
  printf '%s' "{\"file_format_version\":\"1.0.0\",\"ICD\":{\"library_path\":\"$lib\",\"api_version\":\"1.3.0\"}}" \
    > /tmp/icd_bench_${v}.json
done
cc -g -O2 -o /tmp/tsbench /tmp/tsbench.c -I/tmp/kkinc /tmp/libvulkan.1.dylib -Wl,-rpath,/tmp
install_name_tool -change /opt/homebrew/opt/vulkan-loader/lib/libvulkan.1.dylib \
  /tmp/libvulkan.1.dylib /tmp/tsbench
codesign -f -s - /tmp/tsbench >/dev/null 2>&1 || true
for r in 1 2; do
  echo "--- run $r ---"
  for v in base new; do
    [ -f /tmp/icd_bench_${v}.json ] || continue
    echo "[$v]"
    VK_ICD_FILENAMES=/tmp/icd_bench_${v}.json VK_DRIVER_FILES=/tmp/icd_bench_${v}.json \
      /tmp/tsbench ITERS | tail -3
  done
done
REMOTE
ssh "$HOST" "sed -i '' 's/tsbench ITERS/tsbench $ITERS/' /tmp/tsbench-run.sh && bash /tmp/tsbench-run.sh"
