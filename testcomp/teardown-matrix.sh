#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# M3b: drive the teardown paths of spikes/venus-churn-retention/buffer-lifetime-matrix.md §4
# and report what the host actually did, per path.
#
# This is a DISCRIMINATOR, not a confirmation. The matrix originally assumed a SIGKILLed client
# lands in vkr's bare-free() branch and leaks; reading vkr_context.c showed the opposite — the
# sweep is gated on ctx->instance being LIVE (vkr_context.c:1005), and a SIGKILLed client never
# calls vkDestroyInstance, so abrupt exit is the path that runs the FULL sweep. So the job here
# is to find out which path leaves a residual, not to prove a predicted one does.
#
# Modes. The second argument picks WHO OWNS the host IOSurface, which is the axis M3c adds:
# `venus` (default) is symmetric — the owning ref and the borrowed +1 are both venus-side, so one
# sweep frees both. `gbm` is the ASYMMETRIC case of matrix §3 — vrend owns, venus only borrows.
#
#   ./teardown-matrix.sh                    paths 1, 2, 2b with a venus-allocated client  (M3b)
#   ./teardown-matrix.sh matrix gbm         the same paths with a gbm/vrend client        (M3c)
#   ./teardown-matrix.sh leakexit [venus|gbm]
#                                   path 1': a CLEAN exit still holding leaked imports. The only
#                                   arm that reaches the bare-free() branch — every kill above
#                                   goes through the live-instance full sweep instead.
#   ./teardown-matrix.sh redgreen [venus|gbm]
#                                   the vehicle-rule gate: prove the setup can see a retained
#                                   import OF THAT CLASS before believing any green above it
#   ./teardown-matrix.sh path3      the ERROR-PATH PARTIAL: creates the host must refuse
#                                   (VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT, the
#                                   -1000158000 of the incident) — RED/GREEN, needs the fault patch
#   ./teardown-matrix.sh shipped3   the same trigger against a CLEAN build: what limina really does
#   ./teardown-matrix.sh path4 [venus|gbm]
#                                   RING-FATAL teardown: the compositor over-allocates, limina's
#                                   host GPU budget kills ITS context, and the question is what
#                                   happens to the imports it was holding. Also with its own RED
#
# Paths 3 and 4 need a worker built with spikes/venus-churn-retention/fault-inject-paths-3-4.patch
# and a boot that sets LIMINA_GPU_MEM_BUDGET_MIB=2048. Both REDs are selected by touching a file
# rather than by an env var, so each arm's RED and GREEN run against ONE boot — a reboot between
# them would put a fresh guest, fresh contexts and a fresh census between two numbers that only
# mean something compared to each other.
#
# The census-forcing tick client is deliberately venus in BOTH arms: it exists only to make the
# worker allocate, and holding it constant keeps it from being a second variable.
#
# Run from the repo root, with a guest already booted by
#   LIMINA_DISK=<enhanced.raw> LIMINA_GLOBAL_SCANOUT=1 LIMINA_GPU_MEM_BUDGET_CENSUS=10 \
#     RUST_LOG="warn,krun_rutabaga_gfx::virgl_renderer=info" \
#     spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
#
# The RUST_LOG matters: the import and context-destroy lines are vkr_log = VIRGL_LOG_LEVEL_INFO
# and are INVISIBLE at the default `warn`, while the budget lines (vkr_log_error) are not — so a
# log filtered to `warn` looks instrumented while telling you nothing about the import path.

set -uo pipefail

PORT="${PORT:-2222}"
WORKER_LOG="${WORKER_LOG:-/tmp/enhanced-efi-kk-worker.log}"
SIZE="${SIZE:-1920x1080}"   # 8.3 MiB per buffer, so a retained one is unambiguous
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes claude@127.0.0.1)
ICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json

g() { "${SSH[@]}" "$@" 2>/dev/null; }

worker_pid() { pgrep -f "target/debug/limina-vmm" | head -1; }

