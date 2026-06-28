#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Helper: ssh into the s4-hibernate spike guest (limina --net, port 2222, user claude).
# Usage: bash spikes/s4-hibernate/gssh.sh '<remote command>'
exec ssh -p 2222 \
  -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null \
  -o BatchMode=yes \
  -o ConnectTimeout=8 \
  claude@127.0.0.1 "$@"
