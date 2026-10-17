"""Drives an OpenAI-compatible server (omlx) over HTTP and checks what comes back.

    python3 serve_check.py <base-url> <model>

Three runs, each greedy: one 256-token completion; the same request streamed, counting the
chunks and the time to the first; and four different requests at once, which omlx batches.
Prints each run's speed from the server's own usage block and the wall clock, and exits nonzero
if a response is not 200, is empty, or a stream does not end with [DONE].
"""

import json
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

base, model = sys.argv[1].rstrip("/"), sys.argv[2]
ok = True


def post(body, stream=False):
    req = urllib.request.Request(
        f"{base}/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    return urllib.request.urlopen(req, timeout=900)


def ask(prompt, max_tokens=256):
    return {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
    }


def complete(prompt, max_tokens=256):
    start = time.perf_counter()
    with post(ask(prompt, max_tokens)) as r:
        status, body = r.status, json.load(r)
    return status, body, time.perf_counter() - start


def check(name, cond):
    global ok
    if not cond:
        print(f"FAILED: {name}")
        ok = False


status, body, wall = complete("Write a short story about a lighthouse keeper, in about 200 words.")
text = body["choices"][0]["message"]["content"]
u = body["usage"]
check("one completion", status == 200 and text.strip())
print(f"one request: {u['completion_tokens']} tokens, {u['generation_tokens_per_second']:.1f} tok/s "
      f"generating, {u['prompt_tokens_per_second']:.1f} tok/s prompt, first token "
      f"{u['time_to_first_token']:.2f} s, {wall:.2f} s wall")
print("  text:", repr(text[:160]) + "...")

body = ask("List five uses of a paperclip, one per line.", 128)
body["stream"] = True
start, first, chunks, done, streamed = time.perf_counter(), None, 0, False, ""
with post(body) as r:
    check("stream status", r.status == 200)
    for line in r:
        line = line.decode().strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            done = True
            break
        delta = json.loads(data)["choices"][0].get("delta", {}).get("content")
        if delta:
            first = first or time.perf_counter() - start
            chunks += 1
            streamed += delta
wall = time.perf_counter() - start
check("stream ends with [DONE]", done)
check("stream has content", chunks > 0)
print(f"streamed: {chunks} chunks, first after {first or 0:.2f} s, {wall:.2f} s wall")
print("  text:", repr(streamed[:160]) + "...")

prompts = [
    "Explain how a bicycle stays upright, in about 150 words.",
    "Describe the water cycle, in about 150 words.",
    "Summarise the plot of Hamlet, in about 150 words.",
    "Explain what a hash table is, in about 150 words.",
]
start = time.perf_counter()
with ThreadPoolExecutor(len(prompts)) as pool:
    results = list(pool.map(complete, prompts))
wall = time.perf_counter() - start
tokens = sum(b["usage"]["completion_tokens"] for _, b, _ in results)
for (s, b, _), p in zip(results, prompts):
    check(f"concurrent: {p[:30]}", s == 200 and b["choices"][0]["message"]["content"].strip())
per = ", ".join(f"{b['usage']['generation_tokens_per_second']:.1f}" for _, b, _ in results)
print(f"{len(prompts)} at once: {tokens} tokens in {wall:.2f} s wall, {tokens / wall:.1f} tok/s "
      f"together; each request's own rate {per} tok/s")
print("ok" if ok else "FAILED")
sys.exit(0 if ok else 1)
