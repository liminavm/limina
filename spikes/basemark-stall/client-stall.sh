#!/bin/bash
# SPDX-License-Identifier: MIT
# Copyright © 2026 Gustavo Noronha Silva
#
# spikes/basemark-stall: virglrs d0416c9 harness/vm/client-basemark.sh, the perf passes' frozen
# client, changed in two places only -- it runs RUNS suites per session instead of two, and it
# waits for /result/ by polling, dumping the guest (dump-guest.sh) when one test page holds for
# STALL_S. Everything that decides how the suite is driven is the perf passes' own.
#
# Basemark Web 3.0 graphics suite on the stock guest, driven rather than clicked, because the
# sample has to be aimed: the suite is seven short tests in sequence, and a profile that smears
# across several of them cannot be attributed to any one.
#
# What this prints to stdout, interleaved and timestamped, is the whole point -- the host samples
# the vmm in parallel and the two logs are correlated afterwards. Nothing here renders a verdict.
#
# The trap this shares with client-gl-synoik.sh, and the reason that script exists: an SSH shell
# does not have the session's GL environment. Without it Firefox falls back to llvmpipe -- it
# draws, the benchmark completes, it reports a score, and the renderer under test never sees a
# command. That profile reads as "no hotspot", which is the most misleading result available, so
# the renderer is asserted below and a fallback refuses the run rather than scoring it.
set -u
# Repro knobs. A stall has only ever been seen in a session's second or later suite, so a session
# runs several. A test that shows one path for STALL_S is dumped; GIVE_UP_S ends that run.
RUNS=${RUNS:-4} POLL_S=${POLL_S:-2} STALL_S=${STALL_S:-60} GIVE_UP_S=${GIVE_UP_S:-600}

PROBE_HTML=/tmp/basemark-probe.html
URL_PROBE="file://$PROBE_HTML"
# suite=2 is the Graphics suite alone: WebGL 1.0.2, WebGL 2.0, Shader Pipeline, Draw-call Stress,
# Geometry Stress, Canvas, SVG. Per-test selection is a Corporate-version feature, so the suite is
# the finest cut available -- which is exactly the seven tests we care about.
# Community mode is ACTIVATED at the root and persists in the profile; it is not a query parameter
# that /run/ understands on its own. A fresh profile each run means activating each run, so this is
# loaded first and its only job is to leave that state behind. Skipping it leaves the suite sitting
# behind a Start button while the profile records an idle desktop.
#
# Every option is set HERE, on the root, and none of them are understood by /run/. Community mode
# prints its own configuration to the console, and that block -- not the URL we asked for -- is
# what says which suite is about to run. It is asserted below, because a suite=2 that silently
# stayed "All suites" scores the JS tests too and costs eight minutes to discover.
URL_MODE='https://web.gpuscore.com/?mode=community&suite=2&conformance=0&battery=0'
URL_RUN='https://web.gpuscore.com/run/'

PROFILE=/tmp/basemark-profile
LOG=/tmp/basemark.log

stamp() { while IFS= read -r l; do printf '%s %s\n' "$(date +%s.%N)" "$l"; done; }

# The session's GL environment, taken from the compositor itself -- see the note above.
COMP=$(pgrep -u "$(id -u)" -x gnome-shell || pgrep -u "$(id -u)" -x synoik || true)
COMP=${COMP%%$'\n'*}
if [ -n "$COMP" ]; then
  echo "compositor pid=$COMP ($(tr -d '\0' < /proc/$COMP/comm))"
  while IFS= read -r -d '' kv; do
    case "$kv" in
      LD_LIBRARY_PATH=*|MESA_*|GALLIUM_*|ZINK_*|LIBGL_*|EGL_*|VK_*|XDG_RUNTIME_DIR=*|DISPLAY=*)
        export "$kv"; echo "  inherited: $kv" ;;
    esac
  done < /proc/$COMP/environ
else
  echo "no compositor found; Firefox would fall back to llvmpipe and profile nothing" >&2
  exit 1
fi
: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"; export XDG_RUNTIME_DIR
for s in "$XDG_RUNTIME_DIR"/wayland-*; do
  case "$s" in *.lock) continue ;; esac
  [ -S "$s" ] && { export WAYLAND_DISPLAY="$(basename "$s")"; break; }
done
echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-UNSET}"
export MOZ_ENABLE_WAYLAND=1

