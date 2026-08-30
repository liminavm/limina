#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Encode a spread of AV1 clips for the frame-header serializer's fixtures.
#
# Encoder flags are a request, not a result: whether a clip actually carries global
# motion, segmentation or no-show frames is the encoder's decision, and asking for a
# feature is not evidence of getting it. So this script only *encodes*. What the clips
# turn out to exercise is measured afterwards, from the descriptors the guest really
# produced, by ./coverage. That ordering is deliberate -- the VP9 spike lost time to
# `-auto-alt-ref` silently producing no hidden frames, leaving only the easy path tested.
set -euo pipefail

cd "$(dirname "$0")"
mkdir -p clips

DUR=${DUR:-2}
RATE=${RATE:-30}

# A moving synthetic pattern: motion the encoder can find, and sharp edges that make a
# wrong header obvious in the pixels rather than subtly off.
SRC="testsrc2=size=640x360:rate=$RATE:duration=$DUR"
# A whole-frame pan, which is what makes an encoder reach for global motion.
PAN="color=c=black:size=1280x720:rate=$RATE:duration=$DUR"

enc() { # enc <name> <input-filter> [extra svtav1 params...]
    local name=$1 filt=$2; shift 2
    local params=""
    [ $# -gt 0 ] && params="-svtav1-params $(IFS=:; echo "$*")"
    echo "==> $name"
    # shellcheck disable=SC2086
    ffmpeg -hide_banner -loglevel error -f lavfi -i "$filt" \
        -c:v libsvtav1 -preset 6 -crf 40 -g 60 $params -y "clips/$name.mp4"
    ffmpeg -hide_banner -loglevel error -i "clips/$name.mp4" -c:v copy -f obu -y "clips/$name.obu"
}

enc baseline    "$SRC"
enc filmgrain   "$SRC"                                film-grain=30
enc tiles       "$SRC"                                tile-columns=2:tile-rows=1
enc superres    "$SRC"                                superres-mode=2
enc lowdelay    "$SRC"                                pred-struct=1
# A rigid pan over detailed content, the strongest inducement to global motion.
enc pan         "${PAN},noise=alls=40:allf=t+u,crop=640:360:'min(t*60,640)':180"

echo
echo "clips:"
ls -la clips/*.obu
echo
echo "Next: boot a poke VM with LIMINA_AV1_CAPTURE set, play each clip through ffmpeg's"
echo "VA-API decoder in the guest, then run ./coverage over the captured descriptors."
