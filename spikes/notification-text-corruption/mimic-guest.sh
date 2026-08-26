#!/usr/bin/env bash
# Run glyphmimic in the guest, on virgl -- the other half of the 2x2 the vehicle exists for.
#
#   mimic-guest.sh <ssh-port> [episodes]
#
# Copies the source in and builds it there against the guest's own EGL/GLES, so the draw stream
# reaching vrend is the guest mesa's, exactly as gnome-shell's is.
#
# Pair every run with calib.sh posts in the SAME session, before and after. Incidence is
# session-unstable, so a clean mimic in a session that had quietly stopped damaging proves
# nothing; a clean mimic beside a damaging real card is the strongest negative this rig can give.
#
# THE POSITIVE CONTROL RUNS FIRST AND IS NOT OPTIONAL. The oracle was proven on the host's
# zink-on-KosmicKrisp; virgl is a different readback path and inherits nothing from that proof. If
# GM_NODRAW does not score text-lost on every episode here, every clean verdict below it is
# unfalsifiable and means nothing.
set -eu
cd "$(dirname "$0")"
PORT="${1:?ssh port}"
EP="${2:-90}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/gm-%r@%h:%p -o ControlPersist=600 claude@127.0.0.1)

scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    glyphmimic.c claude@127.0.0.1:/tmp/ >/dev/null
"${SSH[@]}" 'cc -O2 -Wall /tmp/glyphmimic.c -lEGL -lGLESv2 -o /tmp/glyphmimic' \
    || { echo "guest build failed (needs mesa-libEGL-devel / mesa-libGLES-devel)"; exit 1; }

echo "--- renderer + positive control (must be text-lost on every episode) ---"
"${SSH[@]}" "GM_NODRAW=1 /tmp/glyphmimic $EP" 2>&1 | grep -E 'glyphmimic:|VERDICT'

echo "--- arms ---"
# The default arm is now the FAITHFUL one -- the card as the vrend trace measures it. Every other
# arm is SUBTRACTIVE: it removes one measured ingredient. That direction matters. Adding one
# property at a time to a stripped mimic never reproduced anything; if the faithful arm does
# reproduce, these say which ingredient it needed.
for arm in "" GM_NODEPTH=1 GM_FRAMES=1 GM_FRAMES=6 GM_WIDE=1 GM_COMPOSITES=0 \
           GM_GAP_MS=0 GM_PRESENT=1 GM_U16=1 GM_NOSTRIDE0=1 GM_FINISH=1 GM_FLUSH=1; do
    printf '%-22s ' "${arm:-baseline}"
    "${SSH[@]}" "env $arm /tmp/glyphmimic $EP" 2>&1 | grep VERDICT || echo "(no verdict)"
done

echo "--- repetition: 40 runs, the real incidence was session-unstable ---"
"${SSH[@]}" "bad=0; for i in \$(seq 1 40); do v=\$(env GM_ATLASUP=1 GM_PAD=8 /tmp/glyphmimic $EP 2>&1 | grep VERDICT); \
  case \"\$v\" in *'text-lost=0 blank=0 other=0'*) ;; *) bad=\$((bad+1)); echo \"run \$i: \$v\";; esac; done; \
  echo \"guest repetition: \$bad non-clean of 40 runs\""
