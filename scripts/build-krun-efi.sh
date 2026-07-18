#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the KRUN_EFI ArmVirtKrun firmware (EDK2) for limina's EFI boot path, in the unified
# `limina-build:fc43` container image (scripts/build-image.sh) — the same host-native aarch64
# Linux build env every limina Linux build now uses. (Replaced the standalone f44-edk2-build VM,
# retired 2026-06-25; the image bakes edk2's deps + a -std=gnu17 ccwrap for its K&R BaseTools.)
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
# RELEASE is the default for production (smaller, no DEBUG overhead). Both DEBUG and RELEASE now
# boot windowed-GOP: the windowed "BDS hang" was an EDK2 DxeCore ASSERT (CoreRaiseTpl OldTpl 0x10 >
# NewTpl 0x8, Tpl.c:66) — VirtioSerialDxe's SerialIo Read/Write raised to TPL_CALLBACK while
# entered at TPL_NOTIFY from TerminalDxe's serial-poll timer. It is FIXED at the source by step
# (1b) below (RaiseTPL TPL_CALLBACK -> TPL_NOTIFY), so DEBUG_GCC5 (ASSERT_DEADLOOP on) no longer
# dead-loops. Build TARGET=DEBUG for verbose boot-serial; it writes a separate KRUN_EFI.gop.debug.fd
# so it never clobbers the production default. See memory limina-windowed-reboot-present-race.
TARGET="${TARGET:-RELEASE}"               # RELEASE (production default) or DEBUG (dev serial; also boots)
GOP="${GOP:-1}"                           # 1 = add VirtioGpuDxe (graphical console)
JOBS="${JOBS:-8}"
MEM="${MEM:-8g}"
OUT="target/krun-efi"                     # gitignored (under target/)
mkdir -p "$OUT"

case "$GOP" in
    1) if [ "$TARGET" = "DEBUG" ]; then OUT_NAME="KRUN_EFI.gop.debug.fd"; else OUT_NAME="KRUN_EFI.gop.fd"; fi ;;
    0) OUT_NAME="KRUN_EFI.silent-rebuilt.fd" ;;
    *) echo "GOP must be 0 or 1 (got '$GOP')" >&2; exit 1 ;;
esac

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

VOL="limina-edk2-build"
container volume create -s 24g "$VOL" >/dev/null 2>&1 || true

echo "==> building KRUN_EFI ($TARGET, GOP=$GOP) from $EDK2_REPO@$EDK2_BRANCH in an Apple container"
echo "    build volume: $VOL (incremental across runs); output: $OUT/$OUT_NAME"

