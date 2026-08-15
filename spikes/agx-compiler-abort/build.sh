#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the rpcombo sweep: Homebrew Vulkan loader, KK ICD chosen at run time (see run.sh).
# No codesigning needed — it never touches hv_vm_* (no VM).
set -euo pipefail
cd "$(dirname "$0")"

clang -O1 -g -Wall -o rpcombo rpcombo.c \
   -I"$(brew --prefix vulkan-headers)/include" \
   -L"$(brew --prefix vulkan-loader)/lib" -lvulkan
echo "built ./rpcombo"
