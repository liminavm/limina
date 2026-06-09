#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + deploy an INSTRUMENTED host MoltenVK as a venus-debugging oracle.
#
# Why: when venus rendering is opaque (e.g. bug A #31), the surest instrument is the dependency we
# own. This builds third_party/MoltenVK-src (gitignored) with our fprintf probes and points the
# worker at it via VK_ICD_FILENAMES (see boot-mvkinst.sh). The instrumentation itself lives as
# mvk-instrument.patch (next to this file) because the source tree is gitignored — re-apply it to a
# fresh checkout with:  ./rebuild-mvk.sh --apply-patch
#
# Probes the patch adds (search the worker log):
#   [LIMINA-DRAW] prim/vtxCount/instCount/cull/raster/viewport/scissor at each non-indexed draw
#   [LIMINA-VTX]  the actual Shared vertex buffer the GPU fetches (firstNonzeroByte scan = coherency tell)
#
# First-time setup of a fresh MoltenVK checkout also needs:  (cd third_party/MoltenVK-src && ./fetchDependencies --macos)
set -e -o pipefail  # pipefail is load-bearing: without it `make | tail` masks compile failures and DEPLOYS A STALE DYLIB
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
MVK="$ROOT/third_party/MoltenVK-src"
HERE="$ROOT/spikes/venus-draw-probe"
DEST=/tmp/mvk-instrumented   # runtime deploy dir (transient by design; rebuilt from this script)

if [ "$1" = "--apply-patch" ]; then
  echo "applying mvk-instrument.patch to $MVK"
  git -C "$MVK" apply "$HERE/mvk-instrument.patch"
  echo "applied. now run ./rebuild-mvk.sh (no args) to build+deploy."
  exit 0
fi

cd "$MVK"
make macos 2>&1 | tail -3
mkdir -p "$DEST"
cp Package/Release/MoltenVK/dynamic/dylib/macOS/libMoltenVK.dylib "$DEST/libMoltenVK.dylib"
codesign -s - --force "$DEST/libMoltenVK.dylib"   # worker is adhoc/no-hardened-runtime, so it loads this
cat > "$DEST/MoltenVK_icd.json" <<JSON
{
    "file_format_version" : "1.0.0",
    "ICD": {
        "library_path": "$DEST/libMoltenVK.dylib",
        "api_version" : "1.4.0",
        "is_portability_driver" : true
    }
}
JSON
echo "DEPLOYED $DEST  (probes present: $(strings "$DEST/libMoltenVK.dylib" | grep -c LIMINA-))"
echo "boot with it via: spikes/venus-draw-probe/boot-mvkinst.sh"
