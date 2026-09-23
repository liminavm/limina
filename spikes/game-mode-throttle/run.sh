#!/bin/bash
# Build the probe and the bait, then run the arms: for each, a supervisor-shaped parent spawns a
# worker-shaped child; 6 s in, the bait goes fullscreen for 15 s (Game Mode on), and the probe
# keeps reporting for the rest of its 32 s. The display is taken over while the bait is up.
#
#   run.sh <outdir> [arm...]
#       arms: none user latency parent-latency (activities; the default set)
#             guard-role guard-bg guard-both (both processes reset their own darwin state)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:?outdir}"
shift
arms=("$@")
[ ${#arms[@]} -gt 0 ] || arms=(none user latency parent-latency)
mkdir -p "$out" "$here/build"

clang -O2 -Wall -Wextra -fobjc-arc -framework AppKit -o "$here/build/probe" "$here/probe.m"
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
