#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Drive the WebGL-aquarium benchmark across fish counts and harvest a readable fps counter for each,
# with NO human in the loop.
#
# THE POINT. The aquarium's fps is only ever drawn on screen — there is no log line, no exit code, no
# file. Historically someone had to look at the window. This script closes that loop using the
# supervisor's own frame dump (LIMINA_WINDOW_CAPTURE), so an agent can run the sweep unattended and
# only needs to *read* the small cropped PNGs at the end.
#
# PREREQUISITES — the VM must ALREADY be booted with:
#   LIMINA_WINDOW_CAPTURE=/tmp/limina-capture.png   (the supervisor overwrites this PNG with the
#                                                    presented scanout roughly every 2 s)
#   a PINNED display (see scripts/perf/set-guest-display.py — an unpinned match-host boot gives a
#   fractional scale, which moves the counter and changes the workload)
# e.g.
#   LIMINA_DISK=perf-f43-enh.raw LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 \
#   LIMINA_WINDOW_CAPTURE=/tmp/limina-capture.png \
#   LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
#     spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &
#
# Usage:
#   scripts/perf/aquarium-run.sh <tier-label> [fish counts...]
#   scripts/perf/aquarium-run.sh venus 5000 10000 15000
# Env: LIMINA_SSH_PORT (default 2222), LIMINA_CAPTURE (default /tmp/limina-capture.png),
#      AQ_SETTLE (seconds to let each fish count settle before capturing, default 35),
#      AQ_OUT (evidence dir, default perf/evidence/aquarium-<today>),
#      AQ_EXTRA_ENV (extra --setenv=K=V args for the firefox unit). The software-2D tier needs
#        AQ_EXTRA_ENV="--setenv=LIBGL_ALWAYS_SOFTWARE=1": with no 3D device a GL client does NOT
#        fall back to llvmpipe on its own, it tries the absent virtio-gpu native-context path and
#        fails to get a context at all.
#
# Output: for each fish count, a full-frame PNG and a cropped+upscaled `*-fps.png`. READ the crop.
set -euo pipefail

TIER="${1:?usage: aquarium-run.sh <tier-label> [fish...]}"; shift
FISH=("$@"); [ ${#FISH[@]} -gt 0 ] || FISH=(5000 10000 15000)

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
PORT="${LIMINA_SSH_PORT:-2222}"
CAP="${LIMINA_CAPTURE:-/tmp/limina-capture.png}"
SETTLE="${AQ_SETTLE:-35}"
OUT="${AQ_OUT:-perf/evidence/aquarium-$(date +%Y-%m-%d)}"
mkdir -p "$OUT"

SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1)

[ -f "$CAP" ] || { echo "no capture at $CAP — was the VM booted with LIMINA_WINDOW_CAPTURE=$CAP?" >&2; exit 1; }

for n in "${FISH[@]}"; do
  echo "=== $TIER / numFish=$n ==="
  # MOZ_DISABLE_GPU_SANDBOX=1 is MANDATORY on the GPU tiers: without it Firefox's GPU process cannot
  # reach the virtio-gpu device and NO WINDOW EVER MAPS (cost a long session once). `procs=1` is
  # normal with it. Stop any prior unit first and let the compositor settle before relaunching.
  "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
    systemctl --user stop ff-bench 2>/dev/null || true
    sleep 3
    busctl --user set-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive b false 2>/dev/null || true
    systemd-run --user --unit=ff-bench \
      --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=MOZ_ENABLE_WAYLAND=1 \
      --setenv=MOZ_DISABLE_GPU_SANDBOX=1 --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
      ${AQ_EXTRA_ENV:-} \
      /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=$n' >/dev/null"

  echo "  settling ${SETTLE}s…"
  sleep "$SETTLE"

  full="$OUT/$TIER-$n.png"
  # Capture-until-valid. Two independent hazards, both seen for real on 2026-07-26:
  #   1. TORN FILE — the supervisor rewrites $CAP in place, so a plain cp can catch it half-written
  #      and PIL fails with UnidentifiedImageError.
  #   2. BLANK FRAME — the settle time is not enough on the FIRST launch (Firefox cold start), so the
  #      dump is of a page that has not painted; the crop is black and carries no fps at all.
  # Both are fixed the same way: require the mtime to advance (a genuinely new dump), then demand the
  # crop decode AND contain bright text, retrying otherwise. Silent failure here is the dangerous
  # one — a stale or blank frame reads as a legitimate data point.
  ok=0
  for attempt in $(seq 1 12); do
    before=$(stat -f %m "$CAP")
    for _ in $(seq 1 30); do
      [ "$(stat -f %m "$CAP")" -gt "$before" ] && break
      sleep 1
    done
    sleep 0.5   # let the in-place rewrite finish before reading it
    cp "$CAP" "$full"
    if scripts/perf/crop-fps.py "$full" "$OUT/$TIER-$n-fps.png" --require-content; then
      ok=1; break
    fi
    echo "  capture attempt $attempt unusable, retrying…"
    sleep 5
  done
  [ "$ok" = 1 ] || { echo "  FAILED to capture a usable frame for $TIER/$n" >&2; continue; }
  echo "  wrote $full and $OUT/$TIER-$n-fps.png"
done

"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; systemctl --user stop ff-bench 2>/dev/null || true" || true
echo
echo "READ these crops to get the fps numbers:"
ls -1 "$OUT"/"$TIER"-*-fps.png