# THE ORACLE: the dealloc-sentinel `alive` count from vkr_mtl_refcount_census
# (vkr_metal_helpers.m:152). It counts real IOSurface -dealloc, so one retained surface is
# visible regardless of byte noise.
#
# Five oracles were tried and only two discriminate — measured 2026-08-07 against a deliberate
# leak (`--leak-imports`, 40 x 1920x1080 buffers). Run `redgreen` below to reproduce:
#
#   owned unmapped   the row is ABSENT on this worker entirely. Not zero — absent. It is the
#                    right oracle for M1's scanout churn and the wrong one here.
#   vmmap IOSurface  364.2M -> 351.9M in BOTH arms. Virtual size, dominated by other mappings.
#   iosurface (+N)   `iosurface A/F (+1)` in BOTH arms: that counter tracks alloc-vs-free of
#                    surfaces we OWN, and a borrowed import allocates nothing.
#   DEALLOC alive    2 -> 2 (green) vs 2 -> 43 (red). This one.
#   lookup (+N)      `lookup 508/508 (+0)` vs `lookup 506/465 (+41)`. Also discriminates, and
#                    more specifically: it counts exactly the outstanding borrowed +1 refs that
#                    `vkr_mtl_iosurface_lookup` handed out. Use it to corroborate `alive`, which
#                    also moves for surfaces nobody imported.
#
# Note the two `(+N)` counters on the same census line disagree by design — do not read "the
# census counter" as one thing. Identical A/B readings from the first three are the classic
# "the differential is not reaching the system under test" — not an absence of retention.
alive_iosurfaces() {
  grep -a "DEALLOC iosurface" "$WORKER_LOG" | tail -1 \
    | sed 's/.*DEALLOC iosurface [0-9]* (alive \([0-9-]*\)).*/\1/'
}

# The census ticks on the ALLOCATION path (vkr_budget.c:425), so a quiesced worker never emits a
# fresh one and `tail -1` silently returns a STALE line — which reads as "nothing changed". Wait
# out the interval, then make the compositor allocate again with a throwaway client.
force_census() {
  local mark; mark=$(mark)
  sleep 11
  g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD XDG_RUNTIME_DIR=/tmp/testcomp-run \
     WAYLAND_DISPLAY=wayland-1 timeout 15 ./limina-testcomp client-dmabuf 3 320x240" >/dev/null 2>&1
  sleep 2
  since "$mark" | grep -a "DEALLOC iosurface" | tail -1 \
    | sed 's/.*DEALLOC iosurface [0-9]* (alive \([0-9-]*\)).*/\1/'
}

# macOS `wc -l` pads with spaces; `tail -n +<padded>` rejects it as an illegal offset.
mark() { wc -l < "$WORKER_LOG" | tr -d ' '; }
since() { tail -n "+$1" "$WORKER_LOG"; }

report() {
  local path="$1" from="$2"
  echo "  alive IOSurfaces: $(force_census)"
  # Per-PATH provenance, not just per-run: the standard for the gbm arm is that every path shows
  # host-side evidence its buffers were classic. Printing it once for the whole script would let
  # a single path silently run venus blobs under a vrend label.
  [ "${KIND:-venus}" = gbm ] && vrend_bucket
  echo "  --- host lines for this path ---"
  # The destroy line is the discriminator: it says whether the instance was live (full sweep) or
  # gone (bare free()), and how much survived the sweep.
  since "$from" | grep -aE "destroying context|destroyed —|destroyed with|import res|still holds" \
    | sed 's/.*virgl: vkr: /    /' | sed 's/.*virgl_renderer\] /    /'
}

start_comp() {
  local extra="$1"
  g "sudo -n pkill -f 'limina-testcomp run'" ; sleep 1
  # shellcheck disable=SC2029
  "${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info nohup \
     ./limina-testcomp run $extra > /tmp/comp.log 2>&1 &" >/dev/null 2>&1 &
  sleep 4
  g "grep -a 'COMPOSITOR READY' /tmp/comp.log" || { echo "!! compositor did not start"; g 'tail -5 /tmp/comp.log'; return 1; }
}

