#!/usr/bin/env bash
# Verify the H.264 parameter-set serializer against a real stream.
#
#   ./verify.sh <clip.264> <width> <height>
#
# Reads the stream's OWN SPS/PPS field values with ffmpeg's trace_headers, feeds those
# (and only those) to the serializer, splices the result in place of the stream's real
# parameter sets, and decodes both. H.264 is normatively exact, so the frame hashes must
# match exactly -- anything else means the synthesized sets configure the decoder
# differently from the encoder's own.
#
# Deliberately NOT extracted and handed over: pic_width_in_mbs_minus1,
# pic_height_in_map_units_minus1 and the frame_crop_* values. The serializer derives those
# from the display size, so passing them in would test nothing.
set -euo pipefail
cd "$(dirname "$0")"

CLIP="${1:?usage: verify.sh <clip.264> <width> <height>}"
W="${2:?width}"
H="${3:?height}"
NAME="$(basename "$CLIP" .264)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# One trace, split into the SPS and PPS halves so a name appearing in both (e.g.
# seq_parameter_set_id) cannot collide.
ffmpeg -loglevel trace -i "$CLIP" -c copy -bsf:v trace_headers -f null - 2>&1 |
  awk '
    /Sequence Parameter Set/ { sec="sps_"; next }
    /Picture Parameter Set/  { sec="pps_"; next }
    /Slice/                  { sec="";     next }
    sec != "" && /= *-?[0-9]+$/ {
      # "[trace_headers @ 0x..] 24   level_idc    00011110 = 30"
      for (i = 1; i <= NF; i++) if ($i ~ /^[a-z_][a-z0-9_]*$/) { name = $i; break }
      if (name != "" && !seen[sec name]++) print sec name "=" $NF
      name = ""
    }
  ' > "$WORK/fields"

echo "=== $NAME (${W}x${H})"
echo "  profile_idc $(grep -E '^sps_profile_idc=' "$WORK/fields" | cut -d= -f2)," \
     "poc_type $(grep -E '^sps_pic_order_cnt_type=' "$WORK/fields" | cut -d= -f2)," \
     "cabac $(grep -E '^pps_entropy_coding_mode_flag=' "$WORK/fields" | cut -d= -f2)," \
     "stream crop_bottom $(grep -E '^sps_frame_crop_bottom_offset=' "$WORK/fields" | cut -d= -f2 || echo 0)"

./synth "$WORK/fields" "$W" "$H" "$WORK/ps.bin"

ffmpeg -v error -i "$CLIP" -c copy -bsf:v "filter_units=remove_types=7|8" -f h264 -y "$WORK/slices.264"
cat "$WORK/ps.bin" "$WORK/slices.264" > "$WORK/ours.264"

ffmpeg -v error -i "$CLIP"        -f framemd5 -y "$WORK/ref.md5"
ffmpeg -v error -i "$WORK/ours.264" -f framemd5 -y "$WORK/ours.md5"

grep -v '^#' "$WORK/ref.md5"  > "$WORK/a"
grep -v '^#' "$WORK/ours.md5" > "$WORK/b"

if [ ! -s "$WORK/a" ]; then
  echo "  FAIL: the reference decoded no frames"; exit 1
fi
if diff -q "$WORK/a" "$WORK/b" >/dev/null; then
  echo "  PASS: $(wc -l < "$WORK/a" | tr -d ' ') frames, bit-exact"
else
  echo "  FAIL: $(diff "$WORK/a" "$WORK/b" | grep -c '^<') frames differ"
  diff "$WORK/a" "$WORK/b" | head -4
  exit 1
fi
