#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the KRUN_EFI ArmVirtKrun firmware (EDK2) for limina's EFI boot path, using Apple
# `container` as the (host-native aarch64) Linux build environment.
#
# Why our own build (M2.5 Track B): krunkit ships only KRUN_EFI.silent.fd — serial-only,
# no GOP — so EFI/GRUB/early-kernel are invisible in the limina window. The ArmVirtKrun
# platform ALREADY wires the full graphics-console stack (ConSplitter/GraphicsConsole/GOP
# support); it's "silent" only because no GOP-*producing* video driver is included. Adding
# `OvmfPkg/VirtioGpuDxe` (which binds libkrun's virtio-gpu over the mmio VIRTIO_DEVICE_PROTOCOL,
# same as VirtioBlk/Rng/Serial) gives the firmware a graphics console on our virtio-gpu
# scanout — which our software-2D libkrun patch already presents.
#
# Source: slp/edk2 @ krun-support (ArmVirtPkg/ArmVirtKrun.{dsc,fdf}); the same tree krunkit's
# blob is built from. Output: a KRUN_EFI.fd, gitignored under target/.
#
# Caching: the edk2 checkout + build live in a PERSISTENT `container volume` (case-sensitive
# ext4) so re-runs are incremental. Reset with `container volume rm limina-edk2-build`.
#
# Usage: scripts/build-krun-efi.sh
#   GOP=0 scripts/build-krun-efi.sh    # Phase 0: build the UNMODIFIED (silent) firmware
#   GOP=1 scripts/build-krun-efi.sh    # (default) add VirtioGpuDxe -> graphical boot console
#   TARGET=RELEASE scripts/build-krun-efi.sh   # DEBUG (default, matches shipped) or RELEASE
# Prereq: `container system start`. Network required (first run clones edk2 + submodules).
set -euo pipefail
cd "$(dirname "$0")/.."

EDK2_REPO="${EDK2_REPO:-https://github.com/slp/edk2}"
EDK2_BRANCH="${EDK2_BRANCH:-krun-support}"
TARGET="${TARGET:-DEBUG}"                 # DEBUG (matches the shipped blob) or RELEASE
GOP="${GOP:-1}"                           # 1 = add VirtioGpuDxe (graphical console)
JOBS="${JOBS:-8}"
MEM="${MEM:-8g}"
OUT="target/krun-efi"                     # gitignored (under target/)
mkdir -p "$OUT"

case "$GOP" in
    1) OUT_NAME="KRUN_EFI.gop.fd" ;;
    0) OUT_NAME="KRUN_EFI.silent-rebuilt.fd" ;;
    *) echo "GOP must be 0 or 1 (got '$GOP')" >&2; exit 1 ;;
esac

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

VOL="limina-edk2-build"
container volume create -s 24g "$VOL" >/dev/null 2>&1 || true

echo "==> building KRUN_EFI ($TARGET, GOP=$GOP) from $EDK2_REPO@$EDK2_BRANCH in an Apple container"
echo "    build volume: $VOL (incremental across runs); output: $OUT/$OUT_NAME"

container run --rm --cpus "$JOBS" --memory "$MEM" \
    -v "$(pwd)/$OUT:/out" \
    -v "$VOL:/build" \
    docker.io/library/fedora:41 bash -euo pipefail -c "
        TARGET='$TARGET'; GOP='$GOP'; OUT_NAME='$OUT_NAME'; JOBS='$JOBS'
        echo '--- installing edk2 build deps'
        dnf -y install gcc gcc-c++ make python3 git nasm acpica-tools libuuid-devel \
            which findutils diffutils >/dev/null

        cd /build
        if [ ! -d edk2/.git ]; then
            echo '--- cloning edk2 ($EDK2_BRANCH) (first run)'
            git clone --depth 1 --branch '$EDK2_BRANCH' '$EDK2_REPO' edk2
        else
            echo '--- reusing edk2 checkout (incremental)'
        fi
        cd edk2
        # Restore pristine platform files (we patch them in-place below).
        git checkout -- ArmVirtPkg/ArmVirtKrun.dsc ArmVirtPkg/ArmVirtKrun.fdf 2>/dev/null || true
        # Init ONLY the submodules ArmVirtKrun actually needs (idempotent — already-init'd
        # ones are skipped). Avoids the UnitTestFrameworkPkg submodules (subhook/googletest/
        # cmocka) we never build, one of which failed to clone. libfdt = FDT parsing (ArmVirt),
        # openssl = EnrollDefaultKeys crypto, brotli (x2) = BaseTools BrotliCompress + decompress.
        echo '--- init required submodules'
        # libfdt/openssl/brotli = actually compiled. mipisyst/oniguruma = NOT compiled, but
        # MdePkg.dec / MdeModulePkg.dec declare [Includes] paths into them, which edk2's
        # meta-data parser validates exist when those (always-used) packages are loaded.
        git submodule update --init --depth 1 -- \
            MdePkg/Library/BaseFdtLib/libfdt \
            MdePkg/Library/MipiSysTLib/mipisyst \
            CryptoPkg/Library/OpensslLib/openssl \
            BaseTools/Source/C/BrotliCompress/brotli \
            MdeModulePkg/Library/BrotliCustomDecompressLib/brotli \
            MdeModulePkg/Universal/RegularExpressionDxe/oniguruma

        # --- patch the ArmVirtKrun platform ---
        # (1) drop the stale TerminalPcdProducerLib ref (removed from edk2; ArmVirtKrun.dsc
        #     HEAD still references it, which is why the shipped blob is built from an older
        #     commit — ArmVirtQemu.dsc on this same branch already dropped it).
        # (2) GOP=1: add OvmfPkg/VirtioGpuDxe so GraphicsConsoleDxe has a GOP to bind.
        echo '--- patching ArmVirtKrun platform (drop stale lib; GOP='\"\$GOP\"')'
        GOP_ENABLED=\"\$GOP\" python3 - <<'PY'
