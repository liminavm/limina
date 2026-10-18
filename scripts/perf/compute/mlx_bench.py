# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# MLX on the GPU, the same script in a guest (over limina-metal) and on macOS (the native control):
#   matmul   2048^3 float32 and 4096^3 float16, ms, median over 100 after a second of warm-up
#   lm       greedy generation with mlx-lm from a fixed ~700-token prompt: prompt and generation
#            tokens per second, median of 5 back-to-back runs after 2 untimed ones, 128 tokens each
#            (the prompt asks for far more, so a model stopping early fails the run), and a hash
#            of the text, which must be the same in every run and is expected to match the other
#            side
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
        # A second of the same work first: the GPU ramps its clock up under load, and the timed
        # part is short enough to land inside the ramp otherwise.
        t = time.perf_counter()
        while time.perf_counter() - t < 1.0:
            mx.eval(a @ b)
        times = []
        for _ in range(100):
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
    # Sustained speed, not burst: the runs go back to back after an untimed one. On an M1 Max the
    # GPU clocks up for a short burst and settles lower under sustained load (7B generation: ~70
    # tok/s for the first couple of seconds, ~45-50 after), and a pause between runs makes the
    # number depend on how long each run lasts against that ramp. Sustained is what a long
    # generation gets, and it puts both sides in the same regime.
    for run in range(7):
        text, last = "", None
        for r in stream_generate(model, tok, prompt, max_tokens=128):
            text += r.text
            last = r
        if last.generation_tokens != 128:
            print(f"FAIL: generated {last.generation_tokens} tokens, not 128")
            sys.exit(1)
        hashes.add(hashlib.sha256(text.encode()).hexdigest()[:16])
        if run > 1:
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
