#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Point git at the in-repo hooks (one-time, per clone). Hooks live in .githooks/ so they
# are version-controlled; core.hooksPath is local config, hence this setup step.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# Relative on purpose: the setting is shared by every worktree, and git resolves a relative
# hooksPath per worktree — an absolute one would make every worktree run THIS checkout's hooks.
git config core.hooksPath .githooks
echo "hooks enabled: core.hooksPath = .githooks (pre-commit runs cargo fmt + clippy;"
echo "               pre-push refuses commits whose dependency pins no remote carries)"
echo "               drop an executable at $(git rev-parse --git-common-dir)/local-pre-commit"
echo "               to add per-clone checks git will never publish"
