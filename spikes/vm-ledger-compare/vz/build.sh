#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
set -e
cd "$(dirname "$0")"
swiftc -O -o vz-probe probe.swift -framework Virtualization
codesign --entitlements vz.entitlements -s - --force vz-probe
echo "==> vz-probe built + signed"
