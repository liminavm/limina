#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Does a frame uploaded through GL actually reach the GPU, and come back right?
#
# Run this IN THE GUEST. It pushes videotestsrc's SMPTE pattern through
# glupload (which, on a guest whose buffers qualify, takes the udmabuf/dmabuf
# import path) and reads it back, once per pixel format.
#
# The scaling step is load-bearing: `glupload ! gldownload` at identical caps
# issues no transfer and no draw, so it never samples the host texture and
# reports "fine" no matter how broken the import is. glcolorscale to a
# DIFFERENT size forces a real draw. (That mistake cost a round of measurements
# on 2026-08-23 — do not "simplify" the pipeline.)
#
# A blank frame is ~1 kB; a correct 640x360 SMPTE pattern is tens of kB. The
# comparison that matters is against the same pipeline with no GL in it, which
# this prints as the reference.
#
# Usage: gl-upload-oracle.sh [WIDTHxHEIGHT] [format ...]
set -u
SIZE="${1:-1280x720}"; shift || true
W="${SIZE%x*}"; H="${SIZE#*x}"
OW=$((W / 2)); OH=$((H / 2))
FORMATS=("$@")
[ ${#FORMATS[@]} -eq 0 ] && FORMATS=(RGBA NV12 I420)

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

gst-launch-1.0 -q videotestsrc num-buffers=1 \
    ! "video/x-raw,format=RGBA,width=$OW,height=$OH" \
    ! videoconvert ! pngenc ! multifilesink location="$OUT/ref.png" >/dev/null 2>&1
echo "reference (no GL): $(stat -c %s "$OUT/ref.png" 2>/dev/null) bytes"

for F in "${FORMATS[@]}"; do
    gst-launch-1.0 -q videotestsrc num-buffers=1 \
        ! "video/x-raw,format=$F,width=$W,height=$H" \
        ! glupload ! glcolorconvert ! 'video/x-raw(memory:GLMemory),format=RGBA' \
        ! glcolorscale ! "video/x-raw(memory:GLMemory),width=$OW,height=$OH" \
        ! gldownload ! videoconvert ! pngenc \
        ! multifilesink location="$OUT/$F.png" >"$OUT/$F.log" 2>&1
    SZ="$(stat -c %s "$OUT/$F.png" 2>/dev/null || echo 0)"
    printf '%-6s %8s bytes  md5 %s\n' "$F" "$SZ" \
        "$(md5sum "$OUT/$F.png" 2>/dev/null | cut -c1-12)"
done

echo
echo "Sizes near the reference mean the pattern came back; ~1 kB means blank."
echo "Colours still need an eye: a luma-only result is the right shape in red."
