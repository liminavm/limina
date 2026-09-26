#!/bin/bash
# Build the probe and the bait, then run the arms: for each, a supervisor-shaped parent spawns a
# worker-shaped child; 6 s in, the bait goes fullscreen for 15 s (Game Mode on), and the probe
# keeps reporting for the rest of its 32 s. The display is taken over while the bait is up.
#
#   run.sh <outdir> [arm...]
#       arms: none user latency parent-latency (activities; the default set)
#             guard-role guard-bg guard-both (both processes reset their own darwin state)
#             wake-burnN (wake.m: which wakes Game Mode throttles, with N game-side spinner threads)
#             lineage-shell lineage-app lineage-launchd (wake.m without audio, started from this
#             shell / spawned by an AppKit parent / as a `launchctl submit` job)
#             lineage-app-audio (spawned by an AppKit parent, AUHAL on)
#             lineage-agent (a gui-domain LaunchAgent with ProcessType=Interactive, AUHAL on)
# rendezvous.c (built and run by hand, see RESULTS.md 5): fds to a launchd job over Mach.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:?outdir}"
shift
arms=("$@")
[ ${#arms[@]} -gt 0 ] || arms=(none user latency parent-latency)
mkdir -p "$out" "$here/build"

clang -O2 -Wall -Wextra -fobjc-arc -framework AppKit -o "$here/build/probe" "$here/probe.m"
clang -O2 -Wall -Wextra -fobjc-arc -framework Foundation -framework AudioToolbox -o "$here/build/wake" "$here/wake.m"
app="$here/build/GameModeBait.app"
mkdir -p "$app/Contents/MacOS"
cp "$here/Info.plist" "$app/Contents/Info.plist"
swiftc -O -o "$app/Contents/MacOS/bait" "$here/bait.swift"
codesign -f -s - "$app" >/dev/null 2>&1

if pgrep -fl '[t]est-boot.sh|[r]un-suite.sh|[l]imina-vmm' >"$out/busy.txt"; then
    echo "WARNING: heavy processes present (Game Mode would throttle them too):" >&2
    /bin/cat "$out/busy.txt" >&2
fi

for arm in "${arms[@]}"; do
    if [[ "$arm" == lineage-* ]]; then
        start=$(date '+%Y-%m-%d %H:%M:%S')
        fifo="$here/build/wake.fifo"
        case "$arm" in
            lineage-shell)
                "$here/build/wake" --secs 32 --fifo "$fifo" --label "$arm" --no-audio >"$out/$arm.txt" 2>&1 &
                probe=$! ;;
            lineage-app)
                "$here/build/probe" parent --secs 32 --child-exe "$here/build/wake" --fifo "$fifo" \
                    --label "$arm" >"$out/$arm.txt" 2>&1 &
                probe=$! ;;
            lineage-app-audio)
                "$here/build/probe" parent --secs 32 --child-exe "$here/build/wake" --fifo "$fifo" \
                    --label "$arm" --child-audio >"$out/$arm.txt" 2>&1 &
                probe=$! ;;
            lineage-agent)
                plist="$here/build/dev.limina.spike.agent.plist"
                /bin/cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.limina.spike.agent</string>
  <key>ProgramArguments</key><array>
    <string>$here/build/wake</string><string>--secs</string><string>32</string>
    <string>--fifo</string><string>$fifo</string><string>--label</string><string>$arm</string>
  </array>
  <key>ProcessType</key><string>Interactive</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>$out/$arm.txt</string>
  <key>StandardErrorPath</key><string>$out/$arm.err</string>
</dict></plist>
PLIST
                launchctl bootstrap "gui/$(id -u)" "$plist"
                probe= ;;
            lineage-launchd)
                launchctl submit -l dev.limina.spike.wake -o "$out/$arm.txt" -e "$out/$arm.err" -- \
                    "$here/build/wake" --secs 32 --fifo "$fifo" --label "$arm" --no-audio
                probe= ;;
            *) echo "unknown arm $arm" >&2; exit 2 ;;
        esac
        sleep 6
        open -n "$app" --args 15 fifo "$fifo"
        if [ -n "$probe" ]; then
            wait "$probe"
        else
            sleep 28
            launchctl remove dev.limina.spike.wake 2>/dev/null || true
            launchctl bootout "gui/$(id -u)/dev.limina.spike.agent" 2>/dev/null || true
        fi
        log show --start "$start" --style compact \
            --predicate 'process == "gamepolicyd" AND eventMessage CONTAINS "Game mode"' \
            >"$out/$arm.gamepolicyd.txt" 2>&1
        echo "== $arm"
        /bin/cat "$out/$arm.gamepolicyd.txt" "$out/$arm.txt"
        sleep 5
        continue
    fi
    if [[ "$arm" == wake-burn* ]]; then
        start=$(date '+%Y-%m-%d %H:%M:%S')
        fifo="$here/build/wake.fifo"
        "$here/build/wake" --secs 32 --fifo "$fifo" --label "$arm" >"$out/$arm.txt" 2>&1 &
        probe=$!
        sleep 6
        open -n "$app" --args 15 burn "${arm#wake-burn}" fifo "$fifo"
        wait "$probe"
        log show --start "$start" --style compact \
            --predicate 'process == "gamepolicyd" AND (eventMessage CONTAINS "Game mode" OR eventMessage CONTAINS "gaming session")' \
            >"$out/$arm.gamepolicyd.txt" 2>&1
        echo "== $arm"
        /bin/cat "$out/$arm.gamepolicyd.txt" "$out/$arm.txt"
        sleep 5
        continue
    fi
    case "$arm" in
        none) pa=none ca=none g=none ;;
        user) pa=none ca=user g=none ;;
        latency) pa=none ca=latency g=none ;;
        parent-latency) pa=latency ca=none g=none ;;
        guard-role) pa=none ca=none g=role ;;
        guard-bg) pa=none ca=none g=bg ;;
        guard-both) pa=none ca=none g=both ;;
        *) echo "unknown arm $arm" >&2; exit 2 ;;
    esac
    start=$(date '+%Y-%m-%d %H:%M:%S')
    "$here/build/probe" parent --secs 32 --activity "$pa" --child-activity "$ca" --guard "$g" --label "$arm" \
        >"$out/$arm.txt" 2>&1 &
    probe=$!
    sleep 6
    open -n "$app" --args 15
    wait "$probe"
    log show --start "$start" --style compact \
        --predicate 'process == "gamepolicyd" AND (eventMessage CONTAINS "Game mode" OR eventMessage CONTAINS "gaming session")' \
        >"$out/$arm.gamepolicyd.txt" 2>&1
    echo "== $arm"
    /bin/cat "$out/$arm.gamepolicyd.txt"
    /bin/cat "$out/$arm.txt"
    sleep 5
done