# THE PROVENANCE ORACLE for the gbm arm — proves the buffer under test is a CLASSIC vrend
# resource and not a venus blob wearing a gbm API.
#
# This is the check without which M3c is unreadable. `MESA_LOADER_DRIVER_OVERRIDE` selects gbm's
# backing driver: under `zink` a gbm buffer IS a venus allocation, so the arm would re-run M3b's
# symmetric case while every label said vrend. The client's own env guard (require_classic_gbm)
# proves only intent — this proves what the host actually did.
#
# It works because a vrend-allocated IOSurface bills to the shared "vrend" pseudo-context
# (ctx UINT32_MAX-1 = 4294967294, vkr_budget_set_vrend() from vrend_resource_iosurface_init),
# while a venus one bills to the client's own context. So the bucket naming a charge of exactly
# the client's buffer size is positive proof of who allocated it.
#
# Measured 2026-08-07 with a 640x480 client: `ctx 4294967294 [vrend]: 1.2 MiB live ... 1 x 1.2 MiB
# (IOSurface, 1 ever)` at the same second as the compositor's import line, and 1.2 MiB is exactly
# 640*480*4 = 1228800, the size the import line reported.
#
# Note the budget ledger is the RIGHT oracle for provenance and the WRONG one for retention here:
# per-client attribution of a vrend surface does not exist by construction (see the matrix §6).
vrend_bucket() {
  grep -a "\[vrend\]" "$WORKER_LOG" | tail -1 | sed 's/.*\[vrend\]: /    vrend bucket: /'
}

# Both census `(+N)` refcount deltas plus the dealloc `alive` count, on one line.
refs() {
  local mark; mark=$(mark)
  sleep 11
  g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD XDG_RUNTIME_DIR=/tmp/testcomp-run \
     WAYLAND_DISPLAY=wayland-1 timeout 15 ./limina-testcomp client-dmabuf 3 320x240" >/dev/null 2>&1
  sleep 2
  # Keep BOTH `(+N)` counters: `iosurface` is blind to a borrowed import (it counts surfaces we
  # own), `lookup` is exactly the outstanding borrowed +1s. Dropping the latter would hide the
  # discriminating half of the reading.
  since "$mark" | grep -a "refs — iosurface" | tail -1 \
    | sed 's/.*refs — //;s/ texture [0-9/]* ([+-][0-9]*)//;s/ registry [0-9/]* ([+-][0-9]*)//' \
    | sed 's/ publish .*| DEALLOC iosurface [0-9]*/ |/;s/ vkr-tex.*//'
}

