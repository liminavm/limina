#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the probe against the freshly-built KosmicKrisp dylib in /Volumes/mesa-cs/build-kk.
# RED (unfixed KK): SIGABRT with the vk_render_pass.c:2708 format assert.
# GREEN (fixed KK):  exits 0 with the readback proof.
set -uo pipefail
cd "$(dirname "$0")"

export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"
echo "VK_ICD_FILENAMES=$VK_ICD_FILENAMES"
./probe
echo "exit=$?"
