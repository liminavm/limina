#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Point git at the in-repo hooks (one-time, per clone). Hooks live in .githooks/ so they
# are version-controlled; core.hooksPath is local config, hence this setup step.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
git config core.hooksPath .githooks
echo "hooks enabled: core.hooksPath = .githooks (pre-commit runs cargo fmt + clippy)"
