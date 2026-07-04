#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build xfb-oob-probe (RED reproducer for the KK transform-feedback OOB write).
# The probe dlopens the KK dylib directly (ICD interface) — it does NOT link or
# use the system Vulkan loader — so it needs only the Vulkan headers.
#
# Point it at a KK build via KK_DYLIB and run:
#   KK_DYLIB=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib \
#     ./xfb-oob-probe            # default index 4096 (past the 4-elem array)
set -eu
cd "$(dirname "$0")"
VK_HEADERS=$(brew --prefix vulkan-headers)
cc -g -O1 -o xfb-oob-probe xfb-oob-probe.c -I"$VK_HEADERS/include"
echo "built ./xfb-oob-probe"
