# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# llama-bench's jsonl on stdin to `ROW llama-bench-<tag> <test>_tok_s|_stddev <value>` lines;
# anything that is not a result passes through as commentary.
# Usage: llama-bench ... -o jsonl 2>&1 | python3 -I llama_rows.py <tag>
import json
import sys

tag = sys.argv[1]
for line in sys.stdin:
    if not line.startswith("{"):
        print(line, end="")
        continue
    j = json.loads(line)
    test = f"pp{j['n_prompt']}" if j["n_prompt"] else f"tg{j['n_gen']}"
    print(f"ROW llama-bench-{tag} {test}_tok_s {j['avg_ts']:.2f}")
    print(f"ROW llama-bench-{tag} {test}_stddev {j['stddev_ts']:.2f}")
