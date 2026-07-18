#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 ACCEPTANCE — the full managed-VM suspend/resume flow through the real CLI verbs:
#   limina create -> limina start (bg) -> limina suspend -> limina start (restore).
# Asserts: (1) `suspend` snapshots + tears the VM down and state.toml records [suspended] with a
# snapshot.bin on disk; (2) the next `start` RESTORES it (same boot_id = resumed, not rebooted; live
# net) and CLEARS [suspended] (consume-on-start). Headless managed VM (M9.2 is headless-scoped;
# windowed/GPU restore is M9.3).
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/managed-rt.log"; DISK="$JOB/tmp/managed-rt.raw"; LIB="$JOB/tmp/managed-lib"
FW="$REPO/target/krun-efi/KRUN_EFI.gop.fd"
PORT=2233
LIMINA="$REPO/target/debug/limina"
BUNDLE="$LIB/mrt.liminavm"
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
ssh_up(){ $SSH true 2>/dev/null; }
cleanup(){ pkill -9 -f "managed-rt.raw" 2>/dev/null; pkill -9 -f "mrt.liminavm" 2>/dev/null; }
cd "$REPO"

rm -rf "$LIB"; mkdir -p "$LIB"
say "cloning stock F44..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

say "limina create (headless, ssh-port $PORT, 2G dynamic)"
"$LIMINA" create mrt --disk "$DISK" --in-place --no-window --ssh-port "$PORT" --cpus 2 --memory 2G --dir "$LIB" 2>&1 | tee -a "$LOG"
[ -d "$BUNDLE" ] || { say "create failed"; exit 1; }
# Headless boots need explicit firmware (windowed auto-resolves GOP). Inject into the empty [boot].
/usr/bin/sed -i '' "s|^\[boot\]$|[boot]\nfirmware = \"$FW\"|" "$BUNDLE/vm.toml"
say "boot.firmware set: $(grep firmware "$BUNDLE/vm.toml")"

### START #1 (cold boot)
say "START#1: limina start (background)"
RUST_LOG=limina=info,limina_vmm=info caffeinate -dimsu "$LIMINA" start "$BUNDLE" > "$JOB/tmp/managed-start1.log" 2>&1 &
SUP1=$!; say "supervisor#1 pid=$SUP1"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; cleanup; exit 1; }
BOOTID_PRE=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
CURL_PRE=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
say "pre: boot_id=$BOOTID_PRE curl=$CURL_PRE"

### SUSPEND
say "SUSPEND: limina suspend (own process; SIGTSTP -> supervisor -> bracket)"
"$LIMINA" suspend "$BUNDLE" 2>&1 | tee -a "$LOG"; SUS_RC=${pipestatus[1]:-$?}
say "suspend verb rc=$SUS_RC"
# supervisor#1 should have exited 126 and persisted [suspended].
wait "$SUP1" 2>/dev/null; SUP1_RC=$?
say "supervisor#1 exit rc=$SUP1_RC (want 126)"
say "state.toml:"; cat "$BUNDLE/state.toml" 2>/dev/null | tee -a "$LOG"
SNAP_SZ=$(ls -la "$BUNDLE/run/snapshot.bin" 2>/dev/null | awk '{print $5}')
say "snapshot.bin: ${SNAP_SZ:-MISSING}"
# NOTE: `grep -c` prints "0" AND exits 1 on no match, so `|| echo 0` would double the output — use
# a trailing `|| true` to swallow the exit while keeping grep's own count.
HAS_SUS=$(grep -c '\[suspended\]' "$BUNDLE/state.toml" 2>/dev/null || true)
if [ "$SUP1_RC" != 126 ] || [ -z "$SNAP_SZ" ] || [ "$HAS_SUS" -lt 1 ]; then
  say "VERDICT: SUSPEND FAILED (rc=$SUP1_RC snap=$SNAP_SZ has_suspended=$HAS_SUS)"
  cleanup; exit 1
fi
say "suspend OK: torn down, [suspended] persisted, snapshot on disk"

### START #2 (restore)
say "START#2: limina start (should --restore from state.toml)"
RUST_LOG=limina=info,limina_vmm=info caffeinate -dimsu "$LIMINA" start "$BUNDLE" > "$JOB/tmp/managed-start2.log" 2>&1 &
SUP2=$!; say "supervisor#2 pid=$SUP2"
back=0; for i in $(seq 1 40); do sleep 3; ssh_up && { back=1; say "SSH BACK ~$((i*3))s"; break; }; done
if [ "$back" = 1 ]; then
  BOOTID_POST=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
  CURL_POST=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
  say "post: boot_id=$BOOTID_POST curl=$CURL_POST"
  say "state.toml after restore:"; cat "$BUNDLE/state.toml" 2>/dev/null | tee -a "$LOG"
  CLEARED=$(grep -c '\[suspended\]' "$BUNDLE/state.toml" 2>/dev/null || true)
  say "restore worker log:"; grep -iE 'restor|resuming suspended|wake|s2idle' "$JOB/tmp/managed-start2.log" | tail -6 | tee -a "$LOG"
  if [ "$BOOTID_PRE" = "$BOOTID_POST" ] && [ -n "$BOOTID_POST" ] && [ "$CURL_POST" = 200 ] && [ "$CLEARED" = 0 ]; then
    say "VERDICT: MANAGED SUSPEND/RESUME GREEN — resumed same boot ($BOOTID_POST), live net, [suspended] consumed"
  else
    say "VERDICT: PARTIAL/FAIL — pre=$BOOTID_PRE post=$BOOTID_POST curl=$CURL_POST suspended_still_present=$CLEARED"
  fi
else
  say "VERDICT: FAIL — VM did not come back after restore"
  grep -iE 'restor|error|panic|resuming' "$JOB/tmp/managed-start2.log" | tail -15 | tee -a "$LOG"
fi
say "cleanup"; kill -9 $SUP2 2>/dev/null; cleanup; rm -rf "$LIB" "$DISK"; say "managed spike done."