# ---------------------------------------------------------------------------------------------
# `redgreen` — the vehicle-rule gate. A negative result from this vehicle only counts once it
# has caught a real failure of THIS class, so before trusting the all-clean matrix below, prove
# the setup can see a retained client import at all: run the same 40-buffer churn against a
# compositor that deliberately never evicts (`--leak-imports`) and against the shipped one.
#
# Measured 2026-08-07: GREEN 2 -> 2 alive / 42 evictions; RED 2 -> 43 alive / 0 evictions.
# The +41 (not +40) is right: `refs`/`force_census` run a throwaway tick client to make the
# worker allocate, and under RED that client's import is retained too — the baseline reading
# already includes the opening tick, so the delta is 40 churn buffers + the closing tick.
# ---------------------------------------------------------------------------------------------
if [ "${1:-matrix}" = "redgreen" ]; then
  # Second argument picks WHO OWNS the host IOSurface — the whole axis M3c adds:
  #   venus (default)  the client allocates through Vulkan. Symmetric: both the owning ref and
  #                    the borrowed +1 sit on the venus side, so one sweep frees both.
  #   gbm              the client allocates through gbm as a CLASSIC vrend resource. Asymmetric:
  #                    vrend owns, venus borrows — matrix §3, the shape §1's residual looks like.
  KIND="${2:-venus}"
  case "$KIND" in
    venus) CHURN_MODE="client-churn"; CLIENT_ENV="" ;;
    gbm)   CHURN_MODE="client-churn-gbm"; CLIENT_ENV="MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu" ;;
    *) echo "unknown kind $KIND — expected venus or gbm"; exit 1 ;;
  esac

  echo "=== M3 RED/GREEN ($KIND-allocated client buffers) ==="
  echo "can this vehicle see a retained client import of this class?"
  echo
  for arm in "GREEN (shipped)::" "RED (--leak-imports)::--leak-imports"; do
    name="${arm%%::*}"; flag="${arm##*::}"
    start_comp "$flag" || exit 1
    echo "--- $name ---"
    echo "  before: $(refs)"
    g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV XDG_RUNTIME_DIR=/tmp/testcomp-run \
       WAYLAND_DISPLAY=wayland-1 ./limina-testcomp $CHURN_MODE 40 $SIZE" \
      | grep -a "CHURN DONE" | sed 's/^/  /'
    echo "  after : $(refs)"
    [ "$KIND" = gbm ] && vrend_bucket
    echo "  comp  : $(g 'grep -ac "imported client dmabuf" /tmp/comp.log' | tr -d ' ') imports / \
$(g 'grep -ac "evicted a client dmabuf" /tmp/comp.log' | tr -d ' ') evictions"
    echo
  done
  echo 'RED must show a large positive lookup (+N) and a raised alive. If both arms read the'
  echo "same, the differential is not reaching the worker — fix that before reading the matrix."
  if [ "$KIND" = gbm ]; then
    echo
    echo "AND for gbm, the vrend bucket must show charges of exactly the client's buffer size."
    echo "Without that the buffers were venus blobs and this re-ran the venus arm under a"
    echo "different name — a vacuous pass that LOOKS like evidence."
  fi
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# `path3` — the ERROR-PATH PARTIAL.
#
# The matrix's reasoning: "a partially-built object on an error path is the classic dropped
# release — far enough along to have taken a host allocation, not far enough to be registered
# where the sweep will find it." The trigger is real and deterministic (`badmod` asks for a
# modifier no driver implements; KosmicKrisp answers -1000158000, the incident's own error).
#
# But READ THE SHIPPED CODE FIRST, because it decides what a green number here can mean: with
# KK's native VK_EXT_image_drm_format_modifier, a modifier-tiled create reaches the driver
# verbatim and vkr allocates NOTHING before it — the KK-linear IOSurface is allocated only after
# `args->ret == VK_SUCCESS` (vkr_image.c). So the shipped path cannot strand anything on this
# error, and a flat census would certify a vehicle that arms nothing.
#
# Hence the RED: the fault patch takes a host allocation BEFORE the doomed create and, when
# /tmp/limina-fault-leak-errpath exists, drops it on the error path. That is the class, synthesized
# — and it is what earns the right to report the shipped arm's flatness as a result.
# ---------------------------------------------------------------------------------------------
if [ "${1:-matrix}" = "path3" ]; then
  N="${N:-40}"
  echo "=== path 3: the error-path partial ($N doomed creates at $SIZE) ==="
  echo
  # A compositor is running purely so `refs` has something to make the worker allocate with.
  start_comp "" || exit 1
  # TRAP, cost one run: the injector's `access()` runs in the WORKER, on the HOST filesystem.
  # Touching /tmp/limina-fault-leak-errpath over ssh puts it in the GUEST, where nothing reads it
  # — and a RED arm that silently stays green is indistinguishable from a clean result. The
  # "N leak(s) injected" count below is the guard: RED with zero injections is a broken arm.
  #
  # There is no third "shipped" arm here on purpose. This build HAS the injector, which allocates
  # before every doomed create, so nothing run against it can report shipped behaviour. Run
  # `path3 shipped` against a clean build for that — it asserts the pre-create count is zero.
  for arm in "RED (fault: leak on the error path)::yes" "GREEN (fault: free on the error path)::no"; do
    name="${arm%%::*}"; leak="${arm##*::}"
    case "$leak" in
      yes) touch /tmp/limina-fault-leak-errpath ;;
      *)   rm -f /tmp/limina-fault-leak-errpath ;;
    esac
    echo "--- $name ---"
    echo "  before: $(refs)"
    FROM=$(mark)
    g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info \
       ./limina-testcomp badmod $N $SIZE" | grep -aE "BADMOD (DONE|INVALID)" | sed 's/^/  /'
    echo "  after : $(refs)"
    echo "  host  : $(since "$FROM" | grep -ac "vkCreateImage failed host-side: -1000158000") \
refusal(s), $(since "$FROM" | grep -ac "LIMINA-FAULT.*LEAKING") leak(s) injected, \
$(since "$FROM" | grep -ac "LIMINA-FAULT.*pre-create") pre-create alloc(s)"
    echo
  done
  rm -f /tmp/limina-fault-leak-errpath
  echo "RED must raise alive, and its injection count must be nonzero — a RED that injected"
  echo "nothing is a broken arm, not a clean result. GREEN must come back flat."
  exit 0
fi

