#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Fetch a VP9 clip that actually contains hidden alt-ref frames.
#
# This is harder to come by than it sounds: `ffmpeg -c:v libvpx-vp9 -auto-alt-ref`
# produced none at any setting tried, so a hand-rolled clip silently tests the easy
# case. libvpx's own aq2 test vector has 7 superframes carrying 7 show_frame=0
# frames, which is the case the backend must not drop.
set -euo pipefail

cd "$(dirname "$0")"

BASE=https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx
VECTOR=vp90-2-09-aq2.webm

[ -f "$VECTOR" ] || curl -fsSL -o "$VECTOR" "$BASE/$VECTOR"

# IVF, because it frames the stream for us; the guest's decoder does the same
# splitting before VA-API ever sees a frame.
ffmpeg -hide_banner -loglevel error -i "$VECTOR" -c:v copy -f ivf hidden-frames.ivf -y

echo "wrote hidden-frames.ivf"
