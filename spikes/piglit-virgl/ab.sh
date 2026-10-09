#!/bin/bash
# ab.sh <disk.raw> <kk-icd.json> <regex> <out-prefix> — one arm of a KK A/B over piglit.
# Boots <disk.raw> (a clone of a guest that has piglit built: setup-guest.sh) on the KosmicKrisp
# ICD given, runs every piglit gpu test whose name or command matches <regex> once (rep.sh), and
# powers off. Writes <out-prefix>.list (the tests), <out-prefix>.txt (rep.sh's "<result>\t<name>"
# lines) and <out-prefix>.worker.log. Run it once per KK build on the same disk and compare:
#   join -t$'\t' -1 2 -2 2 <(sort -t$'\t' -k2 old.txt) <(sort -t$'\t' -k2 new.txt) | awk -F'\t' '$2!=$3'
# A result column made only of rcNNN means the list was malformed and nothing ran.
set -u
DISK=${1:?disk}; ICD=${2:?icd json}; RE=${3:?regex}; OUT=${4:?out prefix}
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
LOG=/tmp/limina-worker-$(basename "${DISK%.raw}").log
SSH=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=error)
# The waiter takes the last port line in the log; a leftover log hands it a stale port.
rm -f "$LOG"
LIMINA_KK_ICD=$ICD LIMINA_DISK=$DISK LIMINA_CPUS=4 LIMINA_RAM_MIB=8192 \
  RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT.boot.out" 2>&1 &
boot=$!
port=$(scripts/wait-guest-ssh.sh "$LOG" 400 "$boot") || { echo "boot failed"; kill "$boot"; exit 1; }
poweroff() {
  ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "sudo systemctl poweroff" >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$boot" 2>/dev/null || break; sleep 3; done
  kill -0 "$boot" 2>/dev/null && { echo "poweroff timed out"; kill "$boot"; }
  wait "$boot" 2>/dev/null
}
scp -q -P "$port" "${SSH[@]}" spikes/piglit-virgl/rep.sh claude@127.0.0.1:
# piglit's --format does not expand \t, and {command} carries bin/ and -auto, which rep.sh adds:
# build the list with a plain separator and rewrite it. glx@ tests need X, which gbm lacks.
ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 "cd ~/piglit && ./piglit print-cmd --format '{name}|||{command}' gpu 2>/dev/null |
  grep -iE '$RE' | grep -v '^glx@' |
  sed -e 's/|||bin\//\t/' -e 's/|||/\t/' -e 's/ -auto//g' -e 's/ -fbo//g' > ~/ab.list"
scp -q -P "$port" "${SSH[@]}" claude@127.0.0.1:ab.list "$OUT.list"
if [ ! -s "$OUT.list" ] || awk -F'\t' 'NF!=2 || $2 ~ /^bin\// {bad=1} END {exit !bad}' "$OUT.list"; then
  echo "malformed or empty list: $OUT.list"; poweroff; exit 1
fi
echo "$(wc -l < "$OUT.list") tests"
gtimeout --kill-after=30 7200 ssh -p "$port" "${SSH[@]}" claude@127.0.0.1 'bash ~/rep.sh ~/ab.list 1' > "$OUT.txt"
echo "rep rc=$? results=$(wc -l < "$OUT.txt")"
cp "$LOG" "$OUT.worker.log"
poweroff