# `shipped3` — the same trigger against a build WITHOUT the fault patch. This is the arm that
# reports what limina actually does, and its whole content is two assertions: the host refused
# every create, and it allocated nothing before doing so.
if [ "${1:-matrix}" = "shipped3" ]; then
  N="${N:-40}"
  echo "=== path 3, SHIPPED build: $N doomed creates at $SIZE ==="
  start_comp "" || exit 1
  echo "  before: $(refs)"
  FROM=$(mark)
  g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info \
     ./limina-testcomp badmod $N $SIZE" | grep -aE "BADMOD (DONE|INVALID)" | sed 's/^/  /'
  echo "  after : $(refs)"
  refusals=$(since "$FROM" | grep -ac "vkCreateImage failed host-side: -1000158000")
  faults=$(since "$FROM" | grep -ac "LIMINA-FAULT")
  echo "  host  : $refusals refusal(s), $faults fault-injector line(s)"
  [ "$faults" != 0 ] && echo "!! this build still carries the fault patch — not a shipped reading"
  [ "$refusals" = 0 ] && echo "!! no host-side refusal: the guest answered locally, arm is vacuous"
  since "$FROM" | grep -aE "still charged|destroyed with" | sed 's/.*virgl: vkr: /    /' | tail -4
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# `path4` — RING-FATAL teardown.
#
# The matrix asks whether "teardown takes a different branch than the healthy one". Reading
# vkr_context_destroy says it does not: the sweep is gated on ctx->instance and nothing else, so
# fatal or healthy, the same sweep runs. What a FATAL really changes is WHEN — a context whose
# ring is dead keeps every host allocation until its guest process goes away.
#
# So this arm measures both moments: with the compositor's context FATAL but the PROCESS STILL
# ALIVE, and then after killing it. The kill is what should reclaim; the first reading is what
# tells you a wedged-but-alive client holds everything (matrix §7 caveat #1, "the context never
# actually died").
#
# The vehicle: `--hog-mib` allocates over the budget from inside the compositor, which is where
# the borrowed +1s live. `vkr_budget_kills_context` sets the FATAL, deterministically, with no
# need to provoke a real ring fault. The RED is the fault patch's skipped sweep.
# ---------------------------------------------------------------------------------------------
if [ "${1:-matrix}" = "path4" ]; then
  KIND="${2:-venus}"
  case "$KIND" in
    venus) CLIENT_MODE="client-dmabuf"; CLIENT_ENV="" ;;
    gbm)   CLIENT_MODE="client-gbm";    CLIENT_ENV="MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu" ;;
    *) echo "unknown kind $KIND — expected venus or gbm"; exit 1 ;;
  esac
  HOG="${HOG:-4096}"
  echo "=== path 4: ring-FATAL teardown ($KIND client, hog ${HOG} MiB) ==="
  echo
  for arm in "RED (fault: the imported IOSurface release is dropped)::yes" "GREEN (shipped teardown)::no"; do
    name="${arm%%::*}"; skip="${arm##*::}"
    # HOST-side, like path 3's: the injector's access() runs in the worker. Touching it over ssh
    # puts it in the guest, where nothing reads it, and the RED silently runs as a second GREEN.
    case "$skip" in
      yes) touch /tmp/limina-fault-fatal-skip-release ;;
      *)   rm -f /tmp/limina-fault-fatal-skip-release ;;
    esac
    echo "--- $name ---"
    # The counter baseline belongs BEFORE anything in this arm runs. Taken after `refs` it
    # already included the arm's own refusal whenever the hog fired early, and printed a
    # difference of zero for an arm that had refused — a lie in the safest-looking direction.
    R0=$(grep -ac "REFUSING" "$WORKER_LOG"); F0=$(grep -ac "LIMINA-FAULT" "$WORKER_LOG")
    # A SEPARATE log, for the same reason `leakexit` uses one: the fresh compositor started at
    # the end to tick the census would otherwise overwrite /tmp/comp.log, and whether the hog
    # ever fired would be unknowable — the difference between an answer and a number.
    g "sudo -n pkill -f 'limina-testcomp run'; sudo -n rm -f /tmp/limina-testcomp-hog"; sleep 2
    "${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info nohup \
       ./limina-testcomp run --hog-mib $HOG > /tmp/path4-comp.log 2>&1 &" >/dev/null 2>&1 &
    sleep 5
    g "grep -a 'COMPOSITOR READY' /tmp/path4-comp.log" >/dev/null || { echo "!! no compositor"; exit 1; }
    echo "  before        : $(refs)"
    FROM=$(mark)
    # Counters are taken as BEFORE/AFTER totals, not from a line slice. Slicing kept reporting
    # "0 refusals in window" for arms whose refusal is plainly in the log: `refs` sleeps ~15 s
    # before FROM is marked, and anything that fires in that gap lands above the slice. These
    # events are monotonic, so a difference of totals cannot miss one however the timing moves.
    # The client has to still be committing when the hog fires, so the compositor is holding a
    # live import at the moment its context is killed. --park keeps it there.
    "${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV RUST_LOG=info \
       XDG_RUNTIME_DIR=/tmp/testcomp-run WAYLAND_DISPLAY=wayland-1 \
       ./limina-testcomp $CLIENT_MODE 60 $SIZE --park" >/tmp/path4-client.log 2>&1 &
    sleep 8
    grep -aE "CLIENT (COMMITTED|PARKED)" /tmp/path4-client.log | sed 's/^/  /'
    # Only NOW — with the measurement window open and the real client's import live. The trigger
    # is explicit because "hog as soon as any import exists" fired on the census tick client
    # instead, ~15 s earlier, killing the context before the arm had even started measuring.
    g "sudo -n touch /tmp/limina-testcomp-hog"
    # WAIT for the refusal rather than sleeping a guess: the compositor hogs on its next event
    # loop tick and the 4 GiB request takes a second or two to reach the host.
    for _ in $(seq 1 20); do
      [ "$(grep -ac 'REFUSING' "$WORKER_LOG")" != "$R0" ] && break
      sleep 2
    done
    echo "  hog           : $(g 'grep -a "HOG SUBMITTED" /tmp/path4-comp.log' | tr -d '\r')"
    echo "  host refusals : $(( $(grep -ac "REFUSING" "$WORKER_LOG") - R0 )) — $(grep -a "REFUSING" \
      "$WORKER_LOG" | tail -1 | sed 's/.*REFUSING a //' | cut -c1-60)"
    echo "  ring FATAL    : $(since "$FROM" | grep -a "ring FATAL set" | tail -1 \
      | sed 's/.*virgl: vkr: //')"
    # THE READING PATH 4 IS ABOUT: the context is dead, the process holding the imports is not.
    # It only exists because the vehicle survives its own FATAL — a compositor that exits here
    # runs its destructors and the host takes the ordinary `instance was gone` teardown instead.
    echo "  alive?        : $(g "pgrep -f 'limina-test[c]omp run' >/dev/null && echo yes || echo no — IT EXITED, this arm tests the clean path")"
    echo "  ctx FATAL, compositor process STILL ALIVE:"
    echo "    $(refs)"
    g "sudo -n pkill -9 -f 'limina-testcomp client-'"
    g "sudo -n pkill -9 -f 'limina-testcomp run'"
    sleep 4
    # The census ticks on the allocation path, so with both testcomp processes dead nothing
    # emits one — a fresh compositor is the only way to get a reading rather than an empty line.
    start_comp "" >/dev/null 2>&1
    echo "  after both are gone:"
    echo "    $(refs)"
    # A SECOND settled reading. The first one lands while the last context teardown is still in
    # flight, so a RED arm's own leak showed up only in the NEXT arm's baseline — real, but
    # unreadable in place. Two consecutive reads, the venus_fd_census discipline.
    echo "  settled       : $(refs)"
    # Per-ARM provenance for the gbm holder, the standard M3c set: without it this arm could be
    # running venus blobs behind a gbm API and every label would still say vrend.
    [ "$KIND" = gbm ] && vrend_bucket
    echo "  fault fired   : $(( $(grep -ac "LIMINA-FAULT" "$WORKER_LOG") - F0 )) time(s)"
    echo "  --- host lines ---"
    since "$FROM" | grep -aE "destroying context|destroyed —|destroyed with|LIMINA-FAULT|still holds" \
      | sed 's/.*virgl: vkr: /    /;s/.*virgl_renderer\] /    /' | tail -8
    echo
  done
  # HOST-side, like every other touch/rm of a selector file: the injector's access() runs in the
  # worker. `g "rm ..."` here would delete a guest file nothing ever reads.
  rm -f /tmp/limina-fault-fatal-skip-release
  echo "RED must hold references the GREEN arm gives back. Read the FATAL-but-alive line too:"
  echo "that is the state the 2026-08-06 incident was in for 38 s before the jetsam kill."
  exit 0
