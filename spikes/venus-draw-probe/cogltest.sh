#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Usage: cogltest.sh "<COGL_DEBUG value>" "<CLUTTER_PAINT value>" <tag>
# Only writes non-empty vars (empty assignment makes environment.d reject the whole file).
SSH="ssh -o ConnectTimeout=8 -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1"
COGL="$1"; CPAINT="$2"; TAG="$3"
{
  echo "rm -f ~/.config/environment.d/zz-cogldebug.conf"
  echo "f=~/.config/environment.d/zz-cogldebug.conf"
  [ -n "$COGL" ]   && echo "echo 'COGL_DEBUG=$COGL' >> \$f"
  [ -n "$CPAINT" ] && echo "echo 'CLUTTER_PAINT=$CPAINT' >> \$f"
  echo "echo '--- wrote:'; cat \$f 2>/dev/null || echo '(none)'"
} | $SSH "bash -s"
$SSH "sudo pkill -9 -f texfan; sudo pkill -9 -f primtest; sudo systemctl stop user@1000.service >/dev/null 2>&1; sudo systemctl restart gdm" >/dev/null 2>&1
echo "reseated tag=$TAG (cogl='$COGL' clutter='$CPAINT')"
