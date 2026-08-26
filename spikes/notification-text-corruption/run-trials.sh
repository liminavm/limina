#!/usr/bin/env bash
# Drive the notification-text-corruption repro and measure every card.
#
# One notification at a time (sent, measured, then closed) so the banner geometry stays fixed and
# no tray backlog builds up — a large outstanding list visibly slows the guest shell down.
#
# EVERY TRIAL USES UNIQUE TEXT. This is load-bearing, not cosmetic: with a constant string cogl
# renders the glyphs once and every later card reuses the cached texture, so only the very first
# card exercises a fresh text render. 30 trials of identical text reproduced nothing; the fault
# rides the creation of a new text texture, so the counter has to reach the pixels.
#
#   run-trials.sh <ssh-port> <surface-id-list> <trials> <outdir>
#
# The scanout is a rotating pool, so no single IOSurface id stays live: pass the whole pool
# (comma-separated, e.g. 77,156,254,255 — read them from the worker log's "scanout 0 -> IOSurfaces"
# line) and the runner takes whichever one currently holds the card.
#
# Writes outdir/trials.tsv (n, HEADER, TITLE, BODY ink) and, for any card where a band's ink
# collapses, outdir/bad-<n>.png. Bands: header = app icon + name + "Just now", title = summary,
# body = the message. The bug drops or garbles one of them; a healthy card inks all three.
set -u
PORT="${1:?ssh port}"; SURFS="${2:?scanout surface id list}"; N="${3:-100}"; OUT="${4:-out}"
# IDLE=<secs> leaves the desktop completely quiet before each notification. Back-to-back trials
# never let the compositor or GPU settle, which may be why a dense loop stays clean.
IDLE="${IDLE:-0}"
mkdir -p "$OUT"
HERE="$(cd "$(dirname "$0")" && pwd)"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes
     -o ControlMaster=auto -o ControlPath=/tmp/nc-%r@%h:%p -o ControlPersist=120 claude@127.0.0.1)
: >"$OUT/trials.tsv"
# Flush any notification still outstanding. A single leftover CRITICAL notification pins the banner
# slot forever (GNOME never auto-dismisses those), so every trial afterwards queues behind it and
# the probe measures that one stale card over and over — with perfectly plausible ink.
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/\$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;
    for n in \$(seq 1 1200); do gdbus call --session --dest org.freedesktop.Notifications \
      --object-path /org/freedesktop/Notifications \
      --method org.freedesktop.Notifications.CloseNotification \$n; done" >/dev/null 2>&1
sleep 2
for i in $(seq 1 "$N"); do
    [ "$IDLE" != "0" ] && sleep "$IDLE"
    "$HERE/defeat-idle.sh"      # else the banner is withheld and we measure a frozen screen
    "$HERE/bannerprobe" auto --sigs "$OUT/sigs" >/dev/null   # snapshot BEFORE the notification
    id=$("${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/\$(id -u);
        export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;
        notify-send -p 'notifyprobe $i' 'MMMM WWWW MMMM $i'" 2>/dev/null | tr -d '\r')
    # Sample the same card at three moments: a corruption that heals on the next repaint would
    # be invisible to a single well-timed read. Worst reading across the three wins.
    line="NO_CARD"; bh=999999; bt=999999; bb=999999; got=0
    for delay in 0.5 0.8 0.9; do
        sleep "$delay"
        for sid in ${SURFS//,/ }; do
            try=$("$HERE/bannerprobe" "$sid" --since "$OUT/sigs" --png "$OUT/s-$i.png")
            case "$try" in NO_CARD*|SURFACE_DEAD*) continue ;; esac
            got=1; line="$try"
            # Parse by LABEL, never by field position: the probe's prefix fields grew once
            # already (LIVE/SURF), which silently shifted every column and reported healthy cards
            # as damaged.
            h=$(sed -n 's/.*HEADER \([0-9]*\).*/\1/p' <<<"$try")
            t=$(sed -n 's/.*TITLE \([0-9]*\).*/\1/p' <<<"$try")
            b=$(sed -n 's/.*BODY \([0-9]*\).*/\1/p' <<<"$try")
            [ "$h" -lt "$bh" ] && { bh=$h; cp "$OUT/s-$i.png" "$OUT/worst-$i.png"; }
            [ "$t" -lt "$bt" ] && { bt=$t; cp "$OUT/s-$i.png" "$OUT/worst-$i.png"; }
            [ "$b" -lt "$bb" ] && { bb=$b; cp "$OUT/s-$i.png" "$OUT/worst-$i.png"; }
            break
        done
    done
    [ "$got" = "1" ] && line="HEADER $bh TITLE $bt BODY $bb"
    case "$line" in
        NO_CARD*|SURFACE_DEAD*) printf '%s\t-\t-\t-\t%s\n' "$i" "${line%% *}" >>"$OUT/trials.tsv" ;;
        *) printf '%s\t%s\t%s\t%s\tOK\n' "$i" "$bh" "$bt" "$bb" >>"$OUT/trials.tsv"
           # A band that collapses is the bug; keep the pixels as evidence.
           if [ "${h:-0}" -lt 300 ] || [ "${t:-0}" -lt 300 ] || [ "${b:-0}" -lt 300 ]; then
               cp "$OUT/worst-$i.png" "$OUT/bad-$i.png"
           fi ;;
    esac
    [ -n "$id" ] && "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/\$(id -u);
        export DBUS_SESSION_BUS_ADDRESS=unix:path=\$XDG_RUNTIME_DIR/bus;
        gdbus call --session --dest org.freedesktop.Notifications \
          --object-path /org/freedesktop/Notifications \
          --method org.freedesktop.Notifications.CloseNotification $id" >/dev/null 2>&1
    sleep 0.4
done
echo "--- done: $(wc -l <"$OUT/trials.tsv") trials, $(ls "$OUT"/bad-*.png 2>/dev/null | wc -l) damaged ---"
