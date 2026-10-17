"""Compares lm_check.py's GPU and CPU logits, and prints the GPU's generation.

    python3 lm_compare.py <gpu-prefix> <cpu-prefix>

Exits nonzero unless, in float32, the next-token choice agrees at every prompt position and the
largest difference is within 1e-4 of the logits' scale, and the GPU's second generation, whose
weights another stream computes, is the first one token for token.
"""

import json
import sys

import mlx.core as mx

g_prefix, c_prefix = sys.argv[1], sys.argv[2]
with mx.stream(mx.cpu):
    g = mx.load(f"{g_prefix}-logits.npy")
    c = mx.load(f"{c_prefix}-logits.npy")
    diff = mx.abs(g - c).max().item()
    scale = mx.abs(c).max().item()
    agree = (mx.argmax(g, -1) == mx.argmax(c, -1)).sum().item()
positions = g.shape[1]
gr = json.load(open(f"{g_prefix}.json"))

print(f"prompt logits in float32: {positions} positions x {g.shape[2]}, "
      f"max|gpu-cpu| {diff:.3g} of {scale:.3g}, next-token choice agrees at {agree}/{positions}")
print(f"GPU, {gr['dtype']}: {len(gr['tokens'])} tokens at {gr['generation_tps']:.1f} tok/s, "
      f"prompt {gr['prompt_tps']:.1f} tok/s, peak {gr['peak_memory_gb']:.2f} GB")
print("GPU text:", repr(gr["text"]))
same = gr["again"] == gr["tokens"]
print("the generation after a lazy cast back:", "the same tokens" if same else "DIFFERENT tokens")
ok = agree == positions and diff <= 1e-4 * scale and same
print("ok" if ok else "FAILED")
sys.exit(0 if ok else 1)
