#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Sweep render-pass attachment combinations against the host KosmicKrisp build.
# RED:   SIGABRT; the LAST "TRY" line names the combination that triggered it.
# GREEN: "swept N combinations ... no abort".
set -uo pipefail
cd "$(dirname "$0")"

export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json}"
echo "VK_ICD_FILENAMES=$VK_ICD_FILENAMES"
./rpcombo "$@"
echo "exit=$?"
