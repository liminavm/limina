#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the Vulkan timestamp probe against a KK build on a REMOTE Mac — specifically dogfood-mac, the
# M4 Pro, which is the only machine that exhibits the counter-resolve defect this whole spike is
# about. The dev Mac (M1 Max) cannot falsify a fix for it; see RESULTS.md.
#
#   ./run-remote-m4.sh [host] [runs]        # default: dogfood-mac, 100
#
# Ships the locally-built dylib + probe.c to the host's /tmp and builds there. Everything it
# writes is confined to /tmp — it does NOT touch the installed limina.app, any VM, or any config
# (dogfood-mac is the dogfood Mac; see the dogfood-mac-hands-off memory). Re-running replaces its own files.
#
# Bootstrap it needs on the host, from an earlier session and left in place:
#   /tmp/libvulkan.1.dylib     the Vulkan loader (no Homebrew vulkan-loader there)
#   /tmp/libSPIRV-Tools.dylib  KK links it by Homebrew abs path, which the host lacks
#   /tmp/kkinc/                Vulkan headers
# If those are gone, copy them from this Mac's /opt/homebrew (lib/libvulkan.1.dylib,
# opt/spirv-tools/lib/libSPIRV-Tools.dylib, include/{vulkan,vk_video}).
set -euo pipefail
cd "$(dirname "$0")"

HOST="${1:-dogfood-mac}"
RUNS="${2:-100}"
DYLIB="${KK_DYLIB:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib}"
[ -f "$DYLIB" ] || { echo "no KK dylib at $DYLIB"; exit 1; }

echo "==> shipping $(basename "$DYLIB") ($(stat -f%z "$DYLIB") B) + probe.c to $HOST:/tmp"
scp -q "$DYLIB" "$HOST:/tmp/libkk_probe.dylib"
scp -q probe.c "$HOST:/tmp/probe_kk.c"

# ssh may land in fish; force bash for the remote script.
ssh "$HOST" "cat > /tmp/kk-probe-run.sh" <<REMOTE
#!/bin/bash
set -euo pipefail
# KK records Homebrew's SPIRV-Tools path; repoint it at the copy this rig carries.
install_name_tool -change /opt/homebrew/opt/spirv-tools/lib/libSPIRV-Tools.dylib \\
  /tmp/libSPIRV-Tools.dylib /tmp/libkk_probe.dylib
codesign -f -s - /tmp/libkk_probe.dylib >/dev/null 2>&1 || true
printf %s '{"file_format_version":"1.0.0","ICD":{"library_path":"/tmp/libkk_probe.dylib","api_version":"1.3.0"}}' \\
  > /tmp/kk_probe_icd.json

cc -g -O0 -o /tmp/probe_kk /tmp/probe_kk.c -I/tmp/kkinc /tmp/libvulkan.1.dylib -Wl,-rpath,/tmp
# Same story for the loader: the link records a Homebrew path the host does not have.
install_name_tool -change /opt/homebrew/opt/vulkan-loader/lib/libvulkan.1.dylib \\
  /tmp/libvulkan.1.dylib /tmp/probe_kk
codesign -f -s - /tmp/probe_kk >/dev/null 2>&1 || true

export VK_ICD_FILENAMES=/tmp/kk_probe_icd.json VK_DRIVER_FILES=/tmp/kk_probe_icd.json

echo "=== one traced run ==="
LIMINA_KK_TS_TRACE=1 /tmp/probe_kk 2>&1 | head -40

echo
echo "=== $RUNS runs ==="
# A is a bare poll, so VK_NOT_READY is a legal answer and is counted apart from a real value.
# ZERO = value 0 reported as AVAILABLE: the disease. It must be 0.
a_real=0; a_nr=0; a_zero=0; b_real=0; b_zero=0; c_real=0; c_zero=0
for i in \$(seq 1 $RUNS); do
  out=\$(/tmp/probe_kk 2>/dev/null)
  case "\$(echo "\$out" | grep -E '^A ')" in
    *"-> 0"*"q0=0 avail=1"*) a_zero=\$((a_zero+1));;
    *"-> 0"*)                a_real=\$((a_real+1));;
    *"-> 1"*)                a_nr=\$((a_nr+1));;
  esac
  case "\$(echo "\$out" | grep -E '^B ')" in *"q0=0 avail=1"*) b_zero=\$((b_zero+1));; *) b_real=\$((b_real+1));; esac
  case "\$(echo "\$out" | grep -E '^C ')" in *"q0=0 avail=1"*) c_zero=\$((c_zero+1));; *) c_real=\$((c_real+1));; esac
done
echo "A  real=\$a_real  not_ready=\$a_nr  ZERO-as-available=\$a_zero  / $RUNS"
echo "B  real=\$b_real  ZERO-as-available=\$b_zero  / $RUNS"
echo "C  real=\$c_real  ZERO-as-available=\$c_zero  / $RUNS"
REMOTE

ssh "$HOST" 'bash /tmp/kk-probe-run.sh'
