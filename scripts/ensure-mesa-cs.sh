#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Ensure the case-sensitive Mesa volume (/Volumes/mesa-cs) is mounted.
#
# The host KosmicKrisp and zink-on-KK builds live on third_party/mesa-cs.sparseimage,
# not in the repo. macOS drops the mount on reboot, which makes every consumer of
# /Volumes/mesa-cs (build-app.sh, build-virglrenderer.sh, the venus tests, …) fail with
# a missing-file error that looks like a lost build. Re-attach it instead.
#
# Source it (`. scripts/ensure-mesa-cs.sh`) or run it standalone; idempotent either way.
set -euo pipefail

MESA_CS_MOUNT="/Volumes/mesa-cs"
MESA_CS_IMAGE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/third_party/mesa-cs.sparseimage"

if [ ! -d "$MESA_CS_MOUNT" ]; then
  if [ ! -e "$MESA_CS_IMAGE" ]; then
    echo "MISSING $MESA_CS_IMAGE — the host KK/zink Mesa builds live there; see docs/codebases.md" >&2
    exit 1
  fi
  echo "==> $MESA_CS_MOUNT not mounted; attaching $(basename "$MESA_CS_IMAGE")"
  hdiutil attach "$MESA_CS_IMAGE" >/dev/null
  [ -d "$MESA_CS_MOUNT" ] || { echo "attached $MESA_CS_IMAGE but $MESA_CS_MOUNT still absent" >&2; exit 1; }
fi
