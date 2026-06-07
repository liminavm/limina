#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + run the MoltenVK IOSurface-export probe (host-only; no VM boot).
set -euo pipefail
cd "$(dirname "$0")"

VK_HEADERS="$(ls -d /opt/homebrew/Cellar/vulkan-headers/*/include 2>/dev/null | head -1)"
VK_LOADER_LIB="/opt/homebrew/opt/vulkan-loader/lib"
MVK_ICD="$(ls /opt/homebrew/Cellar/molten-vk/*/etc/vulkan/icd.d/MoltenVK_icd.json 2>/dev/null | head -1)"

[ -n "$VK_HEADERS" ] || { echo "vulkan-headers not found (brew install vulkan-headers)"; exit 1; }
[ -n "$MVK_ICD" ]    || { echo "MoltenVK_icd.json not found (brew install molten-vk)"; exit 1; }

clang -g -O0 -fno-objc-arc \
  -I"$VK_HEADERS" \
  -framework Foundation -framework Metal -framework IOSurface -framework QuartzCore -framework CoreGraphics \
  -L"$VK_LOADER_LIB" -lvulkan \
  probe.m -o probe

# Point the loader straight at MoltenVK (the worker does the same; avoids any system ICD).
# Newer loaders prefer VK_DRIVER_FILES; set both for safety.
export VK_DRIVER_FILES="$MVK_ICD"
export VK_ICD_FILENAMES="$MVK_ICD"
echo "ICD: $MVK_ICD"
echo "---"
exec ./probe
