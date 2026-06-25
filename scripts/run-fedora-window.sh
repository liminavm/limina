#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the real Fedora guest in a native limina window — the M2 goal: Fedora to GDM in an
# NSWindow, driven by host keyboard/mouse, on the tier-1 software-2D display.
#
# Boots via the EFI firmware path (EDK2 .fd) off the Fedora raw disk. The disk is booted
# WRITABLE (a desktop can't reach GDM with a read-only root), but to keep the pristine
# image untouched we boot an APFS clone (`cp -c` — instant, copy-on-write) made fresh each
# run, so every launch starts from the same clean state and the original is never written.
#
# Usage: scripts/run-fedora-window.sh [debug|release]
#   Env: LIMINA_FEDORA_IMAGE (default ./Fedora-Workstation-43.raw),
#        LIMINA_FIRMWARE (default: our GOP firmware target/krun-efi/KRUN_EFI.gop.fd if built —
#                       so EFI/GRUB also render in the window — else krunkit's silent .fd),
#        LIMINA_FEDORA_RAM_MIB (default 6144), LIMINA_FEDORA_CPUS (default 4),
#        LIMINA_DISPLAY_SIZE (default 1280x800).
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
CARGO_FLAGS=()
[ "$PROFILE" = "release" ] && CARGO_FLAGS=(--release)

# Default to the clean pristine-stock base, which EFI-boots straight to GDM (Fedora-built labels
# intact, no relabel loop) with a `claude` user + autologin already set up. Override with
# LIMINA_FEDORA_IMAGE (e.g. Fedora-Workstation-43.enhanced.raw for the venus tier).
if [ -n "${LIMINA_FEDORA_IMAGE:-}" ]; then
    IMAGE="$LIMINA_FEDORA_IMAGE"
else
    IMAGE="Fedora-Workstation-43.accessible.raw"
    echo "==> using $IMAGE (clean pristine-stock base + user/autologin) — one clean boot to GDM"
fi
# Prefer our GOP firmware (VirtioGpuDxe + ConOut patch) so EFI/GRUB render in the window too;
# fall back to krunkit's silent .fd (serial-only firmware) if it hasn't been built yet.
GOP_FIRMWARE="target/krun-efi/KRUN_EFI.gop.fd"
if [ -n "${LIMINA_FIRMWARE:-}" ]; then
    FIRMWARE="$LIMINA_FIRMWARE"
elif [ -f "$GOP_FIRMWARE" ]; then
    FIRMWARE="$GOP_FIRMWARE"
    echo "==> using GOP firmware ($GOP_FIRMWARE) — EFI/GRUB will render in the window"
else
    FIRMWARE="/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd"
    echo "==> using silent firmware (no GOP); build it for a graphical boot console:" >&2
    echo "    GOP=1 scripts/build-krun-efi.sh   (then re-run; or set LIMINA_FIRMWARE)" >&2
fi
SCRATCH="Fedora-Workstation-43.scratch.raw"   # *.raw is gitignored
RAM="${LIMINA_FEDORA_RAM_MIB:-6144}"
CPUS="${LIMINA_FEDORA_CPUS:-4}"
SIZE="${LIMINA_DISPLAY_SIZE:-1280x800}"

[ -f "$IMAGE" ]    || { echo "Fedora image not found: $IMAGE (set LIMINA_FEDORA_IMAGE)" >&2; exit 1; }
[ -f "$FIRMWARE" ] || { echo "firmware not found: $FIRMWARE (set LIMINA_FIRMWARE)" >&2; exit 1; }

echo "==> building limina + limina-vmm ($PROFILE)"
cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -p limina -p limina-vmm

echo "==> codesigning the worker (hypervisor entitlement)"
crates/limina-vmm/sign.sh "$PROFILE"

echo "==> cloning $IMAGE -> $SCRATCH (APFS copy-on-write; original stays pristine)"
rm -f "$SCRATCH"
cp -c "$IMAGE" "$SCRATCH"

echo "==> launching Fedora in a limina window ($CPUS vcpu, ${RAM}MiB, $SIZE; close window or Ctrl-C to quit)"
echo "    interactive serial console: watch for 'limina: interactive serial console at /dev/ttysNNN'"
echo "    then, in another terminal:  screen /dev/ttysNNN   (drives EDK2/GRUB; a login shell with console=ttyAMA0)"
exec "target/$PROFILE/limina" --window \
    --firmware "$FIRMWARE" \
    --disk "$SCRATCH" \
    --cpus "$CPUS" \
    --ram-mib "$RAM" \
    --display-size "$SIZE" \
    --console-pty
