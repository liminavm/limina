#!/usr/bin/env bash
# Calibrate and VALIDATE the header detector against ground truth.
#
# For each post it records (a) the ink in a strip that contains ONLY the header text — above the
# large icon, left of the close button — and (b) the full banner PNG, so the number can be checked
# by eye against what actually rendered. A detector for an intermittent bug is worthless unless its
# hits and misses have been confirmed against the pixels at least once.
#
# Liveness is taken against a signature snapshot from BEFORE the post, because the scanout pool
# rotates and a stale member reports plausible constant ink forever.
#   calib.sh <ssh-port> <posts> <outdir>
set -u
PORT="${1:?}"; N="${2:-8}"; OUT="${3:-calib}"
HERE="$(cd "$(dirname "$0")" && pwd)"
# NTITLE/NBODY override the posted strings. The bug's incidence turned out to depend on WHICH
# card is posted, so the text is a variable of the experiment, not decoration.
NTITLE="${NTITLE:-Critical Updates}"
NBODY="${NBODY:-Install critical updates as soon as possible}"
HDR="${HDR:-980,48,440,30}"     # header-text strip only: excludes the icon below and the × right
TITLE="${TITLE:-1120,100,480,28}" # title-text strip: the ONLY strip that scores content, see below
BODY="${BODY:-1120,132,480,32}" # body-text strip: the VALIDITY GATE, see below
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cal-%r@%h:%p -o ControlPersist=300 claude@127.0.0.1)
GE="export YDOTOOL_SOCKET=/tmp/.ydotool_socket;
    export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;"
mkdir -p "$OUT"; : >"$OUT/hdr.tsv"
# Clear the whole notification list ONCE. A resident card (e.g. "Support GNOME") otherwise owns
# the banner slot and every sample is discarded. Doing this per-post instead cost several seconds
# each, which made the sample sizes this bug's run-to-run swing demands unaffordable.
"${SSH[@]}" "$GE for n in \$(seq 1 6000); do gdbus call --session \
    --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
# Leave the Activities overview. A freshly-booted session lands there, and GNOME shows no banners
# in the overview at all — every sample would be discarded by the gate below (or, without it,
# recorded as 100% damage). Escape only here: during the run it would dismiss the banner itself.
"${SSH[@]}" "$GE ydotool key 1:1 1:0" >/dev/null 2>&1
sleep 1.5
for i in $(seq 1 "$N"); do
    p=$(printf '%03d' "$i")
    # Close only the card this script posted last, not the whole id space.
    if [ -n "${LAST_ID:-}" ]; then
        "${SSH[@]}" "$GE gdbus call --session \
            --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
            --method org.freedesktop.Notifications.CloseNotification $LAST_ID" >/dev/null 2>&1
    fi
    # Leave the overview before EVERY post, not just once. If the session is in the overview the
    # banner sits lower and the header strip lands on the search entry instead -- whose ink scores
    # as a present header, silently turning damaged samples into clean ones. Safe here because the
    # previous card is already closed and the next is not yet posted, so Escape cannot dismiss the
    # banner being measured.
    "${SSH[@]}" "$GE ydotool key 1:1 1:0" >/dev/null 2>&1
    "${SSH[@]}" "$GE ydotool key 62:1 62:0" >/dev/null 2>&1   # F4: reset the guest idle timer
    sleep 0.6
    "$HERE/bannerprobe" auto --sigs "$OUT/sigs" >/dev/null
    # Notify returns the id, so the next iteration can close exactly this card.
    LAST_ID=$("${SSH[@]}" "$GE gdbus call --session \
        --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
        --method org.freedesktop.Notifications.Notify 'Software' 0 \
        'software-update-available-symbolic' '$NTITLE $p' '$NBODY' '[]' '{}' 5000" \
        2>/dev/null | sed -n 's/.*uint32 \([0-9]*\).*/\1/p')
    sleep 1.1
    hdr=$("$HERE/bannerprobe" auto --since "$OUT/sigs" --rect "$HDR" --raw | sed -n 's/^RAW \([0-9]*\).*/\1/p')
    # The TITLE is the only element whose text CHANGES per post, so it is the only one that can
    # distinguish a card that rendered from one showing preserved stale pixels. The header strip
    # cannot: "Software  Just now" is identical on every card, so any arm that preserves a reused
    # texture's contents makes the header come back and scores as a clean card while the title is
    # still missing. That is not hypothetical -- KK_LIMINA_FORCE_LOAD=small measured 7/7 "clean" on
    # the header strip and every one of those cards had no title. Score the title; keep the header
    # only as a second signal.
    ttl=$("$HERE/bannerprobe" auto --since "$OUT/sigs" --rect "$TITLE" --raw | sed -n 's/^RAW \([0-9]*\).*/\1/p')
    # Validity gate. GNOME withholds banners entirely once its idle monitor says the user is away,
    # and "no banner at all" measures zero header ink — identical to "banner with no header". Only
    # a sample whose BODY row is inked proves a banner was actually on screen and the zero above
    # means something. Without this the damage rate silently inflates to 100% as the session idles.
    bdy=$("$HERE/bannerprobe" auto --since "$OUT/sigs" --rect "$BODY" --raw | sed -n 's/^RAW \([0-9]*\).*/\1/p')
    "$HERE/bannerprobe" auto --since "$OUT/sigs" --png "$OUT/full-$p.png" >/dev/null
    if [ "${bdy:-0}" -lt 200 ]; then
        printf '%s\tNOBANNER\t%s\n' "$p" "${bdy:-0}" >>"$OUT/hdr.tsv"
        echo "post $p NO BANNER (body ink ${bdy:-0}) - discarded"
    else
        printf '%s\t%s\t%s\t%s\n' "$p" "${ttl:-NA}" "${hdr:-NA}" "${bdy}" >>"$OUT/hdr.tsv"
        echo "post $p TITLE-ink=${ttl:-NA} header-ink=${hdr:-NA} body-ink=$bdy"
    fi
    sleep 4.5
done
