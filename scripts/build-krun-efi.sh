#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the KRUN_EFI ArmVirtKrun firmware (EDK2) for limina's EFI boot path, in the unified
# `limina-build:fc43` container image (scripts/build-image.sh) — the same host-native aarch64
# Linux build env every limina Linux build uses.
#
# Source: the FORK MODEL (task #22, 2026-08-06) — github.com/liminavm/edk2, branch `limina`
# (base: slp/edk2 @ krun-support, the tree krunkit's shipped blob is built from), pinned by
# `[edk2]` in third_party/manifest.toml. The branch IS the delta: the TerminalPcdProducerLib
# build fix, the VirtioSerialDxe TPL fix, PL031 DT status=okay, the vendored
# VirtioKeyboardDxe, and the VirtioGpuDxe GOP + ConIn enablement are all commits there —
# NOTHING is patched at build time any more (patches/edk2/ is a tombstone). To change the
# firmware: commit on the fork's `limina` branch, push, bump the manifest rev.
#
# Why our own build (M2.5 Track B): krunkit ships only KRUN_EFI.silent.fd — serial-only, no
# GOP, and a DEBUG build whose live ASSERTs end in CpuDeadLoop (the #14 cold-boot wedge).
# Ours is RELEASE by default and carries the graphical, typeable boot console.
#
# Caching: the edk2 checkout + build live in a PERSISTENT `container volume` (case-sensitive
# ext4) so re-runs are incremental. Reset with `container volume rm limina-edk2-build`.
#
# Usage: scripts/build-krun-efi.sh
#   TARGET=DEBUG scripts/build-krun-efi.sh    # verbose boot-serial build (separate output name)
# Prereq: `container system start`. Network required (first run clones edk2 + submodules;
# rev bumps re-fetch).
set -euo pipefail
cd "$(dirname "$0")/.."

# The pin lives in the manifest; EDK2_REV/EDK2_REPO env override for fork surgery only.
MANIFEST_REV=$(awk '/^\[edk2\]/{f=1; next} /^\[/{f=0} f && /^rev = /{gsub(/"/, "", $3); print $3; exit}' third_party/manifest.toml)
[ -n "$MANIFEST_REV" ] || { echo "no [edk2] rev in third_party/manifest.toml" >&2; exit 1; }
EDK2_REPO="${EDK2_REPO:-https://github.com/liminavm/edk2}"
EDK2_REV="${EDK2_REV:-$MANIFEST_REV}"
# RELEASE is the production default (smaller, no DEBUG overhead; ASSERTs compile out, so a
# firmware error degrades instead of dead-looping). DEBUG builds boot fine too (the TPL fix
# is on the branch) and write a separate output so they never clobber the default.
TARGET="${TARGET:-RELEASE}"
JOBS="${JOBS:-8}"
MEM="${MEM:-8g}"
OUT="target/krun-efi"                     # gitignored (under target/)
mkdir -p "$OUT"

if [ "$TARGET" = "DEBUG" ]; then OUT_NAME="KRUN_EFI.gop.debug.fd"; else OUT_NAME="KRUN_EFI.gop.fd"; fi

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

VOL="limina-edk2-build"
container volume create -s 24g "$VOL" >/dev/null 2>&1 || true

echo "==> building KRUN_EFI ($TARGET) from $EDK2_REPO @ $EDK2_REV in an Apple container"
echo "    build volume: $VOL (incremental across runs); output: $OUT/$OUT_NAME"

scripts/build-image.sh   # ensure the unified limina-build image (edk2 deps + gnu17 ccwrap baked)
container run --rm --cpus "$JOBS" --memory "$MEM" \
    -v "$(pwd)/$OUT:/out" \
    -v "$VOL:/build" \
    limina-build:fc43 bash -euo pipefail -c "
        TARGET='$TARGET'; OUT_NAME='$OUT_NAME'; JOBS='$JOBS'; EDK2_REV='$EDK2_REV'
        # The unified image is F43, whose gcc defaults to gnu23 and miscompiles edk2's K&R
        # BaseTools ('()' becomes '(void)'). The image ships a -std=gnu17 wrapper OUT of PATH so it
        # can't taint the kernel/mesa builds; opt into it here only (build tools only — the
        # firmware itself is cross-compiled by edk2's own GCC5 toolchain, unaffected).
        export PATH=/opt/limina-ccwrap:\$PATH

        cd /build
        if [ ! -d edk2/.git ]; then
            echo '--- cloning edk2 (first run)'
            git clone --branch limina --depth 50 '$EDK2_REPO' edk2
        fi
        cd edk2
        # A volume from the pre-fork era points at slp/edk2 and carries the old script's
        # in-place patches + untracked vendored files: repoint the remote and clear the
        # platform dirs (NOT Build/ — that is the incremental cache).
        git remote set-url origin '$EDK2_REPO'
        git reset --hard >/dev/null
        git clean -fdq -- OvmfPkg ArmVirtPkg
        if ! git cat-file -e \"\$EDK2_REV^{commit}\" 2>/dev/null; then
            echo '--- fetching the pinned rev'
            git fetch --depth 50 origin \"\$EDK2_REV\"
        fi
        git checkout --detach \"\$EDK2_REV\"
        echo \"--- edk2 at \$(git rev-parse --short HEAD) (\$(git log -1 --format=%s))\"

        # Init ONLY the submodules ArmVirtKrun actually needs (idempotent — already-init'd
        # ones are skipped). libfdt/openssl/brotli are actually compiled; mipisyst/oniguruma
        # are NOT, but MdePkg.dec / MdeModulePkg.dec declare [Includes] paths into them,
        # which edk2's meta-data parser validates exist.
        echo '--- init required submodules'
        git submodule update --init --depth 1 -- \
            MdePkg/Library/BaseFdtLib/libfdt \
            MdePkg/Library/MipiSysTLib/mipisyst \
            CryptoPkg/Library/OpensslLib/openssl \
            BaseTools/Source/C/BrotliCompress/brotli \
            MdeModulePkg/Library/BrotliCustomDecompressLib/brotli \
            MdeModulePkg/Universal/RegularExpressionDxe/oniguruma

        echo '--- building BaseTools (incremental; no-op if already built)'
        # -std=gnu17: BaseTools' bundled Pccts (ANTLR/DLG) is K&R C that gcc>=15 rejects
        # (C23 makes '()' mean '(void)'); gnu17 keeps the old unspecified-args meaning. If a
        # build ever fails here after a base-image/compiler bump, \`container volume rm $VOL\`.
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
