#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the ArmVirtKrun GOP firmware inside the f44-edk2-build VM (the build VM preferred over the
# Apple `container` for firmware work — see docs/images.md). This is the build-VM sibling of
# scripts/build-krun-efi.sh and applies the same limina platform patches via ~/edk2-patch.py plus
# the VirtioSerial TPL fix (the windowed-GOP hang root cause; see
# spikes/archive/windowed-firmware-hang/README.md). DEPLOY: scp this to the VM as ~/guest-build-edk2.sh.
#
#   TARGET=RELEASE         -> ~/KRUN_EFI.gop.fd          (production windowed default)
#   TARGET=DEBUG           -> ~/KRUN_EFI.gop.debug[.caller].fd  (verbose boot-serial)
#   CALLERPRINT=1 (default) inserts the CoreRaiseTpl culprit-address diag print; =0 for clean fw.
# Then scp the blob back into target/krun-efi/.
set -euo pipefail
JOBS="${JOBS:-8}"
TARGET="${TARGET:-DEBUG}"

echo "=== installing edk2 build deps ==="
sudo dnf -y install gcc gcc-c++ make python3 git nasm acpica-tools libuuid-devel \
    which findutils diffutils >/dev/null 2>&1 || \
sudo dnf -y install gcc gcc-c++ make python3 git nasm acpica-tools libuuid-devel which findutils diffutils

# Fedora 44's GCC 16 defaults to gnu23, where `()` means `(void)`; slp/edk2's bundled BaseTools
# (Pccts antlr/dlg) is ancient K&R C with unprototyped `()` function pointers, so C23 turns its
# calls into hard errors ("too many arguments"). Those Pccts makefiles hardcode their own CFLAGS
# and ignore EXTRA_OPTFLAGS, so the only reliable lever is a compiler wrapper. C compiles get
# -std=gnu17 (restores K&R `()` semantics — what the container's GCC 14 defaulted to) plus
# -Wno-error; C++ compiles get only -Wno-error (a C std flag is invalid for g++). Last-wins, so
# these override anything the makefiles set. Build tools only; benign for the firmware C too.
echo "=== installing gcc wrapper (-std=gnu17 / -Wno-error for GCC 16) ==="
mkdir -p ~/ccwrap
for c in gcc cc; do
    real="$(command -v "/usr/bin/$c" || true)"; [ -n "$real" ] || continue
    printf '#!/bin/bash\nexec %s "$@" -std=gnu17 -Wno-error\n' "$real" > ~/ccwrap/"$c"
    chmod +x ~/ccwrap/"$c"
done
for c in g++ c++; do
    real="$(command -v "/usr/bin/$c" || true)"; [ -n "$real" ] || continue
    printf '#!/bin/bash\nexec %s "$@" -Wno-error\n' "$real" > ~/ccwrap/"$c"
    chmod +x ~/ccwrap/"$c"
done
export PATH="$HOME/ccwrap:$PATH"

cd ~
if [ ! -d edk2/.git ]; then
    echo "=== cloning slp/edk2 @ krun-support ==="
    git clone --depth 1 --branch krun-support https://github.com/slp/edk2 edk2
else
    echo "=== reusing edk2 checkout ==="
fi
cd edk2
git checkout -- ArmVirtPkg/ArmVirtKrun.dsc ArmVirtPkg/ArmVirtKrun.fdf \
    ArmVirtPkg/Library/PlatformBootManagerLib/PlatformBm.c 2>/dev/null || true
git checkout -- MdeModulePkg/Core/Dxe/Event/Tpl.c 2>/dev/null || true
git checkout -- OvmfPkg/VirtioSerialDxe/VirtioSerialPort.c 2>/dev/null || true

