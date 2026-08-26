#!/usr/bin/env bash
# Boot the poke VM with a triggered Metal capture aimed at the FAILING label pass, addressed to an
# attached Xcode rather than to a .gputrace file.
#
# The file destination is not an option: Apple's GPUToolsCapture segfaults in its resource-download
# path whenever the window spans that pass (2 of 2, evidence/gputoolscapture-segv-on-commit.ips).
# Leaving KK_LIMINA_CAPTURE_DIR unset selects MTLCaptureDestinationDeveloperTools instead, which
# does not run that path -- but it delivers to an attached developer tool, so Xcode must be
# attached to the worker BEFORE the trigger fires.
#
# Order matters and is easy to get wrong:
#   1. ./attach-capture-boot.sh                 # boots, waits for ssh, prints the worker pid
#   2. open xcode-attach/Package.swift          # Xcode greys out Debug -> Attach with no workspace
#   3. Xcode: Debug -> Attach to Process -> by PID -> the pid printed in step 1
#   4. ./metal-capture.sh <port> <outdir> 1     # touches the trigger, posts a card, scores it
#
# The lever opens and closes the window itself, so no capture button is needed. It arms on a
# 968x44 pass into an attachment already rendered once -- the repeat render, which is the one that
# produces nothing -- and closes at the commit carrying it.
#
# The worker MUST be signed with get-task-allow or attaching does not fail politely -- the kernel
# SIGKILLs it, no crash report, and the VM just vanishes mid-session. LIMINA_SIGN_DEBUGGABLE=1 is
# exported here rather than left to the caller because `xtask run` re-signs on every boot, so a
# worker signed debuggable by hand is silently replaced by a non-attachable one seconds later.
set -eu
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
export LIMINA_SIGN_DEBUGGABLE=1
DISK="${LIMINA_DISK:-poke-stock-0824.raw}"
SCRATCH="${LIMINA_SCRATCH:-$PWD/poke-build-scratch.raw}"
LOG="/tmp/limina-worker-${DISK%.raw}.log"

rm -f /tmp/limina-kk-capture-trigger
MTL_CAPTURE_ENABLED=1 \
KK_LIMINA_CAPTURE=968x44 KK_LIMINA_CAPTURE_ARM=568x44 KK_LIMINA_CAPTURE_REPEAT=1 \
KK_LIMINA_CAPTURE_TRIGGER=/tmp/limina-kk-capture-trigger \
KK_LIMINA_CAPTURE_PASSES=1 KK_LIMINA_CAPTURE_MAX_CBS=4 KK_LIMINA_CAPTURE_RUNS=4 \
KK_LIMINA_VP_LOG=1 LIMINA_GLOBAL_SCANOUT=1 RUST_LOG=limina=info \
nohup xtask/target/debug/xtask run --disk "$DISK" -- --disk "$SCRATCH" >/tmp/capboot.log 2>&1 &

port=$(scripts/wait-guest-ssh.sh "$LOG" 300)
spikes/notification-text-corruption/ensure-input.sh "$port" >/dev/null

grep -m1 "LIMINA-KK-CAPTURE. armed" "$LOG" || { echo "capture lever never armed -- check $LOG" >&2; exit 1; }
echo
echo "worker pid : $(pgrep -f "target/debug/limina-vmm .*$DISK" | tail -1)"
echo "ssh port   : $port"
echo "worker log : $LOG"
echo "            tail -f $LOG | grep -E 'LIMINA-KK-CAPTURE|dispatch area 968x44'"
echo
echo "Attach Xcode to the pid above, then:"
echo "  spikes/notification-text-corruption/metal-capture.sh $port caprun 1"
