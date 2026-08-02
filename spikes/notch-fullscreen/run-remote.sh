#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the notch probe on ANOTHER Mac and print the commands that run each arm.
#
# Why remote: on 2026-08-01 the local measurement (dev-mac, 14" M1 Max, panel at its Default
# scaled mode) said `NSPrefersDisplaySafeAreaCompatibilityMode = true` hands the fullscreen
# window the whole panel — and the dogfood Mac (dogfood-mac, 14" M4 Pro, panel at "More Space"
# 2048x1330) behaves the opposite way with the same key, which is also what Apple documents.
# One of those measurements is wrong, or the behavior depends on something we have not
# identified. Re-measure on the machine that disagrees rather than arguing from the table.
#
# Each arm gets its own bundle identifier: macOS keeps notch policy per app (that is the Finder
# "Scale to fit below built-in camera" checkbox) and LaunchServices caches a registration, so
# reusing one identity across arms can leave the previous arm's policy in force.
#
# Usage:  ./run-remote.sh [user@host]        (default user@dogfood-mac)
#
# The probe takes over a Space for a few seconds per arm, so this only STAGES and BUILDS —
# running is left to whoever is sitting at that Mac.
set -euo pipefail
cd "$(dirname "$0")"

TARGET="${1:-user@dogfood-mac}"
DIR=/tmp/notch-probe

ssh -o BatchMode=yes "$TARGET" "rm -rf $DIR && mkdir -p $DIR"
scp -q -o BatchMode=yes probe.swift build.sh "$TARGET:$DIR/"

# absent / false / true — the three states the key can be in.
ssh -o BatchMode=yes "$TARGET" "cd $DIR \
  && APP_NAME=NotchAbsent BUNDLE_ID=eti.noronha.limina.notchabsent bash build.sh \
  && SAFE_AREA_COMPAT=false APP_NAME=NotchFalse BUNDLE_ID=eti.noronha.limina.notchfalse bash build.sh \
  && SAFE_AREA_COMPAT=true  APP_NAME=NotchTrue  BUNDLE_ID=eti.noronha.limina.notchtrue  bash build.sh"

cat <<EOF

Built on $TARGET. Run each arm there (lid open — the probe picks the notched screen):

  $DIR/NotchAbsent.app/Contents/MacOS/NotchAbsent
  $DIR/NotchFalse.app/Contents/MacOS/NotchFalse
  $DIR/NotchTrue.app/Contents/MacOS/NotchTrue

Each goes fullscreen for a few seconds and prints screen.frame, the fullscreen contentView
frame, safeAreaInsets.top and auxiliaryTopLeftArea. The contentView height is the answer:
equal to screen.frame's height means that arm gets the whole panel.
EOF
