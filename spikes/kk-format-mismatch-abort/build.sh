#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the kk-format-mismatch-abort probe: link the Homebrew Vulkan loader; the KK ICD
# is selected at run time via VK_ICD_FILENAMES (see run.sh). No codesigning needed —
# the probe never touches hv_vm_* (no VM).
set -euo pipefail
cd "$(dirname "$0")"

clang -O1 -g -Wall -o probe probe.c \
   -I"$(brew --prefix vulkan-headers)/include" \
   -L"$(brew --prefix vulkan-loader)/lib" -lvulkan
echo "built ./probe"