rm -rf /tmp/basemark-profile
mkdir -p "$PROFILE"
# Content-process console.log to stdout: this is what carries community mode's machine-readable
# per-test lines, and so what the sample windows are aimed by. Verify on the first run that these
# actually appear -- a silent pref rename would leave the run looking fine and unattributable.
cat > "$PROFILE/user.js" <<'PREFS'
user_pref("devtools.console.stdout.content", true);
user_pref("webgl.sanitize-unmasked-renderer", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("app.update.auto", false);
user_pref("toolkit.telemetry.enabled", false);
PREFS

# A real file, not a data: URL: Firefox has blocked top-level data: navigation since 59, so a
# data: probe never loads at all -- and the failure is indistinguishable from the console pref not
# taking, which is a diagnostic pointing at the wrong thing.
cat > "$PROBE_HTML" <<'HTML'
<canvas id=c></canvas><script>
var gl = document.getElementById("c").getContext("webgl");
var e = gl && gl.getExtension("WEBGL_debug_renderer_info");
console.log("BASEMARK_PROBE renderer=" +
  (e ? gl.getParameter(e.UNMASKED_RENDERER_WEBGL) : "unknown") +
  " version=" + (gl ? gl.getParameter(gl.VERSION) : "no-webgl"));
</script>
HTML

# The one Firefox is this script's to end as well as to start. Left running, it holds the stdout
# the driver reads: `ssh guest client-basemark.sh` returns when nothing holds that stream any more,
# not when the script exits -- so a run that has finished and printed its scores looks, from the
# host, exactly like one still going, and the driver hangs until its own timeout.
#
# Killed by pattern and not by `$!`, which is the pipeline's LAST element (`tee`) and not the
# browser. The bracket keeps the pattern from matching the `pkill` that carries it.
cleanup() {
  pkill -u "$(id -u)" -f '[f]irefox.*basemark-profile' 2>/dev/null
  [ -n "${PIPE:-}" ] && kill "$PIPE" 2>/dev/null
  return 0
}
trap cleanup EXIT

echo "=== one Firefox for the whole run"
# ONE process, start to finish. The configuration community mode records is session state, and a
# probe/configure/run sequence of separate processes relies on it surviving a SIGTERM and a
# round trip through the profile on disk. It does not, reliably: the run comes up with defaults
# and the suite sits behind its Start button while every log line still looks right. Later URLs
# are handed to the instance that is already running, which is what a person does.
# 2400 and not 900: two suite runs plus the probe and configure steps do not fit in 15 minutes,
# and the timeout firing mid-run leaves the driver reading a log that simply stops.
timeout $((RUNS * 1500)) firefox --profile "$PROFILE" --new-instance --marionette "$URL_PROBE" 2>&1 \
  | stamp | tee -a "$LOG" &
PIPE=$!
sleep 22

probe=$(grep -o 'BASEMARK_PROBE renderer=.*' "$LOG" | tail -1 || true)
echo "probe: ${probe:-NONE}"
case "$probe" in
  *llvmpipe*|*softpipe*|*swrast*)
    echo "REFUSING: Firefox is on a software rasteriser -- this run would profile nothing" >&2
    exit 1 ;;
  "")
    echo "REFUSING: no probe line reached the log; the console pref did not take, so nothing" >&2
    echo "          here can be correlated or trusted" >&2
    exit 1 ;;
esac

# Configuring is per RUN, not per session. A session that has finished a run has consumed the
# state `/run/` needs: handing it that URL again lands on `/`, the suite never starts, and the wait
# below spends its whole 900 s on a browser sitting at the site's front page. Measured: run 2
# reported "no test page appeared" and `now at: "/"` while the renderer counted 0 fences/s.
configure() {
  echo "=== configuring: community mode and the graphics suite, in the same session"
  firefox --profile "$PROFILE" "$URL_MODE" > /dev/null 2>&1
  sleep 20

  echo "--- reported configuration:"
  grep -o '"[ ]*[A-Za-z][A-Za-z ]*: [^"]*"' "$LOG" | sort -u
  # The newest line each, because the log now carries one configuration block per run.
  suite_line=$(grep -o '"    Suite: [^"]*"' "$LOG" | tail -1)
  mode_line=$(grep -o '"    Mode: [^"]*"' "$LOG" | tail -1)
  case "$suite_line" in
    *Graphics*) ;;
    *) echo "REFUSING: the suite is ${suite_line:-unreported}, not Graphics" >&2; exit 1 ;;
  esac
  case "$mode_line" in
    *community*) ;;
    *) echo "REFUSING: mode is ${mode_line:-unreported}" >&2; exit 1 ;;
  esac
}

