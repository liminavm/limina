#!/usr/bin/env bash
# Verify the HEVC parameter-set serializer against a real stream.
#
#   ./verify.sh <clip.265> <width> <height>
#
# Reads the stream's OWN VPS/SPS/PPS field values with ffmpeg's trace_headers, feeds those
# (and only those) to the serializer, splices the synthesized sets in place of the real
# ones, and decodes both. HEVC is normatively exact, so the frame hashes must match.
#
# Deliberately NOT extracted and handed over -- these are what the backend must invent:
# the entire VPS, general_level_idc, the conf_win_* offsets, and the contents of every
# short_term_ref_pic_set.
set -euo pipefail
cd "$(dirname "$0")"

CLIP="${1:?usage: verify.sh <clip.265> <width> <height>}"
W="${2:?width}"
H="${3:?height}"
NAME="$(basename "$CLIP" .265)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

ffmpeg -loglevel trace -i "$CLIP" -c copy -bsf:v trace_headers -f null - 2>&1 |
  awk '
    /Video Parameter Set/    { sec="vps_"; next }
    /Sequence Parameter Set/ { sec="sps_"; next }
    /Picture Parameter Set/  { sec="pps_"; next }
    /Slice Segment Header/   { sec="";     next }
    sec != "" && /= *-?[0-9]+$/ {
      # Names may be subscripted -- sps_max_dec_pic_buffering_minus1[0]. Keep index 0 and
      # drop the rest, rather than letting the regex reject the whole line: silently
      # dropping a field means the serializer is fed a DEFAULT, and a default that happens
      # to be plausible produces a mismatch that looks like a serializer bug.
      name = ""
      for (i = 1; i <= NF; i++) if ($i ~ /^[a-z_][a-z0-9_]*(\[[0-9]+\])?$/) { name = $i; break }
      if (name ~ /\[/) {
        if (name !~ /\[0\]$/) next
        sub(/\[0\]$/, "", name)
      }
      if (name != "" && !seen[sec name]++) print sec name "=" $NF
      name = ""
    }
  ' > "$WORK/fields"

echo "=== $NAME (${W}x${H})"
echo "  coded $(grep -E '^sps_pic_width_in_luma_samples=' "$WORK/fields" | cut -d= -f2)x$(grep -E '^sps_pic_height_in_luma_samples=' "$WORK/fields" | cut -d= -f2)," \
     "ref pic sets $(grep -E '^sps_num_short_term_ref_pic_sets=' "$WORK/fields" | cut -d= -f2)," \
     "sao $(grep -E '^sps_sample_adaptive_offset_enabled_flag=' "$WORK/fields" | cut -d= -f2)," \
     "amp $(grep -E '^sps_amp_enabled_flag=' "$WORK/fields" | cut -d= -f2)"

./synth "$WORK/fields" "$W" "$H" "$WORK/ps.bin"

ffmpeg -v error -i "$CLIP" -c copy -bsf:v "filter_units=remove_types=32|33|34" -f hevc -y "$WORK/slices.265"
cat "$WORK/ps.bin" "$WORK/slices.265" > "$WORK/ours.265"

ffmpeg -v error -i "$CLIP"          -f framemd5 -y "$WORK/ref.md5"
ffmpeg -v error -i "$WORK/ours.265" -f framemd5 -y "$WORK/ours.md5"

grep -v '^#' "$WORK/ref.md5"  > "$WORK/a"
grep -v '^#' "$WORK/ours.md5" > "$WORK/b"

if [ ! -s "$WORK/a" ]; then echo "  FAIL: the reference decoded no frames"; exit 1; fi
if diff -q "$WORK/a" "$WORK/b" >/dev/null; then
  echo "  PASS: $(wc -l < "$WORK/a" | tr -d ' ') frames, bit-exact"
else
  echo "  FAIL: $(diff "$WORK/a" "$WORK/b" | grep -c '^<') frames differ"
  diff "$WORK/a" "$WORK/b" | head -4
  exit 1
fi
