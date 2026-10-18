#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# llama.cpp at LLAMA_CPP_REV, as $PERF_HOME/llama.cpp-src.tar (prefix src/) for the guest build,
# from a shallow clone kept in $PERF_HOME/llama.cpp-git.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/env.sh"
G=$PERF_HOME/llama.cpp-git
if [ ! -d "$G/.git" ]; then
  git init -q "$G"
  git -C "$G" remote add origin https://github.com/ggml-org/llama.cpp.git
fi
if [ "$(git -C "$G" rev-parse -q --verify HEAD 2>/dev/null)" != "$LLAMA_CPP_REV" ]; then
  git -C "$G" fetch -q --depth 1 origin "$LLAMA_CPP_REV"
  git -C "$G" checkout -q --detach FETCH_HEAD
fi
git -C "$G" archive --format=tar --prefix=src/ -o "$PERF_HOME/llama.cpp-src.tar" HEAD
