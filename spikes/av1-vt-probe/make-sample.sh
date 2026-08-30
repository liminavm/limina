#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Build an AV1 clip that actually exercises the case the probe is about, and say
# so out loud rather than assuming it.
#
# The VP9 spike learned this the expensive way: `ffmpeg -auto-alt-ref` produced no
# hidden frames at any setting tried, so a hand-rolled clip silently tested only
# the easy path. SVT-AV1 uses alt-refs by default, but "by default" is not
# evidence -- so this counts frame OBUs against the frames dav1d actually
# displays. When the first number exceeds the second, the clip carries no-show
# frames and the probe's cardinality question is meaningful.
set -euo pipefail

cd "$(dirname "$0")"

DUR=${DUR:-3}
SIZE=${SIZE:-640x360}

# Motion, so the encoder has a reason to keep alt-refs around.
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "testsrc2=size=$SIZE:rate=30:duration=$DUR" \
    -c:v libsvtav1 -preset 8 -crf 40 -g 60 -y sample.mp4

# Low-overhead OBU framing: size fields present, no annexb. This is what
# VideoToolbox wants per sample and what the probe splits on temporal delimiters.
ffmpeg -hide_banner -loglevel error -i sample.mp4 -c:v copy -f obu -y sample.obu

# The av1C record, lifted from the muxer's own output. Synthesizing one from
# virgl's parsed fields is the backend's job; borrowing a correct one here keeps
# the probe about VideoToolbox rather than about our serializer.
python3 - <<'PY'
import struct, sys
blob = open("sample.mp4", "rb").read()
i = blob.find(b"av1C")
if i < 0:
    sys.exit("no av1C box in sample.mp4")
size = struct.unpack(">I", blob[i - 4:i])[0]
open("av1C.bin", "wb").write(blob[i + 4:i - 4 + size])
print(f"av1C.bin: {size - 8} bytes")
PY

# The same stream re-framed one frame per temporal unit. This is the framing the
# probe's cardinality question is really about: virgl submits one FRAME at a time,
# while a natural stream bundles a no-show frame and the frame that displays it
# into a single TU. Repeating the sequence header in every TU is legal, and is what
# a backend synthesizing its own headers would emit anyway.
python3 - <<'REFRAME'
data = open("sample.obu", "rb").read()

def walk(d):
    pos = 0
    while pos < len(d):
        h = d[pos]; typ = (h >> 3) & 0xf; cur = pos + 1 + ((h >> 2) & 1)
        v = sh = 0
        while True:
            b = d[cur]; cur += 1; v |= (b & 0x7f) << sh; sh += 7
            if not b & 0x80:
                break
        yield typ, d[pos:cur + v]
        pos = cur + v

td = seq = None
out = bytearray()
frames = 0
for typ, raw in walk(data):
    if typ == 2:
        td = raw
    elif typ == 1:
        seq = raw
    elif typ in (3, 6):
        out += td + (seq or b"") + raw
        frames += 1
    else:
        out += raw
open("sample-perframe.obu", "wb").write(bytes(out))
print(f"sample-perframe.obu: {frames} frame OBUs, one per temporal unit")
REFRAME

displayed=$(ffmpeg -hide_banner -i sample.obu -f null - 2>&1 |
    sed -n 's/.*frame= *\([0-9]*\).*/\1/p' | tail -1)

frame_obus=$(python3 - <<'PY'
data = open("sample.obu", "rb").read()
pos = frames = tus = 0
while pos < len(data):
    header = data[pos]
    typ = (header >> 3) & 0xf
    cur = pos + 1 + ((header >> 2) & 1)
    value = shift = 0
    while True:
        byte = data[cur]; cur += 1
        value |= (byte & 0x7f) << shift; shift += 7
        if not byte & 0x80:
            break
    if typ in (3, 6):
        frames += 1
    if typ == 2:
        tus += 1
    pos = cur + value
print(frames)
PY
)

echo "frame OBUs in stream : $frame_obus"
echo "frames displayed     : $displayed"
if [ "$frame_obus" -gt "$displayed" ]; then
    echo "VERDICT: clip carries $((frame_obus - displayed)) no-show frame(s) -- the hard case is present"
else
    echo "VERDICT: NO no-show frames; this clip tests only the easy path, do not conclude from it"
fi

echo
echo "the two framings answer different questions -- run both:"
echo "  ./av1-vt-probe sample.obu av1C.bin           # natural framing, TU-level"
echo "  ./av1-vt-probe sample-perframe.obu av1C.bin  # per-frame, the unit virgl uses"
