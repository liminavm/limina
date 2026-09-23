#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Build the probe and run every mode against a KosmicKrisp ICD (default: the host build limina
# uses). usage: run.sh [icd.json]
set -e
cd "$(dirname "$0")"
ICD="${1:-$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)}"
cc -o rp_attach_count rp_attach_count.c -I/opt/homebrew/include -L/opt/homebrew/lib -lvulkan
glslangValidator -q -V tri.vert -o tri.vert.spv >/dev/null
glslangValidator -q -V tri.frag -o tri.frag.spv >/dev/null
out=$(mktemp)
for m in ok drawok imageless fb nobegin nullrp nullfb nullfb-il nullview nullfbview draw drawnopass; do
  set +e
  VK_ICD_FILENAMES="$ICD" ./rp_attach_count "$m" >"$out" 2>&1
  rc=$?
  set -e
  grep -E "^mode=|VU violation|violation|Assertion|failed|^draw returned|^RESULT" "$out" || true
  echo "$m exit=$rc"
done
rm -f "$out"