fi

# ---------------------------------------------------------------------------------------------
# `leakexit` — path 1', the one every other arm misses.
#
# Every kill in this script goes through the LIVE-instance full sweep (that is the §2 correction).
# The bare-free() branch takes only what SURVIVES that sweep, so to probe it the compositor has to
# exit CLEANLY — running its destructors, destroying its VkDevice and VkInstance explicitly —
# while still holding imports it never released. That is `run <frames> --leak-imports`: the
# frame bound makes it exit on its own, and Render::drop deliberately skips the import sweep.
#
# MEASURED 2026-08-07, both holders, `COMPOSITOR DONE frames=400 imported=1 evicted=0` with
# self-exited=yes: alive 2 -> 2, lookup +0, and the vrend bucket back to 0 B live. The destroy
# line reads `instance was gone, 0 objects and 0 resources left in the tables` — so the
# bare-free() branch ran with NOTHING to free.
#
# The reason is structural: Vk::drop destroys the VkInstance, and vkr_instance_destroy sweeps the
# whole object hierarchy under it, leaked VkDeviceMemory included. Reaching bare-free() with
# anything in hand needs objects that are NOT reachable from the instance — which a leaked import
# is not. So "the compositor quit and host memory stayed" cannot be explained by a client-buffer
# leak on EITHER holder, however deliberately the guest leaks.
# ---------------------------------------------------------------------------------------------
if [ "${1:-matrix}" = "leakexit" ]; then
  KIND="${2:-venus}"
  case "$KIND" in
    venus) CLIENT_MODE="client-dmabuf"; CLIENT_ENV="" ;;
    gbm)   CLIENT_MODE="client-gbm";    CLIENT_ENV="MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu" ;;
    *) echo "unknown kind $KIND — expected venus or gbm"; exit 1 ;;
  esac
  echo "=== path 1': compositor exits CLEANLY while leaking imports ($KIND client) ==="
  start_comp "" >/dev/null 2>&1
  echo "  before alive : $(refs)"
  FROM=$(mark)

  g "sudo -n pkill -f 'limina-testcomp run'"; sleep 2
  # A SEPARATE log: start_comp's redirect would overwrite /tmp/comp.log with the fresh
  # measurement compositor's output, and then whether this one exited cleanly is unknowable —
  # which is the difference between an answer and a number.
  "${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD RUST_LOG=info nohup \
     ./limina-testcomp run 400 --leak-imports > /tmp/leakexit.log 2>&1 &" >/dev/null 2>&1 &
  sleep 4
  g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV XDG_RUNTIME_DIR=/tmp/testcomp-run \
     WAYLAND_DISPLAY=wayland-1 ./limina-testcomp $CLIENT_MODE 20 $SIZE" \
    | grep -aE "CLIENT (COMMITTED|DONE)" | sed 's/^/  /'
  # Wait for the frame bound to fire and the process to leave ON ITS OWN. It only renders when
  # there is something to render, so the client must still be committing while the count runs
  # out — hence the 20 s client above rather than a brief one.
  self_exited=no
  for _ in $(seq 1 30); do
    if ! g "pgrep -f 'limina-test[c]omp run'" >/dev/null 2>&1; then self_exited=yes; break; fi
    sleep 2
  done
  # THE VALIDITY GATE for this arm. No COMPOSITOR DONE means the process did not reach its own
  # exit path, so nothing ran its destructors and this arm tested a kill wearing a clean-exit
  # label. Refuse to report a number in that case rather than publish an unreadable one.
  done_line=$(g 'grep -a "COMPOSITOR DONE" /tmp/leakexit.log' | tr -d '\r')
  echo "  self-exited  : $self_exited"
  echo "  exit line    : ${done_line:-<none — see below>}"
  if [ -z "$done_line" ]; then
    echo
    echo "!! INVALID ARM: the compositor never printed COMPOSITOR DONE, so it did not exit"
    echo "!! through its own destructors. Whatever the counts below say, this did NOT test the"
    echo "!! clean-exit path. Raise the frame bound or keep the client committing for longer."
    g 'tail -4 /tmp/leakexit.log' | sed 's/^/    /'
  fi
  sleep 3
  start_comp "" >/dev/null 2>&1
  echo "  after alive  : $(refs)"
  [ "$KIND" = gbm ] && vrend_bucket
  echo "  --- host lines ---"
  since "$FROM" | grep -aE "destroying context|destroyed —|destroyed with|still holds" \
    | sed 's/.*virgl: vkr: /    /;s/.*virgl_renderer\] /    /' | tail -6
  exit 0
