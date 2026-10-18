#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Fills the compute battery's tools disk (scripts/perf-compute.sh). The tools live on a disk of
# their own, attached next to a fresh clone of metal.test at every run, so a driver delivery to
# metal.test reaches the compute rows with nothing to redo here. Rebuild the disk when the MLX
# pin, LLAMA_CPP_REV, MLX_LM or the image's Python moves.
#
# Boot a throwaway clone of metal.test with the disk attached, then run this against it:
#   truncate -s 64G $PERF_HOME/tools.raw
#   cp -c Fedora-Workstation-44.metal.test.raw $PERF_HOME/work.noindex/provision.raw
#   LIMINA_GPU_METAL=1 LIMINA_CPUS=8 LIMINA_RAM_MIB=16384 LIMINA_NET=1 \
#     LIMINA_EXTRA_ARGS="--disk $PERF_HOME/tools.raw" LIMINA_DISK=$PERF_HOME/work.noindex/provision.raw \
#     spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &
#   scripts/perf/compute/tools-disk.sh $(scripts/wait-guest-ssh.sh /tmp/limina-worker-provision.log)
# then power the guest off and delete the clone. The clone's root takes the build tools; the disk
# takes nothing from it but what tools-disk.sh copies (guest-provision.sh says what).
#
# PERF_HOME (default: the main checkout's target/perf-compute) holds the disk, the models and the
# native control's builds. Models are fetched once, by models.sh.
# Usage: scripts/perf/compute/tools-disk.sh <ssh-port>
set -euo pipefail
PORT=${1:?ssh port}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(git -C "$HERE" rev-parse --show-toplevel)
. "$HERE/env.sh"
O=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
g() { gtimeout --kill-after=10 "$1" sshpass -p "$GUEST_PASS" ssh -p "$PORT" "${O[@]}" "$GUEST_USER@127.0.0.1" "$2"; }

# Host artifacts: limina-metal, the MLX prefix and source (MLX's pinned fork), the bench metallib,
# llama.cpp's source at its pin.
"$ROOT/spikes/mlx-linux/host.sh" "$MLX_TREE"
xcrun -sdk macosx metal -o "$PERF_HOME/metal-bench.metallib" "$HERE/metal-bench.metal"
"$HERE/llama-src.sh"
[ -f "$PERF_HOME/models/gguf/Qwen2.5-7B-Instruct-Q4_0.gguf" ] || { echo "no models: run models.sh" >&2; exit 1; }

g 60 'set -e; d=/dev/disk/by-id/virtio-disk1
  sudo blkid "$d" >/dev/null || sudo mkfs.ext4 -q -L perf-compute "$d"
  sudo mkdir -p /opt/perf-compute; mountpoint -q /opt/perf-compute || sudo mount "$d" /opt/perf-compute
  sudo chown claude: /opt/perf-compute; rm -rf /opt/perf-compute/stage; mkdir /opt/perf-compute/stage'
S="$ROOT/spikes/mlx-linux/out"
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -cf - \
    -C "$S" prefix mlx-src.tar \
    -C "$ROOT/spikes/metal-cpp-shim/out" liblimina_metal.a \
    -C "$HERE" metal-bench.cpp mlx_bench.py guest-provision.sh \
    -C "$PERF_HOME" metal-bench.metallib llama.cpp-src.tar models |
  gtimeout --kill-after=10 3600 sshpass -p "$GUEST_PASS" ssh -p "$PORT" "${O[@]}" "$GUEST_USER@127.0.0.1" \
    'tar -C /opt/perf-compute/stage -xf -'
g 7200 "bash /opt/perf-compute/stage/guest-provision.sh $LLAMA_CPP_REV $MLX_LM"
g 60 'sync; sudo umount /opt/perf-compute'
echo "tools disk ready: $PERF_HOME/tools.raw"