import os
dsc = 'ArmVirtPkg/ArmVirtKrun.dsc'
fdf = 'ArmVirtPkg/ArmVirtKrun.fdf'

# (1) Replace the TerminalDxe component block (which pulls the removed
# TerminalPcdProducerLib via NULL|) with a plain INF line, matching ArmVirtQemu.dsc.
s = open(dsc).read()
stale = ('  MdeModulePkg/Universal/Console/TerminalDxe/TerminalDxe.inf {\n'
         '    <LibraryClasses>\n'
         '      NULL|ArmVirtPkg/Library/TerminalPcdProducerLib/TerminalPcdProducerLib.inf\n'
         '  }\n')
plain = '  MdeModulePkg/Universal/Console/TerminalDxe/TerminalDxe.inf\n'
if stale in s:
    s = s.replace(stale, plain, 1)
    print('  dropped stale TerminalPcdProducerLib ref')
elif 'TerminalPcdProducerLib' in s:
    raise SystemExit('TerminalPcdProducerLib present but block shape changed — update patch')
# PcdResizeXterm was produced (patchable) by TerminalPcdProducerLib; with that lib gone the
# PCD is orphaned (edk2 removed both together, as did ArmVirtQemu.dsc). Drop its line.
s = s.replace('  gEfiMdeModulePkgTokenSpaceGuid.PcdResizeXterm|FALSE\n', '', 1)
open(dsc, 'w').write(s)

# (2) GOP: insert VirtioGpuDxe after VirtioSerial (same binding family) in .dsc + .fdf.
if os.environ.get('GOP_ENABLED') == '1':
    def add(path, anchor, addition):
        t = open(path).read()
        if 'VirtioGpuDxe' in t:
            return
        assert anchor in t, f'anchor not found in {path}: {anchor!r}'
        open(path, 'w').write(t.replace(anchor, anchor + addition, 1))
    add(dsc, '  OvmfPkg/VirtioSerialDxe/VirtioSerial.inf\n',
        '  OvmfPkg/VirtioGpuDxe/VirtioGpu.inf\n')
    add(fdf, '  INF OvmfPkg/VirtioSerialDxe/VirtioSerial.inf\n',
        '  INF OvmfPkg/VirtioGpuDxe/VirtioGpu.inf\n')
    print('  added OvmfPkg/VirtioGpuDxe')
PY

        echo '--- building BaseTools (incremental; no-op if already built)'
        # -std=gnu17: BaseTools' bundled Pccts (ANTLR/DLG) is K&R C that gcc>=15 rejects
        # (C23 makes '()' mean '(void)'); gnu17 keeps the old unspecified-args meaning. If a
        # build ever fails here after a base-image/compiler bump, `container volume rm $VOL`.
        make -C BaseTools -j\"\$JOBS\" EXTRA_OPTFLAGS=-std=gnu17 \
            >/tmp/basetools.log 2>&1 || { tail -40 /tmp/basetools.log; exit 1; }

        export WORKSPACE=/build/edk2
        export PACKAGES_PATH=/build/edk2
        export GCC5_AARCH64_PREFIX=
        set +u; . ./edksetup.sh BaseTools; set -u

        echo \"--- build -a AARCH64 -t GCC5 -b \$TARGET -p ArmVirtPkg/ArmVirtKrun.dsc\"
        build -a AARCH64 -t GCC5 -b \"\$TARGET\" -n \"\$JOBS\" \
            -p ArmVirtPkg/ArmVirtKrun.dsc -D FD_SIZE_IN_MB=2

        FD=\"Build/ArmVirtKrun-AARCH64/\${TARGET}_GCC5/FV/KRUN_EFI.fd\"
        [ -f \"\$FD\" ] || { echo \"FD not found at \$FD\"; find Build -name 'KRUN_EFI.fd' 2>/dev/null; exit 1; }
        cp -f \"\$FD\" \"/out/\$OUT_NAME\"
        echo \"--- done: \$(wc -c < /out/\$OUT_NAME) bytes -> \$OUT_NAME\"
    "

echo "==> KRUN_EFI ready: $OUT/$OUT_NAME"
echo "    boot it via: --firmware $OUT/$OUT_NAME"
