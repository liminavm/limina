#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""SUBMIT_3D latency split by fence flag and context: which contexts' fences wait, and on whom.

    ctx.py <trace.txt> [label]

A fenced command is answered only when its fence retires, so its latency includes every wait the
fence makes; an unfenced one is answered once the host has processed it. Context names are in the
worker log's `CTX_CREATE ctx=N ... name=` lines.
"""
import re
import sys

E = re.compile(r'\s(\d+\.\d+): virtio_gpu_cmd_(queue|response): vdev=\d+ vq=\d+ name=(\S+) '
               r'type=0x([0-9a-f]+) flags=0x([0-9a-f]+) fence_id=\d+ ctx_id=(\d+).* seqno=(\d+)')


def pct(v, p):
    s = sorted(v)
    return s[min(len(s) - 1, int(round(p / 100 * (len(s) - 1))))]


queued, lat = {}, {}
for line in open(sys.argv[1], errors='replace'):
    m = E.search(line)
    if not m or not m[3].startswith('control'):
        continue
    ts, kind, typ, flags, ctx, seq = float(m[1]), m[2], int(m[4], 16), int(m[5], 16), int(m[6]), int(m[7])
    if kind == 'queue':
        queued[seq] = (ts, typ, flags & 1, ctx)
    elif seq in queued:
        t0, typ, fenced, ctx = queued.pop(seq)
        if typ == 0x0207:
            lat.setdefault((ctx, fenced), []).append((ts - t0) * 1e3)
print(sys.argv[2] if len(sys.argv) > 2 else sys.argv[1])
for (ctx, fenced), v in sorted(lat.items()):
    if len(v) >= 50:
        print(f'  ctx {ctx:3d} {"fenced  " if fenced else "unfenced"} n={len(v):5d}'
              f'  p50 {pct(v, 50):6.2f}  p95 {pct(v, 95):6.2f}  max {max(v):6.2f}')
