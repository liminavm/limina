# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# MLX on the GPU, the same script in a guest (over limina-metal) and on macOS (the native control):
#   matmul   2048^3 float32 and 4096^3 float16, ms, median over 50 after a warm-up
#   lm       greedy generation with mlx-lm from a fixed ~500-token prompt: prompt and generation
#            tokens per second, median over 5 runs after a warm-up (128 tokens each: the prompt asks
#            for far more, so a model stopping early fails the run), and a hash of the text, which
#            must be the same in every run (and is expected to match the other side)
# Each prints `ROW <name> <metric> <value>`; everything else is commentary.
# Usage: python3 mlx_bench.py matmul | lm <model-dir> <tag>
import hashlib
import statistics
import sys
import time

import mlx.core as mx


def guard():
    if not mx.metal.is_available() or mx.default_device() != mx.gpu:
        print("FAIL: MLX is not on the GPU")
        sys.exit(1)
    info = mx.device_info() if hasattr(mx, "device_info") else mx.metal.device_info()
    print("DEVICE", info.get("device_name", "?"), info.get("architecture", "?"))


def matmul():
    for n, dt, name in ((2048, mx.float32, "f32-2048"), (4096, mx.float16, "f16-4096")):
        a = mx.random.normal((n, n)).astype(dt)
        b = mx.random.normal((n, n)).astype(dt)
        mx.eval(a, b)
        for _ in range(5):
            mx.eval(a @ b)
        times = []
        for _ in range(50):
            t = time.perf_counter()
            mx.eval(a @ b)
            times.append((time.perf_counter() - t) * 1e3)
        ms = statistics.median(times)
        print(f"ROW mlx-matmul-{name} ms {ms:.3f}")
        print(f"ROW mlx-matmul-{name} tflops {2 * n**3 / ms / 1e9:.2f}")


PROMPT = (
    "Below are notes from a long walk through an old city. Retell them as a long story of at "
    "least 600 words, one paragraph per stop.\n\n"
    + " ".join(
        f"Stop {i}: the street turned past a bakery, a closed bookshop and a square with a fountain, "
        f"and the walk went on uphill towards the old walls."
        for i in range(1, 21)
    )
)


def lm(path, tag):
    from mlx_lm import load, stream_generate

    model, tok = load(path)
    prompt = tok.apply_chat_template([{"role": "user", "content": PROMPT}], add_generation_prompt=True)
    print("prompt tokens", len(prompt))
    pp, tg, hashes = [], [], set()
    for run in range(6):
        # Back to back, every run after the first loses about a third of its speed on an M1 Max
        # (7B: prompt 485 -> 241 tok/s); two seconds between runs gives it back, clearing MLX's
        # cache does not. Both sides pause the same.
        time.sleep(2)
        text, last = "", None
        for r in stream_generate(model, tok, prompt, max_tokens=128):
            text += r.text
            last = r
        if last.generation_tokens != 128:
            print(f"FAIL: generated {last.generation_tokens} tokens, not 128")
            sys.exit(1)
        hashes.add(hashlib.sha256(text.encode()).hexdigest()[:16])
        if run > 0:
            pp.append(last.prompt_tps)
            tg.append(last.generation_tps)
        print(f"run {run}: prompt {last.prompt_tps:.0f} tok/s, generation {last.generation_tps:.1f} tok/s, peak {last.peak_memory:.2f} GB")
    if len(hashes) != 1:
        print("FAIL: runs differ:", sorted(hashes))
        sys.exit(1)
    print("TEXT", hashes.pop(), repr(text[:80]))
    print(f"ROW mlx-lm-{tag} prompt_tok_s {statistics.median(pp):.1f}")
    print(f"ROW mlx-lm-{tag} gen_tok_s {statistics.median(tg):.1f}")


if __name__ == "__main__":
    guard()
    if sys.argv[1] == "matmul":
        matmul()
    elif sys.argv[1] == "lm":
        lm(sys.argv[2], sys.argv[3])
    else:
        sys.exit(f"unknown mode {sys.argv[1]}")
