#!/usr/bin/env bash
# Width sweep for the vrend/KK unaligned-pitch shear (spikes/vrend-stride-2026-08-14/NOTES.md).
#
# Drives a GL client (glmark2-wayland -> virgl/vrend) at a series of exact BUFFER widths in a
# guest running synoik (a Vulkan compositor -> venus), and counts the host-side
# `[KK-MODIFIER] ... unusable` substitutions each width produces.
#
# The prediction under test: a substitution happens exactly when the client's row pitch
# (width * 4 for XRGB8888) is not a multiple of 16, because KosmicKrisp requires 16-byte
# row alignment (kk_image_layout.c:264) and silently uses its own pitch otherwise
# (kk_image.c:665).
#
# Why the compositor must be synoik and not mutter: the rejected pitch arrives through
# VkImageDrmFormatModifierExplicitCreateInfoEXT, i.e. a *Vulkan* import of the client's
# dmabuf (testcomp/src/vk.rs:467, transcribed from synoik). Mutter composites with GL, so the
# buffer never crosses into a Vulkan import and the path is never exercised -- a mutter run
# produces zero substitutions at every width and is a FALSE NEGATIVE, not a refutation.
#
# Usage: width-sweep.sh <ssh-port> <host-worker-log> [widths...]
set -euo pipefail

PORT="${1:?usage: width-sweep.sh <ssh-port> <worker-log> [widths...]}"
LOG="${2:?usage: width-sweep.sh <ssh-port> <worker-log> [widths...]}"
shift 2
WIDTHS=("$@")
[ ${#WIDTHS[@]} -eq 0 ] && WIDTHS=(1972 1974 1975 1976 1978 1980)

SSH=(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -n -p "$PORT" claude@127.0.0.1)

printf '%-8s %-8s %-7s %-9s %s\n' width pitch mod16 predicted observed
for w in "${WIDTHS[@]}"; do
    pitch=$((w * 4))
    mod=$((pitch % 16))
    if [ "$mod" -eq 0 ]; then predicted=clean; else predicted=SHEAR; fi

    # `grep -c` PRINTS 0 and EXITS 1 when there are no matches. Two traps in one line:
    # a `|| echo 0` fallback yields the string "0\n0" and the next arithmetic dies, and
    # under `set -o pipefail` the nonzero grep fails the whole pipeline so `set -e` aborts
    # the script before the first width ever runs (it printed only the header, twice).
    before=$({ grep -c "KK-MODIFIER" "$LOG" 2>/dev/null || true; } | head -1)
    # Foreground, stderr kept: a driver loop that hides stderr once drove a window to 55360 px
    # and measured a workload that had already died (wallpaper-backdrop-leak RESULTS 9).
    # DISCOVER the Wayland socket, never assume wayland-0: synoik comes up on wayland-1 (gdm
    # holds -0), and a hardcoded -0 means the client never connects, the buffer is never
    # imported, and every width reads "clean" -- a false negative across the whole sweep.
    "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000; \
        export WAYLAND_DISPLAY=\$(basename \$(ls /run/user/1000/wayland-* | grep -v lock | head -1)); \
        echo \"socket=\$WAYLAND_DISPLAY\"; \
        timeout 12 glmark2-wayland -s ${w}x1000 -b build 2>&1 | grep -iE 'error|Surface Size|FPS' || true" \
        > "/tmp/sweep-${w}.txt" 2>&1 || true
    sleep 2
    after=$({ grep -c "KK-MODIFIER" "$LOG" 2>/dev/null || true; } | head -1)
    delta=$((after - before))

    if [ "$delta" -gt 0 ]; then observed="SHEAR ($delta lines)"; else observed="clean (0)"; fi
    printf '%-8s %-8s %-7s %-9s %s\n' "$w" "$pitch" "$mod" "$predicted" "$observed"
    if grep -qi error "/tmp/sweep-${w}.txt"; then
        echo "    !! client error at width $w (buffer may never have been imported):"
        sed 's/^/       /' "/tmp/sweep-${w}.txt" | head -3
    fi
done
