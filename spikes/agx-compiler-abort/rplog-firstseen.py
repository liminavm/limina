#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Find the render-pass configuration that triggered the AGX `bitcode_url` abort (task #29).
#
# Thread correlation does NOT work here, and that is worth knowing before trying it: the abort
# is reported on a Metal-internal compiler-connection thread that never appears in the
# LIMINA_KK_RPLOG output, because Metal issues MTLBuildOpaqueRequest off the thread that created
# the encoder. So instead we use the one property the compile has: Metal builds a background
# object LAZILY, the first time it sees a given render-pass configuration. The trigger is
# therefore a configuration whose FIRST occurrence is at the very end of the log -- everything
# seen earlier already compiled fine.
#
# Usage: rplog-firstseen.py <worker.log> [tail_blocks]
#   Prints the configurations first seen in the last `tail_blocks` render passes (default 200),
#   which for a run that ends in the abort is a very short list.

import re
import sys
from collections import OrderedDict

HDR = re.compile(r"\[LIMINA-KK-RP\] tid=(0x[0-9a-f]+) (.*)")
ATT = re.compile(r"\[LIMINA-KK-RP\]   (\w+)\[(\d+)\] tex=0x[0-9a-f]+ (.*)")


def blocks(path):
    """Yield (index, tid, signature) per render pass. The signature deliberately drops the
    thread id and the texture pointer -- both vary run to run and per surface, while the
    configuration Metal compiles for does not."""
    cur = None
    idx = 0
    with open(path, errors="replace") as f:
        for line in f:
            m = HDR.match(line)
            if m:
                if cur is not None:
                    yield idx, cur[0], "\n".join(cur[1])
                    idx += 1
                cur = (m.group(1), [m.group(2)])
                continue
            if cur is None:
                continue
            m = ATT.match(line)
            if m:
                cur[1].append(f"{m.group(1)}[{m.group(2)}] {m.group(3)}")
            elif line.startswith("[LIMINA-KK-RP]"):
                pass
    if cur is not None:
        yield idx, cur[0], "\n".join(cur[1])


def main():
    path = sys.argv[1]
    tail = int(sys.argv[2]) if len(sys.argv) > 2 else 200

    first_seen = OrderedDict()
    total = 0
    for idx, tid, sig in blocks(path):
        total += 1
        if sig not in first_seen:
            first_seen[sig] = (idx, tid)

    if total == 0:
        print("no [LIMINA-KK-RP] blocks -- was LIMINA_KK_RPLOG=1 set?", file=sys.stderr)
        return 1

    cutoff = total - tail
    late = [(idx, tid, sig) for sig, (idx, tid) in first_seen.items() if idx >= cutoff]

    print(f"{total} render passes, {len(first_seen)} distinct configurations")
    print(f"configurations first seen in the last {tail} passes: {len(late)}\n")
    for idx, tid, sig in late:
        print(f"--- first seen at pass {idx}/{total} (tid={tid}) ---")
        print(sig)
        print()
    if not late:
        print("None. The trigger is not a newly-seen configuration; fall back to the last few")
        print("passes overall, printed here:\n")
        allb = list(blocks(path))
        for idx, tid, sig in allb[-5:]:
            print(f"--- pass {idx}/{total} (tid={tid}) ---")
            print(sig)
            print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
