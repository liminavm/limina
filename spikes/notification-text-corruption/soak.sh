#!/usr/bin/env bash
# Raise the incidence of the card-content loss and detect it without a human in the loop.
#
# The event is rare for everyone, so this stacks the levers rather than looking for a magic
# sequence: notifications that carry a SYMBOLIC SOURCE ICON (each such icon is an extra
# ClutterOffscreenEffect FBO on top of the per-StLabel FBOs), a fixed card count so the panel's
# total ink is comparable between samples, repeated open/close of the menu to force many more FBO
# renders per iteration than simply adding a card, and a sample taken DURING the open animation as
# well as after it, since a bad frame can be re-dirtied and healed.
#
#   soak.sh <ssh-port> <iterations> <outdir>
set -u
PORT="${1:?ssh port}"; N="${2:-50}"; OUT="${3:-soak}"
HERE="$(cd "$(dirname "$0")" && pwd)"
RECT="${RECT:-830,55,540,760}"      # the clock menu's notification-list panel, in scanout pixels
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/soak-%r@%h:%p -o ControlPersist=300 claude@127.0.0.1)
GE="export YDOTOOL_SOCKET=/tmp/.ydotool_socket;
    export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;"
mkdir -p "$OUT"; : >"$OUT/ink.tsv"
sample () {   # sample <iter> <tag>
    local line ink
    line=$("$HERE/bannerprobe" auto --rect "$RECT" --raw --png "$OUT/last.png")
    ink=$(sed -n 's/^RAW \([0-9]*\).*/\1/p' <<<"$line")
    [ -n "${ink:-}" ] || return 0
    printf '%s\t%s\t%s\n' "$1" "$2" "$ink" >>"$OUT/ink.tsv"
    cp "$OUT/last.png" "$OUT/i$1-$2.png"
}
for i in $(seq 1 "$N"); do
    p=$(printf '%04d' "$i")
    "${SSH[@]}" "$GE ydotool key 1:1 1:0;
        for n in \$(seq 1 2000); do gdbus call --session --dest org.freedesktop.Notifications \
          --object-path /org/freedesktop/Notifications \
          --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
    # Three cards, each with a symbolic source icon, fixed-width text so ink is comparable.
    "${SSH[@]}" "$GE
        notify-send -a Software -i software-update-available-symbolic 'Critical Updates $p' 'Install critical updates as soon as possible';
        notify-send -a About -i help-about-symbolic 'Support GNOME $p' 'GNOME needs your help. Your donation will sustain it';
        notify-send -a Files -i folder-symbolic 'Transfer complete $p' 'Copied all of the requested files to the target'" >/dev/null 2>&1
    sleep 1
    for round in a b c; do
        "${SSH[@]}" "$GE ydotool key 125:1 47:1 47:0 125:0" >/dev/null 2>&1   # open
        sleep 0.4;  sample "$p" "$round-early"      # during the open animation
        sleep 1.1;  sample "$p" "$round-late"       # after it settles
        "${SSH[@]}" "$GE ydotool key 1:1 1:0" >/dev/null 2>&1                 # close
        sleep 0.5
    done
    echo "iter $p done"
done
echo "--- ink distribution ---"
awk -F'\t' '{print $3}' "$OUT/ink.tsv" | sort -n | awk '{a[NR]=$1} END{print "n="NR, "min="a[1], "median="a[int(NR/2)+1], "max="a[NR]}'