fi

# The teardown paths run against either holder. `matrix gbm` is M3c: vrend owns the IOSurface and
# venus holds only the borrowed +1, so a venus-side teardown that skips vkr_device_memory_release
# strands the surface with NO owner left. `matrix` alone is M3b's venus/venus case.
KIND="${2:-venus}"
case "$KIND" in
  venus) CLIENT_MODE="client-dmabuf"; CLIENT_ENV="" ;;
  gbm)   CLIENT_MODE="client-gbm";    CLIENT_ENV="MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu" ;;
  *) echo "unknown kind $KIND — expected venus or gbm"; exit 1 ;;
esac

echo "=== teardown matrix — $KIND-allocated client buffers, size $SIZE ==="
echo

# ---------------------------------------------------------------------------------------------
# Path 1 — clean exit. The client destroys its images, device and instance on the way out.
# Baseline: residual must be zero. Note this is ALSO the path that can leave orphans behind,
# since a destroyed instance is what sends vkr down the bare-free() branch for anything left.
# ---------------------------------------------------------------------------------------------
echo "--- path 1: clean exit ---"
start_comp "" || exit 1
FROM=$(mark); echo "  before alive   : $(force_census)"
g "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp $CLIENT_MODE 8 $SIZE" | grep -E "CLIENT (COMMITTED|DONE)" | sed 's/^/  /'
sleep 3
report "clean" "$FROM"
echo "  compositor     : $(g 'grep -ac "evicted a client dmabuf" /tmp/comp.log') eviction(s)"
echo