# The suite is run TWICE and the second run is the one scored. Not a retry -- always, and that is
# the point. The site's first /result/ of a fresh profile dies: measured twice, it ends on "Loading,
# please wait..." with a JSON.parse error of its own, after the run has finished and printed its
# UID. Re-running in the same session works. But a re-run ONLY on failure would make the score's
# warmth -- shader cache, JIT, the lot -- depend on whether the site happened to break that day,
# and the first test in the suite is the one warmth moves most. Two legs compared across that are
# not comparable. So both legs always pay for two runs and are read from the second.
suite() {
  configure
  echo "=== launching the suite in that same session (run $1)"
  firefox --profile "$PROFILE" "$URL_RUN" > /dev/null 2>&1
  sleep 12

  # Framing, not motion: see tap-keys.py. The overview keeps presenting, but it composites the
  # window as a thumbnail inside the shell's UI rather than showing it at its own size.
  sudo python3 /tmp/tap-keys.py esc || echo "WARNING: could not tap esc" >&2
  sleep 2

  # /run/ does not auto-start; it waits behind a Start button, and there is no URL parameter that
  # skips it. Clicked in the page rather than tapped through the focus order, because a
  # tab-then-enter guess reads as success whether or not it hit anything -- which is how a full
  # profile once got taken of a benchmark that had not started. This reports what it clicked, and
  # refuses if nothing matched.
  clicked=$(python3 /tmp/marionette.py js '
var els = Array.from(document.querySelectorAll("button,a,input[type=button],input[type=submit],[role=button]"));
var t = els.filter(function (e) {
  var s = (e.innerText || e.value || "").trim();
  return /^\s*start/i.test(s) && e.offsetParent !== null;
})[0];
if (!t) { return "NONE"; }
t.click();
return (t.tagName + " " + (t.innerText || t.value || "").trim()).slice(0, 60);
  ') || clicked='"ERROR"'
  echo "start control: $clicked"
  # Best effort, not a gate: once the session is configured, /run/ starts on its own, and a run
  # that clicked nothing still runs. What settles whether the suite is going is the page itself.
  case "$clicked" in
    *NONE*|*ERROR*) echo "note: no Start control matched; /run/ normally starts by itself" >&2 ;;
  esac

  # The page's own statement of what it is doing -- /run/tests/<n>/graphics_suite/<test_name>/
  # while a test runs. This is the aiming signal: a sample window labelled by the pathname it was
  # taken under is attributable to one test, which is the whole reason this suite needs driving.
  if ! python3 /tmp/marionette.py waitpath 'graphics_suite' 240 > /dev/null 2>&1; then
    echo "WARNING: no test page in 240s; at $(python3 /tmp/marionette.py js \
      'return document.location.pathname' 2>/dev/null) -- not waiting 900s for a result" >&2
    return 1
  fi
  echo "now at: $(python3 /tmp/marionette.py js 'return document.location.pathname' 2>/dev/null)"

  echo "=== suite $1 running; sample the host vmm now"
  echo "=== host side: sample \$(pgrep -f '[l]imina-vmm --cpus') 5 -f prof.txt"

  # WHICH page, by pathname -- not by what the text says. Body text cannot tell a test page from
  # the result page: every test page reads "WebGL 2.0 Test", so waiting for /WebGL/ returns at test
  # 5 of 20 and prints a test page's furniture under "scores:". That is how a run once reported
  # scores it had not taken. The suite is at /run/tests/<n>/..., the results at /result/.
  echo "=== waiting for /result/ (suite $1)"
  # Not waitpath: it holds Marionette's one session for the whole wait, and a stalled page is
  # exactly when something else needs to look. Short connections, one every POLL_S, each logged
  # with the time, so where a run stopped and for how long is in the record rather than inferred.
  local last="" since now path stalled=0
  since=$(date +%s)
  while :; do
    now=$(date +%s)
    path=$(timeout 20 python3 /tmp/marionette.py url x 2>/dev/null) || path="NO-ANSWER"
    path=${path#*gpuscore.com}
    if [ "$path" != "$last" ]; then
      echo "$now PATH $path"; last=$path; since=$now
    fi
    case "$path" in
      *'/result/'*) break ;;
      # The run finished but the site put its scores on its other result page -- the usual end
      # of a fresh profile's first run. Finished, not stalled.
      *benchmark-result*) echo "=== FINISHED run $1 on the site's other result page"; return 1 ;;
    esac
    if [ "$stalled" = 0 ] && [ $((now - since)) -ge "$STALL_S" ]; then
      stalled=1
      echo "=== STALL run $1 at $path for $((now - since)) s"
      /tmp/dump-guest.sh "/tmp/stall-$1-a" > /dev/null 2>&1
      echo "=== STALL dump a written"
      sleep 60
      /tmp/dump-guest.sh "/tmp/stall-$1-b" > /dev/null 2>&1
      echo "=== STALL dump b written"
    fi
    if [ $((now - since)) -ge "$GIVE_UP_S" ]; then
      echo "WARNING: the suite never reached /result/ (run $1); stuck at $path for $((now - since)) s" >&2
      [ "$stalled" = 1 ] && /tmp/dump-guest.sh "/tmp/stall-$1-c" > /dev/null 2>&1 \
        && echo "=== STALL dump c written"
      return 1
    fi
    sleep "$POLL_S"
  done
  [ "$stalled" = 1 ] && echo "=== RECOVERED run $1 after the stall dump"
  # Reached the page; now whether it RENDERED. These are two conditions and the site fails the
  # second on its own -- hence the digits, which "Loading, please wait..." does not have.
  python3 /tmp/marionette.py wait 'Shader Pipeline[^0-9]*[0-9]' 180 > /tmp/basemark-scores.txt 2>/dev/null \
    || { echo "WARNING: /result/ rendered no score table (run $1)" >&2; return 1; }
  echo "--- scores (run $1):"
  cat /tmp/basemark-scores.txt
  return 0
}

# Every line of this client stamped, so the host's worker log and samples can be lined up with it.
exec > >(stamp) 2>&1
for r in $(seq 1 "$RUNS"); do
  suite "$r" || echo "note: run $r yielded no scores" >&2
done
echo "=== client done"
