#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the L1 test guest: a kernel Image + a rootfs directory containing our limina-init.
#
# Kernel: for now we reuse libkrunfw's bundled aarch64 kernel (extracted straight from
# the dylib's __data section — it's a raw arm64 Image). It has NO CONFIG_BLK_DEV_INITRD
# (so no initramfs boot) but DOES have virtio-fs, and mounts its root over virtiofs —
# so the rootfs is just a host directory, no mkfs/image needed. A custom shallow-clone
# kernel build (for 16 KiB pages / GPU / balloon / initramfs) is a later enhancement that
# needs a Linux build env; this gets L1 running today with no container or C toolchain.
# rootfs: our pure-Rust static limina-init as /init (cross-built with rust-lld), served as
# the guest root via virtio-fs (tag /dev/root, cmdline rootfstype=virtiofs).
#
# Output (gitignored, under target/): target/test-guest/{Image,rootfs/}
# Usage: scripts/build-test-guest.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="target/test-guest"
mkdir -p "$OUT"

DYL="$(/opt/homebrew/bin/brew --prefix libkrunfw 2>/dev/null || echo /opt/homebrew/opt/libkrunfw)/lib/libkrunfw.dylib"
[ -f "$DYL" ] || { echo "libkrunfw dylib not found at $DYL" >&2; exit 1; }

echo "==> extracting bundled aarch64 kernel Image from $(basename "$DYL")"
# The kernel is the dylib's __data section (symbol _KERNEL_BUNDLE). Pull its exact
# file offset + size from the Mach-O headers so this survives libkrunfw version bumps.
DATA_OFF=$(otool -l "$DYL" | awk '$1=="sectname"{s=$2} s=="__data"&&$1=="offset"{print $2; exit}')
DATA_SIZE_HEX=$(otool -l "$DYL" | awk '$1=="sectname"{s=$2} s=="__data"&&$1=="size"{print $2; exit}')
DATA_SIZE=$(( DATA_SIZE_HEX ))
[ -n "$DATA_OFF" ] && [ "$DATA_SIZE" -gt 0 ] || { echo "could not locate __data in $DYL" >&2; exit 1; }
# `head -c` closes the pipe early → SIGPIPE on tail; that's expected, so don't let
# pipefail abort the script for it.
set +o pipefail
tail -c "+$((DATA_OFF + 1))" "$DYL" | head -c "$DATA_SIZE" > "$OUT/Image"
set -o pipefail

# Sanity: arm64 Image magic "ARM\x64" lives at byte offset 56 of the Image.
MAGIC=$(dd if="$OUT/Image" bs=1 skip=56 count=4 2>/dev/null)
[ "$MAGIC" = "ARMd" ] || { echo "extracted Image lacks arm64 magic (got '$MAGIC')" >&2; exit 1; }
echo "    -> $OUT/Image ($(wc -c < "$OUT/Image") bytes, arm64 magic OK)"

echo "==> building limina-init (aarch64-unknown-linux-musl, static, rust-lld)"
( cd guest/limina-init && cargo build --release )
INIT="guest/limina-init/target/aarch64-unknown-linux-musl/release/limina-init"
[ -x "$INIT" ] || { echo "limina-init not built at $INIT" >&2; exit 1; }

echo "==> staging rootfs (/init + /dev mountpoint) for virtio-fs root"
ROOTFS="$OUT/rootfs"
rm -rf "$ROOTFS"
mkdir -p "$ROOTFS/dev"
install -m 0755 "$INIT" "$ROOTFS/init"   # limina-init mounts devtmpfs on /dev itself
echo "    -> $ROOTFS (init $(wc -c < "$ROOTFS/init") bytes)"

echo "==> L1 guest ready:"
echo "    kernel = $OUT/Image"
echo "    rootfs = $ROOTFS  (boot with: rootfstype=virtiofs rw init=/init, tag /dev/root)"