# ---------------------------------------------------------------------------------------------
# Path 2 — abrupt exit (SIGKILL), with the compositor holding the buffer so the kill lands with
# it committed and unreleased. Per the correction above, expect the FULL sweep here.
# ---------------------------------------------------------------------------------------------
echo "--- path 2: SIGKILL, buffer committed and unreleased ---"
start_comp "--hold-buffers" || exit 1
FROM=$(mark); echo "  before alive   : $(force_census)"
"${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp $CLIENT_MODE 60 $SIZE --park" >/tmp/m3b-client.log 2>&1 &
sleep 6
grep -aE "CLIENT (COMMITTED|PARKED)" /tmp/m3b-client.log | sed 's/^/  /'
g "sudo -n pkill -9 -f 'limina-testcomp client-'"
echo "  SIGKILLed"
sleep 4
report "sigkill" "$FROM"
echo "  compositor     : $(g 'grep -ac "evicted a client dmabuf" /tmp/comp.log') eviction(s)"
echo

# ---------------------------------------------------------------------------------------------
# Path 2b — the IMPORTER dies. The borrowed +1 lives on the compositor's side
# (mem->imported_iosurface), so killing the *client* only ever tests exporter-side death. This is
# the side that matches the observed compositor-quit residual.
# ---------------------------------------------------------------------------------------------
echo "--- path 2b: SIGKILL the COMPOSITOR while it holds a live import ---"
start_comp "--hold-buffers" || exit 1
FROM=$(mark); echo "  before alive   : $(force_census)"
"${SSH[@]}" "cd /tmp && sudo -n env VK_DRIVER_FILES=$ICD $CLIENT_ENV RUST_LOG=info XDG_RUNTIME_DIR=/tmp/testcomp-run \
   WAYLAND_DISPLAY=wayland-1 ./limina-testcomp $CLIENT_MODE 60 $SIZE --park" >/tmp/m3b-client2.log 2>&1 &
sleep 6
g "sudo -n pkill -9 -f 'limina-testcomp run'"
echo "  compositor SIGKILLed (client still alive, holding its export)"
sleep 4
report "importer-death" "$FROM"
g "sudo -n pkill -9 -f 'limina-testcomp client-'"
sleep 3
# A fresh compositor, purely to make the worker allocate again: the census ticks on the
# allocation path, and with both testcomp processes dead nothing does. Without this the final
# read comes back EMPTY — which is not the same as zero, and reads like one.
start_comp "" >/dev/null 2>&1
echo "  after both are gone: alive=$(force_census)"
echo

echo "=== done ==="
echo "Read the 'destroying context' lines above: 'instance was live' means the full sweep ran"
echo "(so vkr_device_memory_release dropped the +1); 'instance was gone' means the bare-free()"
echo "branch took whatever was left. A residual with a live instance is the interesting result."
