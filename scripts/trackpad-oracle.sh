#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Judge the trackpad policy against a real-hand recording with the guest's real libinput.
#
#   scripts/trackpad-oracle.sh <recording.jsonl> <ssh-port> [fuzz] [--verbose]
#
# Replays the recording (LIMINA_TRACKPAD_RECORD output) through the policy on the host (the
# `dump_replay` test), copies the result into a booted guest, and runs
# scripts/trackpad-oracle/guest-replay.py there as root: uinput clones of limina's touchpad
# and pointer, the events at their recorded times, and libinput counting the clicks. Exits
# with the guest's verdict, after a line measuring the touchpad's two-finger scrolls (the
# per-scroll table is left in the guest's /tmp/trackpad-oracle-scroll.txt). `fuzz` sets the
# clone's MT position fuzz; `--verbose` logs libinput's gesture state machine into
# /tmp/trackpad-oracle-libinput.log. The guest needs only stock libinput and python3.
set -euo pipefail

recording="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
port=$2
shift 2
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
replay="$root/target/trackpad-replay.txt"

(cd "$root" && TRACKPAD_RECORDING="$recording" TRACKPAD_REPLAY_OUT="$replay" \
    cargo test -q -p limina dump_replay -- --ignored --exact \
    window::trackpad::recordings::dump_replay >/dev/null)

ssh_opts=(-p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
ssh "${ssh_opts[@]}" claude@127.0.0.1 'cat > /tmp/trackpad-replay.txt' < "$replay"
ssh "${ssh_opts[@]}" claude@127.0.0.1 'cat > /tmp/guest-replay.py' < "$here/trackpad-oracle/guest-replay.py"
# The built-in trackpad's surface, as limina advertises it (0.01 mm units).
ssh "${ssh_opts[@]}" claude@127.0.0.1 \
    "sudo python3 /tmp/guest-replay.py /tmp/trackpad-replay.txt 12480 7680 $*"
