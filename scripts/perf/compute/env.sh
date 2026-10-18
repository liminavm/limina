# shellcheck shell=bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# What the compute battery is pinned to, sourced by its scripts. Moving MLX_TREE's commit,
# LLAMA_CPP_REV or MLX_LM means rebuilding the tools disk (tools-disk.sh) and the native control
# (native.sh); the rows name all three.
_main=$(cd "$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
# The disk, the models, the native builds: outside any worktree, shared by all of them.
PERF_HOME=${PERF_HOME:-$_main/target/perf-compute}
# MLX's fork at the manifest pin, as spikes/mlx-linux/host.sh builds it.
MLX_TREE=${MLX_TREE:-$_main/third_party/mlx}
# The rev spikes/kk-iq-large-constants/backend-ops.sh measured KosmicKrisp's ggml kernels at.
LLAMA_CPP_REV=${LLAMA_CPP_REV:-b42b7e6d3059503dd4bed6ca5dcf342bb6379f67}
MLX_LM=${MLX_LM:-0.32.2}
GUEST_USER=${GUEST_USER:-claude}
GUEST_PASS=${GUEST_PASS:-claudiusrobotus}
unset _main