scripts/build-image.sh   # ensure the unified limina-build image (edk2 deps + gnu17 ccwrap baked)
container run --rm --cpus "$JOBS" --memory "$MEM" \
    -v "$(pwd)/$OUT:/out" \
    -v "$(pwd)/patches/edk2:/edk2-vendor" \
    -v "$VOL:/build" \
    limina-build:fc43 bash -euo pipefail -c "
        TARGET='$TARGET'; GOP='$GOP'; OUT_NAME='$OUT_NAME'; JOBS='$JOBS'
        # The unified image is F43, whose gcc defaults to gnu23 and miscompiles edk2's K&R
        # BaseTools ('()' becomes '(void)'). The image ships a -std=gnu17 wrapper OUT of PATH so it
        # can't taint the kernel/mesa builds; opt into it here only (build tools only — the
        # firmware itself is cross-compiled by edk2's own GCC5 toolchain, unaffected).
        export PATH=/opt/limina-ccwrap:\$PATH

        cd /build
        if [ ! -d edk2/.git ]; then
            echo '--- cloning edk2 ($EDK2_BRANCH) (first run)'
            git clone --depth 1 --branch '$EDK2_BRANCH' '$EDK2_REPO' edk2
        else
            echo '--- reusing edk2 checkout (incremental)'
        fi
        cd edk2
        # Restore pristine platform files (we patch them in-place below).
        git checkout -- ArmVirtPkg/ArmVirtKrun.dsc ArmVirtPkg/ArmVirtKrun.fdf \
            ArmVirtPkg/Library/PlatformBootManagerLib/PlatformBm.c \
            ArmVirtPkg/Library/PlatformBootManagerLib/PlatformBootManagerLib.inf \
            ArmVirtPkg/Library/ArmVirtPL031FdtClientLib/ArmVirtPL031FdtClientLib.c \
            OvmfPkg/Include/IndustryStandard/Virtio10.h \
            OvmfPkg/VirtioSerialDxe/VirtioSerialPort.c 2>/dev/null || true
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

        # Vendor OvmfPkg/VirtioKeyboardDxe (limina; pinned edk2-stable202505, carried in
        # the repo under patches/edk2/ and mounted at /edk2-vendor) — the slp/edk2 base
        # predates it. GOP-only: the virtio keyboard ConIn is for the windowed console.
        if [ \"\$GOP\" = '1' ]; then
            echo '--- vendoring OvmfPkg/VirtioKeyboardDxe'
            rm -rf OvmfPkg/VirtioKeyboardDxe
            cp -a /edk2-vendor/OvmfPkg/VirtioKeyboardDxe OvmfPkg/VirtioKeyboardDxe
        fi

        # --- patch the ArmVirtKrun platform ---
        # (1) drop the stale TerminalPcdProducerLib ref (removed from edk2; ArmVirtKrun.dsc
        #     HEAD still references it, which is why the shipped blob is built from an older
        #     commit — ArmVirtQemu.dsc on this same branch already dropped it).
        # (2) GOP=1: add OvmfPkg/VirtioGpuDxe so GraphicsConsoleDxe has a GOP to bind.
        # (3) GOP=1: patch PlatformBm.c to connect the virtio-mmio GPU into ConOut before
        #     console setup (else the non-PCI GOP never enters ConOut; blank boot console).
        # (4) GOP=1: vendor OvmfPkg/VirtioKeyboardDxe + wire libkrun's virtio-input keyboard
        #     into ConIn (IsVirtioInput connect + AddInput), so GRUB/firmware are typeable in
        #     the window. Without it ConIn has only a DEAD USB path + serial, so the window's
        #     virtio keyboard never reaches GRUB. Mirrors the (2)/(3) VirtioGpu->ConOut patch.
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

# (1b) VirtioSerial SerialIo Read/Write: raise to TPL_NOTIFY, not TPL_CALLBACK.
# TerminalDxe registers its serial-poll TimerEvent at TPL_NOTIFY (Terminal.c). When that timer
# polls an open virtio-serial port, VirtioSerialIoRead/Write run at TPL_NOTIFY (0x10) and call
# gBS->RaiseTPL(TPL_CALLBACK=0x8) — raising to a *lower* TPL, which is illegal. In a DEBUG build
# CoreRaiseTpl ASSERTs (OldTpl>NewTpl) -> CpuDeadLoop -> the windowed-GOP boot hangs; in RELEASE
# the assert is a no-op but CoreRaiseTpl still sets gEfiCurrentTpl=0x8, silently LOWERING the TPL
# (state corruption masked, not fixed). TPL_NOTIFY is the correct level for shared-virtqueue
# access anyway (CALLBACK was too low to even serialize against that NOTIFY poll timer), and
# RaiseTPL(NOTIFY) is legal whether entered at CALLBACK or NOTIFY. Minimal + upstreamable.
vsp = 'OvmfPkg/VirtioSerialDxe/VirtioSerialPort.c'
v = open(vsp).read()
old = 'OldTpl = gBS->RaiseTPL (TPL_CALLBACK);'
new = 'OldTpl = gBS->RaiseTPL (TPL_NOTIFY); // limina: was TPL_CALLBACK; reachable at TPL_NOTIFY from TerminalDxe poll timer'
n = v.count(old)
if n:
    if n != 2:
        raise SystemExit(f'VirtioSerialPort.c: expected 2 RaiseTPL(TPL_CALLBACK), found {n} — update patch')
    open(vsp, 'w').write(v.replace(old, new))
    print(f'  VirtioSerial: raised {n} SerialIo RaiseTPL site(s) TPL_CALLBACK -> TPL_NOTIFY')
elif 'RaiseTPL (TPL_NOTIFY)' not in v:
    raise SystemExit('VirtioSerialPort.c: no RaiseTPL(TPL_CALLBACK) and no TPL_NOTIFY — shape changed, update patch')

# (1c) Keep the PL031 RTC DT node visible to the OS (limina). ArmVirtPL031FdtClientLib's
# constructor sets the pl031 node status=\"disabled\" so ONLY UEFI runtime services drive the RTC.
# But the resulting EFI runtime RTC has no alarm (the guest's rtc-efi wakealarm ioctl returns
# EINVAL), leaving rtcwake / suspend-to-idle with no wakeup source. libkrun's PL031 alarm patch
# (0054) gives the device a real alarm IRQ, so flip that \"disabled\" to \"okay\": the guest's
# rtc-pl031 driver then binds and exposes its wakealarm (UEFI keeps using the same PL031 for
# GetTime — concurrent reads don't conflict). The stale in-file comment/DEBUG text is left as-is;
# this build-script comment is the authority.
rtc = 'ArmVirtPkg/Library/ArmVirtPL031FdtClientLib/ArmVirtPL031FdtClientLib.c'
r = open(rtc).read()
# The only double-quoted \"disabled\" occurrences are the SetNodeProperty value arg and its
# sizeof(); the DEBUG message uses single-quoted 'disabled'. So this targets exactly the DT status.
n = r.count('\"disabled\"')
if n == 2:
    open(rtc, 'w').write(r.replace('\"disabled\"', '\"okay\"'))
    print('  PL031: DT node kept enabled (status okay) for OS rtc-pl031 + wakealarm')
elif '\"disabled\"' not in r and '\"okay\"' in r:
    print('  PL031: already patched (status okay)')
else:
    raise SystemExit(f'ArmVirtPL031FdtClientLib.c: expected 2 \"disabled\" tokens, found {n} — update patch')

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

    # (2b) Pin the firmware console + GRUB to the modest PcdVideo*Resolution the .dsc already
    #      sets (1280x800) instead of the host display's full size. VirtioGpuDxe's GopInitialize
    #      overwrites PcdVideoHorizontal/VerticalResolution with the native display-info size
    #      whenever PcdVideoResolutionSource == 0 (its default). In host-matched display mode the
    #      guest boots at the screen size (e.g. 2560x1440), so the firmware console — and GRUB,
    #      which inherits the firmware's GOP mode (verified: no re-modeset through GRUB) — runs at
    #      that full size and draws a tiny menu centered in a huge black framebuffer. The host
    #      compositor can't upscale that (it can't tell the menu from GRUB's own black padding).
    #      Marking the source platform-set (1) stops the clobber, so firmware/GRUB stay at the
    #      modest resolution and the host aspect-fits/upscales them to fill the window. The guest
    #      KERNEL is unaffected: virtio-gpu-drm queries display-info directly and still modesets to
    #      the full screen for a crisp desktop (stock and enhanced alike — this is host-side, tier
    #      agnostic). Value 1 is exactly what OvmfPkg/PlatformDxe sets when the user picks a
    #      resolution in Setup, so the guard in VirtioGpuDxe/QemuVideoDxe (== 0) is the contract.
    d = open(dsc).read()
    if 'PcdVideoResolutionSource' not in d:
        res_anchor = '  gEfiMdeModulePkgTokenSpaceGuid.PcdVideoVerticalResolution|800\n'
        assert res_anchor in d, 'PcdVideoVerticalResolution anchor missing in ArmVirtKrun.dsc'
        d = d.replace(
            res_anchor,
            res_anchor
            + '  # limina: keep the firmware/GRUB console at the modest resolution above; the\n'
            + '  # host upscales it to fill the window (VirtioGpuDxe would otherwise clobber it\n'
            + '  # with the host native size -> a tiny centered GRUB menu the host cannot upscale).\n'
            + '  gUefiOvmfPkgTokenSpaceGuid.PcdVideoResolutionSource|1\n',
            1,
        )
        open(dsc, 'w').write(d)
        print('  pinned PcdVideoResolutionSource=1 (modest firmware/GRUB console; host upscales)')

    # (3) Connect the virtio-mmio GPU in BDS *before* ConOut is populated. ArmVirt's
    #     PlatformBootManagerBeforeConsole only connects PCI displays before adding GOP
    #     handles to ConOut; our (non-PCI) virtio-mmio GOP is produced later (during
    #     EfiBootManagerConnectAll) and never enters ConOut, so the firmware/GRUB graphics
    #     console stays blank. Add an IsVirtioGpu filter + connect it before AddOutput.
    pbm = 'ArmVirtPkg/Library/PlatformBootManagerLib/PlatformBm.c'
    p = open(pbm).read()
    if 'IsVirtioGpu' not in p:
        nl = '\r\n' if '\r\n' in p else '\n'
        inc_anchor = '#include <IndustryStandard/Virtio095.h>' + nl
        assert inc_anchor in p, 'Virtio095.h include anchor missing in PlatformBm.c'
        p = p.replace(
            inc_anchor,
            inc_anchor + '#include <IndustryStandard/Virtio10.h>' + nl, 1)
        rng_tail = ('  return (BOOLEAN)(VirtIo->SubSystemDeviceId ==' + nl +
                    '                   VIRTIO_SUBSYSTEM_ENTROPY_SOURCE);' + nl + '}' + nl)
        assert rng_tail in p, 'IsVirtioRng tail anchor missing in PlatformBm.c'
        gpu_fn = (
            '\n'
            '/**\n'
            '  This FILTER_FUNCTION checks if a handle corresponds to a Virtio GPU device at\n'
            '  the VIRTIO_DEVICE_PROTOCOL level.\n'
            '**/\n'
            'STATIC\n'
            'BOOLEAN\n'
            'EFIAPI\n'
            'IsVirtioGpu (\n'
            '  IN EFI_HANDLE    Handle,\n'
            '  IN CONST CHAR16  *ReportText\n'
            '  )\n'
            '{\n'
            '  EFI_STATUS              Status;\n'
            '  VIRTIO_DEVICE_PROTOCOL  *VirtIo;\n'
            '\n'
            '  Status = gBS->HandleProtocol (\n'
            '                  Handle,\n'
            '                  &gVirtioDeviceProtocolGuid,\n'
            '                  (VOID **)&VirtIo\n'
            '                  );\n'
            '  if (EFI_ERROR (Status)) {\n'
            '    return FALSE;\n'
            '  }\n'
            '\n'
            '  return (BOOLEAN)(VirtIo->SubSystemDeviceId ==\n'
            '                   VIRTIO_SUBSYSTEM_GPU_DEVICE);\n'
            '}\n'
        ).replace('\n', nl)
        p = p.replace(rng_tail, rng_tail + gpu_fn, 1)
        add_out = '  FilterAndProcess (&gEfiGraphicsOutputProtocolGuid, NULL, AddOutput);'
        assert add_out in p, 'AddOutput anchor missing in PlatformBm.c'
        connect = (
            '  //\n'
            '  // limina: connect the virtio-mmio GPU so VirtioGpuDxe produces its GOP before\n'
            '  // ConOut is populated below; the mmio (non-PCI) GOP otherwise appears only\n'
            '  // after EfiBootManagerConnectAll() and never enters ConOut (blank console).\n'
            '  //\n'
            '  FilterAndProcess (&gVirtioDeviceProtocolGuid, IsVirtioGpu, Connect);\n'
            '\n'
        ).replace('\n', nl)
        p = p.replace(add_out, connect + add_out, 1)
        open(pbm, 'w').write(p)
        print('  patched PlatformBm.c: connect virtio-gpu into ConOut before console setup')
PY

        # (4) GOP: wire the vendored VirtioKeyboardDxe into ConIn (libkrun's virtio keyboard
        # -> typeable GRUB/firmware in the window). Standalone .py (mounted at /edk2-vendor)
        # because the bash -c heredoc mangles the backslashes in its C DEBUG strings.
        if [ \"\$GOP\" = '1' ]; then
            echo '--- wiring VirtioKeyboardDxe into ConIn (limina)'
            python3 /edk2-vendor/apply-virtio-keyboard.py
        fi

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
