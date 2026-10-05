#!/bin/bash
# Build lpq and run its policy x busy matrix under each launch path, contexts interleaved per rep and
# the arm order reversed on alternate reps (to spread host drift across arms).
#
#   run.sh <outdir> <reps> [policies] [busy-us list] [periods] [contexts]
#     contexts: shell (this shell's process tree), app (spawned by an AppKit app opened with
#               `open -n`: the worker's shape before limina-launch), job (a gui-domain launchd job
#               with ProcessType=Interactive: the worker's shape now), jobLT (the same job plus
#               LegacyTimers=true)
#
# Output: <outdir>/<context>-r<rep>.txt, one per run, each ending in a DONE line.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$(mkdir -p "${1:?outdir}" && cd "$1" && pwd)"
reps="${2:?reps}"
policies="${3:-default,utility,ui,lat0,critical,rt,wg,wgrt}"
busies="${4:-300,4000,13000}"
periods="${5:-240}"
contexts="${6:-shell,app,job,jobLT}"
ecores=0,1 # this M1 Max; re-check with spikes/rt-ecore-placement `placement calib-bg`

mkdir -p "$here/build"
clang -O2 -Wall -Wextra -framework AudioToolbox -o "$here/build/lpq" "$here/lpq.c"
app="$here/build/LpqApp.app"
mkdir -p "$app/Contents/MacOS"
/bin/cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>dev.limina.spike.lpqapp</string>
  <key>CFBundleExecutable</key><string>lpqapp</string>
  <key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
PLIST
clang -O2 -Wall -Wextra -fobjc-arc -framework AppKit -o "$app/Contents/MacOS/lpqapp" "$here/appstub.m"
codesign -f -s - "$app" >/dev/null 2>&1

lpq="$here/build/lpq"
uid="$(id -u)"

wait_done() { # file bound-seconds
    local f="$1" bound="$2" t0=$SECONDS
    until grep -q ' DONE$' "$f" 2>/dev/null; do
        if ((SECONDS - t0 > bound)); then
            echo "ABORT: no DONE in $f after ${bound}s" | tee -a "$f" >&2
            return 1
        fi
        sleep 2
    done
}

job_plist() { # label outfile legacy(0/1) driver-args...
    local lbl="$1" of="$2" legacy="$3"
    shift 3
    local args=""
    for a in "$lpq" driver "$@"; do args+="<string>$a</string>"; done
    local lt=""
    [ "$legacy" = 1 ] && lt="<key>LegacyTimers</key><true/>"
    /bin/cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$lbl</string>
  <key>ProgramArguments</key><array>$args</array>
  <key>ProcessType</key><string>Interactive</string>
  $lt
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>$of</string>
  <key>StandardErrorPath</key><string>$of</string>
</dict></plist>
PLIST
}

ncells=$(($(tr ',' '\n' <<<"$policies" | wc -l) * $(tr ',' '\n' <<<"$busies" | wc -l)))
bound=$((ncells * (periods * 17 / 1000 + 6) + 30))

for rep in $(seq 1 "$reps"); do
    rev=""
    ((rep % 2 == 0)) && rev="--reverse"
    for ctx in ${contexts//,/ }; do
        of="$out/$ctx-r$rep.txt"
        : >"$of"
        {
            echo "# $ctx rep $rep $(date '+%F %T') load: $(sysctl -n vm.loadavg)"
            pgrep -fl '[l]imina-vmm|[t]est-boot|[r]un-suite' | awk '{print "# busy: " $1, $2}' || true
        } >>"$of"
        dargs=(--policies "$policies" --busy "$busies" --periods "$periods" --ecores "$ecores" --label "$ctx-r$rep")
        [ -n "$rev" ] && dargs+=("$rev")
        case "$ctx" in
        shell)
            "$lpq" driver "${dargs[@]}" >>"$of" 2>&1
            ;;
        app)
            open -n "$app" --args "$of" "$lpq" driver "${dargs[@]}"
            wait_done "$of" "$bound"
            ;;
        job | jobLT)
            lbl="dev.limina.spike.lpq.$ctx"
            plist="$here/build/$lbl.plist"
            job_plist "$lbl" "$of" "$([ "$ctx" = jobLT ] && echo 1 || echo 0)" "${dargs[@]}" >"$plist"
            launchctl bootout "gui/$uid/$lbl" 2>/dev/null || true
            launchctl bootstrap "gui/$uid" "$plist"
            wait_done "$of" "$bound" || true
            launchctl bootout "gui/$uid/$lbl" 2>/dev/null || true
            ;;
        *)
            echo "unknown context $ctx" >&2
            exit 2
            ;;
        esac
        echo "done $ctx r$rep"
        sleep 3
    done
done
