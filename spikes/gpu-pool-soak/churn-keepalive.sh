#!/bin/bash
# Keep the soak's churn presenter alive for the whole run.
#
# WHY. The first soak run went quiet after 50 minutes: kmschurn died at buffer 197,601 with
#   CHURN FAIL drmModeAddFB2WithModifiers rc=-22 handle=3 stride=0
# and nothing noticed. The host pool then read "flat" for an hour — flat because NOTHING WAS
# RUNNING, not because retention is healthy. A soak whose workload can stop without the sampler
# knowing measures an idle VM and reads like a pass.
#
# So: poll for the presenter, restart it when it is gone, and record every restart with the
# failing tail. The restart count is itself a finding — an allocation that fails after ~200k
# buffers is the retention question in a different costume.
#
# Usage: churn-keepalive.sh <out-dir> <ssh-port> [hours]
set -u
OUT=${1:?out dir}; PORT=${2:?ssh port}; HOURS=${3:-24}
SSH="ssh -p $PORT -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 claude@127.0.0.1"
VENUS_ENV="VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json"
FRAMES=$(( HOURS * 3600 * 60 + 100000 ))

START=$(date +%s); END=$(( START + HOURS * 3600 )); n=0
while [ "$(date +%s)" -lt "$END" ]; do
    # Bracket the first char: a plain `pgrep -f kmschurn.py` also matches the ssh-spawned shell
    # carrying that very string in its argv, so the check can never fail and the watchdog is inert.
    if ! $SSH 'pgrep -f "[k]mschurn.py" >/dev/null' 2>/dev/null; then
        n=$(( n + 1 ))
        {
            echo "=== restart #$n $(date +%H:%M:%S) after:"
            $SSH 'tail -3 /tmp/kmschurn.log' 2>/dev/null
        } >> "$OUT/churn-restarts.log"
        $SSH "mv -f /tmp/kmschurn.log /tmp/kmschurn.log.$n 2>/dev/null; \
              nohup sudo -n env $VENUS_ENV python3 /tmp/kmschurn.py churn-vk $FRAMES 2 1280 800 \
              > /tmp/kmschurn.log 2>&1 & echo restarted" >/dev/null 2>&1
    fi
    sleep 60
done
echo "keepalive done $(date +%H:%M:%S), $n restarts" >> "$OUT/churn-restarts.log"
