"""One device's half of the mlx-lm check.

    python3 lm_check.py <model> <gpu|cpu> <out-prefix>

writes <out-prefix>-logits.npy: the prompt's logits with the model in float32, where GPU and CPU
must agree to rounding (in float16 or bfloat16 near ties flip, so those cannot be compared).
On the GPU it first generates greedily in the model's own dtype, and once more after the float32
pass with the weights cast back lazily, and writes <out-prefix>.json with both generations and the
speed. MLX's CPU backend generates at about 0.1 tok/s, too slow to compare. mlx-lm
makes its generation stream on the default device when it is imported, so the device is set
first, and each device runs in a process of its own; lm_compare.py compares them.
"""

import json
import sys

import mlx.core as mx

model_path, device, out = sys.argv[1], sys.argv[2], sys.argv[3]
mx.set_default_device(mx.gpu if device == "gpu" else mx.cpu)

from mlx_lm import load, stream_generate  # noqa: E402
from mlx_lm.sample_utils import make_sampler  # noqa: E402

model, tokenizer = load(model_path)
messages = [{"role": "user", "content": "Explain in three sentences why the sky is blue."}]
prompt = tokenizer.apply_chat_template(messages, add_generation_prompt=True)

native = model.model.embed_tokens.scales.dtype


def generate():
    tokens, text = [], ""
    for r in stream_generate(
        model, tokenizer, prompt, max_tokens=96, sampler=make_sampler(temp=0.0)
    ):
        tokens.append(r.token)
        text += r.text
        last = r
    return tokens, text, last


if device == "gpu":
    tokens, text, last = generate()

model.set_dtype(mx.float32)
logits = model(mx.array(prompt)[None]).astype(mx.float32)
mx.eval(logits)
mx.save(f"{out}-logits.npy", logits)

if device == "gpu":
    # Cast back, lazily: the weights are computed on the default stream, and the generation that
    # uses them runs on mlx-lm's own, so this one waits on the other's events.
    model.set_dtype(native)
    again, _, _ = generate()
    json.dump(
        {
            "dtype": str(native),
            "tokens": tokens,
            "text": text,
            "again": again,
            "prompt_tps": last.prompt_tps,
            "generation_tps": last.generation_tps,
            "peak_memory_gb": last.peak_memory,
        },
        open(f"{out}.json", "w"),
    )
