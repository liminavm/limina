#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Install limina-agent into a RUNNING guest over SSH (the limina --net forward, port 2222).
#
# The hand-install path until the sysext guest-tools image exists (roadmap M5): builds
# the static musl agent, copies it + its systemd unit in, enables it. Idempotent —
# re-running upgrades the binary and restarts the unit.
#
# To make it durable, run this against a bake boot of the golden image
# (LIMINA_DISK=$PWD/Fedora-Workstation-43.dev-enh.raw spikes/venus-draw-probe/boot-seated-kk.sh),
# then sync + poweroff; a boot-seated-kk.sh clone loses it at the next reboot.
#
# NOTE (stock guests): on an SELinux-Enforcing guest, files landing in /usr/local/bin
# via scp+install may need `restorecon` — our dev guest runs selinux=0, so this script
# doesn't handle it yet (tracked under the M5 sysext delivery design).
set -euo pipefail
cd "$(dirname "$0")/.."

SSH_OPTS=(-p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)
SCP_OPTS=(-P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)
HOST=claude@127.0.0.1

echo "==> building limina-agent (aarch64-unknown-linux-musl, static)"
( cd guest && cargo build --release -p limina-agent )
BIN=guest/target/aarch64-unknown-linux-musl/release/limina-agent

echo "==> copying into the guest"
scp "${SCP_OPTS[@]}" "$BIN" guest/limina-agent/limina-agent.service "$HOST":/tmp/

echo "==> installing + enabling the unit"
ssh "${SSH_OPTS[@]}" "$HOST" '
    set -e
    sudo install -m 0755 /tmp/limina-agent /usr/local/bin/limina-agent
    sudo install -m 0644 /tmp/limina-agent.service /etc/systemd/system/limina-agent.service
    sudo systemctl daemon-reload
    sudo systemctl enable --now limina-agent
    sleep 1
    systemctl is-active limina-agent
    journalctl -u limina-agent -n 3 --no-pager
'
echo "==> done — check the host limina log for \"guest agent connected: limina-agent/\""