echo "=== FIX: VirtioSerial SerialIo Read/Write raise to TPL_NOTIFY (not TPL_CALLBACK) ==="
# VirtioSerialIoRead/Write are reachable from TerminalDxe's TPL_NOTIFY serial-poll timer
# (Terminal.c registers TimerEvent at TPL_NOTIFY). RaiseTPL(TPL_CALLBACK) at TPL_NOTIFY is
# illegal (NewTpl<OldTpl) -> CoreRaiseTpl ASSERT -> CpuDeadLoop in the DEBUG firmware -> windowed
# GOP boot hang. TPL_NOTIFY is the correct level for shared-virtqueue access anyway (CALLBACK
# was too low to even serialize against that NOTIFY poll timer). Minimal, upstreamable.
VS=OvmfPkg/VirtioSerialDxe/VirtioSerialPort.c
sed -i 's/OldTpl = gBS->RaiseTPL (TPL_CALLBACK);/OldTpl = gBS->RaiseTPL (TPL_NOTIFY); \/\/ limina: was TPL_CALLBACK; reachable at TPL_NOTIFY from TerminalDxe poll timer/' "$VS"
echo "  RaiseTPL(TPL_NOTIFY) sites now (want 2): $(grep -c 'RaiseTPL (TPL_NOTIFY)' "$VS")"
echo "  RaiseTPL(TPL_CALLBACK) sites left (want 0): $(grep -c 'RaiseTPL (TPL_CALLBACK)' "$VS")"

echo "=== init required submodules ==="
git submodule update --init --depth 1 -- \
    MdePkg/Library/BaseFdtLib/libfdt \
    MdePkg/Library/MipiSysTLib/mipisyst \
    CryptoPkg/Library/OpensslLib/openssl \
    BaseTools/Source/C/BrotliCompress/brotli \
    MdeModulePkg/Library/BrotliCustomDecompressLib/brotli \
    MdeModulePkg/Universal/RegularExpressionDxe/oniguruma

echo "=== applying limina platform patches (VirtioGpuDxe + PlatformBm ConOut) ==="
GOP_ENABLED=1 python3 ~/edk2-patch.py

# CALLERPRINT=1 (default) inserts the CoreRaiseTpl culprit-address print used to root-cause the
# hang. Set CALLERPRINT=0 for a clean production firmware (no diag code).
CALLERPRINT="${CALLERPRINT:-1}"
F=MdeModulePkg/Core/Dxe/Event/Tpl.c
git checkout -- "$F" 2>/dev/null || true
if [ "$CALLERPRINT" = 1 ]; then
    echo "=== adding CoreRaiseTpl caller-print (diag) ==="
    sed -i '/LIMINA_RAISETPL_CALLER/d' "$F"
    sed -i '/FATAL ERROR - RaiseTpl with OldTpl/a\    DEBUG ((DEBUG_ERROR, "LIMINA_RAISETPL_CALLER ra0=%p ra1=%p ra2=%p\\n", RETURN_ADDRESS (0), RETURN_ADDRESS (1), RETURN_ADDRESS (2)));' "$F"
    echo "  LIMINA print count (want 1): $(grep -c LIMINA_RAISETPL_CALLER "$F")"
else
    echo "=== caller-print disabled (clean build) ==="
fi

echo "=== building BaseTools ==="
make -C BaseTools -j"$JOBS" EXTRA_OPTFLAGS=-std=gnu17 >/tmp/basetools.log 2>&1 || { tail -40 /tmp/basetools.log; exit 1; }

export WORKSPACE="$PWD" PACKAGES_PATH="$PWD" GCC5_AARCH64_PREFIX=
set +u; . ./edksetup.sh BaseTools; set -u

echo "=== build -a AARCH64 -t GCC5 -b $TARGET ArmVirtKrun.dsc ==="
build -a AARCH64 -t GCC5 -b "$TARGET" -n "$JOBS" -p ArmVirtPkg/ArmVirtKrun.dsc -D FD_SIZE_IN_MB=2

FD="Build/ArmVirtKrun-AARCH64/${TARGET}_GCC5/FV/KRUN_EFI.fd"
[ -f "$FD" ] || { echo "FD not found at $FD"; find Build -name KRUN_EFI.fd; exit 1; }
# Name the output to match scripts/build-krun-efi.sh's convention so it drops straight into
# target/krun-efi/: RELEASE GOP -> KRUN_EFI.gop.fd (production), DEBUG GOP -> .gop.debug[.caller].fd
if [ "$TARGET" = "RELEASE" ]; then
    OUT=~/KRUN_EFI.gop.fd
elif [ "$CALLERPRINT" = 1 ]; then
    OUT=~/KRUN_EFI.gop.debug.caller.fd
else
    OUT=~/KRUN_EFI.gop.debug.fd
fi
cp -f "$FD" "$OUT"
echo "=== BUILT: $(wc -c < "$OUT") bytes -> $OUT (TARGET=$TARGET CALLERPRINT=$CALLERPRINT) ==="
