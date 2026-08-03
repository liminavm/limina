#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

set -e
cd ~/Projects/limina
ICD="/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json"
MESA_PREFIX="/Volumes/mesa-cs/zink-kk-prefix"
export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$PWD/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
target/debug/limina "$@"
