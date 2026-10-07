#!/bin/sh
# Build both halves into out/: the codesigned VZ host (macOS 27 SDK) and the initramfs.
# Builds on any Mac with the macOS 27 SDK; the result runs only on macOS 27.
set -e
here=$(cd "$(dirname "$0")" && pwd)
out="$here/out"
mkdir -p "$out"
(cd "$here/guest" && cargo build --release --quiet)
python3 -I "$here/mkinitramfs.py" "$here/guest/target/aarch64-unknown-linux-musl/release/vzprobe-guest" "$out/initramfs.cpio"
# vzprobe runs everything (macOS 27); vzprobe26 is the same source with a macOS 26 floor, so the
# Apple-device baselines can be measured on a 26 host (the custom device stays off there).
for v in 27 26; do
    name=vzprobe; [ $v = 26 ] && name=vzprobe26
    xcrun --sdk macosx clang -O2 -fobjc-arc -mmacosx-version-min=$v.0 -Wall -Wno-unused-function \
        -Wno-unguarded-availability-new \
        -framework Foundation -framework Virtualization -framework Metal -framework IOSurface \
        -o "$out/$name" "$here/host/vzprobe.m"
    codesign -f -s - --entitlements "$here/host/vzprobe.entitlements" "$out/$name"
done
echo "built: $out/vzprobe $out/vzprobe26 $out/initramfs.cpio"
