#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build limina-agent-session and install it into the running guest over SSH
# (dev-image convenience until sysext delivery lands; see docs/roadmap.md M5).
#
# Usage: scripts/install-session-helper.sh
# Needs: guest booted with --net (ssh -p 2222 claude@127.0.0.1, passwordless sudo).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1)
SCP=(scp -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null)

echo "== building limina-agent-session (musl static)"
(cd guest && cargo build --release -p limina-agent-session)
BIN=guest/target/aarch64-unknown-linux-musl/release/limina-agent-session
UNIT=guest/limina-agent-session/limina-agent-session.service

echo "== copying into the guest"
"${SCP[@]}" "$BIN" "$UNIT" claude@127.0.0.1:/tmp/

echo "== installing + enabling (systemd user unit)"
"${SSH[@]}" '
set -e
sudo install -m 0755 /tmp/limina-agent-session /usr/local/bin/limina-agent-session
sudo install -m 0644 /tmp/limina-agent-session.service /usr/lib/systemd/user/limina-agent-session.service
export XDG_RUNTIME_DIR=/run/user/$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus
systemctl --user daemon-reload
systemctl --user enable limina-agent-session
# restart, not enable --now: an already-running unit would keep the OLD binary loaded
systemctl --user restart limina-agent-session
sleep 1
systemctl --user is-active limina-agent-session
journalctl --user -u limina-agent-session -n 5 --no-pager
'
echo "== done"
