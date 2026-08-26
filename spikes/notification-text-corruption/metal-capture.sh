#!/usr/bin/env bash
# Capture a Metal GPU trace of a notification card's label passes, and score the same card.
#
# A trace is only worth opening in Xcode if the card it captured was actually damaged. Every
# synchronisation tried so far has cured this bug, and a GPU capture is a heavy one -- so this
# script never reports "captured" on its own. It reports captured AND damaged, or it says the
# capture came back clean, which is itself a finding (another sync-cure) rather than a failure.
#
# The window is opened by touching the trigger file the worker polls at command-buffer creation
# (KK_LIMINA_CAPTURE_TRIGGER), and only then is the card posted: Metal records at the API layer,
# so a capture opened after the pass is encoded would not contain it.
#
#   metal-capture.sh <ssh-port> <outdir> [attempts] [worker-log]
set -u
PORT="${1:?ssh port}"; OUT="${2:?outdir}"; ATTEMPTS="${3:-6}"
WLOG="${4:-/tmp/limina-worker-poke-stock-0824.log}"
TRIGGER="${KK_LIMINA_CAPTURE_TRIGGER:-/tmp/limina-kk-capture-trigger}"
HERE="$(cd "$(dirname "$0")" && pwd)"
# A live GPU capture drags the compositor to a crawl, so the card takes far longer than usual to
# arrive -- and GNOME hides it on its own timer regardless. There is therefore no fixed delay that
# is right: sample too early and the card has not rendered, too late and it is already gone. Poll
# instead, and take the measurement at the first moment a banner is provably on screen.
POLL_MAX="${POLL_MAX:-20}"
TITLE="${TITLE:-1120,100,480,28}"   # the only strip that scores content -- see calib.sh
BODY="${BODY:-1120,132,480,32}"     # validity gate: proves a banner was on screen at all
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/cap-%r@%h:%p -o ControlPersist=300 claude@127.0.0.1)
GE="export YDOTOOL_SOCKET=/tmp/.ydotool_socket;
    export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;"
mkdir -p "$OUT"

notify() {  # $1 = title suffix; echoes the notification id
    "${SSH[@]}" "$GE gdbus call --session \
        --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
        --method org.freedesktop.Notifications.Notify 'Software' 0 \
        'software-update-available-symbolic' 'Critical Updates $1' \
        'Install critical updates as soon as possible' '[]' '{}' 5000" \
        2>/dev/null | sed -n 's/.*uint32 \([0-9]*\).*/\1/p'
}

# Clear the list once: a resident card owns the banner slot and every sample would be discarded.
"${SSH[@]}" "$GE for n in \$(seq 1 6000); do gdbus call --session \
    --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1

for i in $(seq 1 "$ATTEMPTS"); do
    p=$(printf '%02d' "$i")
    [ -n "${LAST_ID:-}" ] && "${SSH[@]}" "$GE gdbus call --session \
        --dest org.freedesktop.Notifications --object-path /org/freedesktop/Notifications \
        --method org.freedesktop.Notifications.CloseNotification $LAST_ID" >/dev/null 2>&1
    "${SSH[@]}" "$GE ydotool key 1:1 1:0" >/dev/null 2>&1      # leave the overview
    "${SSH[@]}" "$GE ydotool key 62:1 62:0" >/dev/null 2>&1    # F4: reset the guest idle timer
    sleep 0.6
    "$HERE/bannerprobe" auto --sigs "$OUT/sigs" >/dev/null

    mark=$(wc -l <"$WLOG" | tr -d " ")
    touch "$TRIGGER"
    # Two stages: the trigger only says "a card is coming". The window itself is opened by the
    # 568x44 pass that always immediately precedes the 968x44 label pass, so the capture spans the
    # label render and almost nothing else. Confirm the trigger was consumed before posting --
    # the worker unlinks it when it takes it, which is the unambiguous signal that it was seen.
    for _ in $(seq 1 40); do
        [ -e "$TRIGGER" ] || break
        "${SSH[@]}" "$GE ydotool mousemove -- 5 0; ydotool mousemove -- -5 0" >/dev/null 2>&1
        sleep 0.25
    done
    if [ -e "$TRIGGER" ]; then
        echo "attempt $p: worker never took the trigger - skipping"
        rm -f "$TRIGGER"; continue
    fi

    LAST_ID=$(notify "$p")
    ttl=0; bdy=0
    for _ in $(seq 1 "$POLL_MAX"); do
        b=$("$HERE/bannerprobe" auto --since "$OUT/sigs" --rect "$BODY" --raw | sed -n 's/^RAW \([0-9]*\).*/\1/p')
        if [ "${b:-0}" -ge 200 ]; then
            bdy="$b"
            ttl=$("$HERE/bannerprobe" auto --since "$OUT/sigs" --rect "$TITLE" --raw | sed -n 's/^RAW \([0-9]*\).*/\1/p')
            "$HERE/bannerprobe" auto --since "$OUT/sigs" --png "$OUT/attempt-$p.png" >/dev/null
            break
        fi
        sleep 0.7
    done

    # A dead worker yields no verdict. The scanout freezes on the last frame that was actually
    # presented -- an earlier, good composite of this same card -- so a probe against it reports a
    # clean card no matter what the captured pass did. Scoring that is how a crash gets recorded
    # as a cure.
    if ! pgrep -f "limina-vmm .*$(basename "${LIMINA_DISK:-poke-stock-0824.raw}")" >/dev/null; then
        echo "attempt $p: worker died during the capture - NO VERDICT (see $OUT/attempt-$p.capture.log)"
        tail -n +"$mark" "$WLOG" | grep "LIMINA-KK-CAPTURE" >"$OUT/attempt-$p.capture.log"
        continue
    fi

    # Give the capture its closing commit, then read the verdict lines out of the worker log.
    sleep 2
    tail -n +"$mark" "$WLOG" | grep "LIMINA-KK-CAPTURE" >"$OUT/attempt-$p.capture.log"
    trace=$(sed -n 's/.*-> \(.*\) (.*/\1/p' "$OUT/attempt-$p.capture.log" | tail -1)
    passes=$(sed -n 's/.*stopped: passes=\([0-9]*\).*/\1/p' "$OUT/attempt-$p.capture.log" | tail -1)

    if [ "${bdy:-0}" -lt 200 ]; then
        echo "attempt $p: NO BANNER (body ink ${bdy:-0}) - discarded"
    elif [ "${ttl:-0}" -lt 100 ]; then
        echo "attempt $p: DAMAGED (title ink ${ttl:-0}, body $bdy) passes=${passes:-0} trace=${trace:-none}"
        [ -n "${trace:-}" ] && { echo "$trace" >"$OUT/DAMAGED-TRACE"; echo "  ^ this is the one to open in Xcode"; break; }
    else
        echo "attempt $p: clean (title ink $ttl, body $bdy) passes=${passes:-0} trace=${trace:-none}"
    fi
    sleep 3
done
