#!/bin/bash
# Run one consumer script in the P0 guest and keep the slice of the swtpm log it produced.
#
#   spikes/vtpm-p0/capture.sh <name> <consumer script>
#
# The consumer script runs in the guest as `claude` (passwordless sudo) via `bash -s`. Its
# output goes to corpus/<name>.out and the TPM traffic to corpus/<name>.swtpm.log, which
# decode.py turns into corpus/<name>.jsonl. Anything else the guest kernel sends in the same
# window (the resource manager's context swaps, hwrng reads) lands in the slice too.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="${VTPM_P0_WORK:-$here/work}"
name="${1:?usage: capture.sh <name> <consumer script>}"
script="${2:?usage: capture.sh <name> <consumer script>}"
SSH_PORT="${SSH_PORT:-2240}"
mkdir -p "$here/corpus"

log="$work/swtpm.log"
before=$(wc -l < "$log")
set +e
ssh -p "$SSH_PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    claude@127.0.0.1 'bash -s' < "$script" > "$here/corpus/$name.out" 2>&1
rc=$?
set -e
after=$(wc -l < "$log")
sed -n "$((before + 1)),${after}p" "$log" > "$here/corpus/$name.swtpm.log"
python3 "$here/decode.py" "$here/corpus/$name.swtpm.log" > "$here/corpus/$name.jsonl"
echo "$name: consumer exit $rc, $((after - before)) log lines, $(wc -l < "$here/corpus/$name.jsonl") commands"
exit "$rc"
