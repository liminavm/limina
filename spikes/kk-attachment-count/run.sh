#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Build the probe and run every mode against a KosmicKrisp ICD (default: the host build limina
# uses). usage: run.sh [icd.json]
set -e
cd "$(dirname "$0")"
ICD="${1:-$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json)}"
cc -o rp_attach_count rp_attach_count.c -I/opt/homebrew/include -L/opt/homebrew/lib -lvulkan
for m in ok imageless fb nobegin; do
  set +e
  VK_ICD_FILENAMES="$ICD" ./rp_attach_count "$m" 2>&1 | grep -E "^mode=|VU violation|Assertion|^RESULT"
  echo "$m exit=$?"
  set -e
done
