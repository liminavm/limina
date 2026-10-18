#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# The compute battery's models, into $PERF_HOME/models: Qwen2.5-Instruct at 0.5B and 7B, once as
# MLX 4-bit and once as GGUF Q4_0, so MLX and llama.cpp run the same model. Fetched once; the
# tools disk and the native control both read them from here.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/env.sh"
V=$PERF_HOME/venv-native
[ -x "$V/bin/hf" ] || { python3 -m venv "$V"; "$V/bin/pip" install -q huggingface_hub; }
M=$PERF_HOME/models
"$V/bin/hf" download mlx-community/Qwen2.5-0.5B-Instruct-4bit --local-dir "$M/mlx-qwen2.5-0.5b-4bit"
"$V/bin/hf" download mlx-community/Qwen2.5-7B-Instruct-4bit --local-dir "$M/mlx-qwen2.5-7b-4bit"
"$V/bin/hf" download bartowski/Qwen2.5-0.5B-Instruct-GGUF Qwen2.5-0.5B-Instruct-Q4_0.gguf --local-dir "$M/gguf"
"$V/bin/hf" download bartowski/Qwen2.5-7B-Instruct-GGUF Qwen2.5-7B-Instruct-Q4_0.gguf --local-dir "$M/gguf"
# The download cache hf keeps beside each model is not the model.
find "$M" -name .cache -type d -prune -exec rm -rf {} +
