#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Pair the guest's virtio-gpu queue and response tracepoints and report command latency.

    parse.py <trace.txt> [label]

The guest kernel fires `virtio_gpu_cmd_queue` when it puts a command on a virtqueue and
`virtio_gpu_cmd_response` when it takes the host's answer, both carrying the command's `seqno`.
The gap is the command's whole trip through the host: time queued behind earlier commands on the
control queue, plus its own processing. For RESOURCE_FLUSH that is the part of flush-to-present a
host-side stall can stretch; the present after the answer is the supervisor's and unaffected.

The response event's `type` is the reply's, so a command's type is taken from its queue event.
"""

import re
import sys

EVENT = re.compile(
    r'\s(\d+\.\d+): virtio_gpu_cmd_(queue|response): vdev=\d+ vq=\d+ name=(\S+) type=0x([0-9a-f]+)'
    r'.* seqno=(\d+)'
)
NAMES = {0x0104: 'RESOURCE_FLUSH', 0x0207: 'SUBMIT_3D', 0x0103: 'SET_SCANOUT',
         0x0105: 'TRANSFER_TO_HOST_2D', 0x0205: 'TRANSFER_TO_HOST_3D'}


def pct(values, p):
    s = sorted(values)
    return s[min(len(s) - 1, int(round(p / 100 * (len(s) - 1))))]


def main():
    path = sys.argv[1]
    label = sys.argv[2] if len(sys.argv) > 2 else path
    queued = {}
    lat = {}
    for line in open(path, errors='replace'):
        m = EVENT.search(line)
        if not m:
            continue
        ts, kind, vq, typ, seq = float(m[1]), m[2], m[3], int(m[4], 16), int(m[5])
        if not vq.startswith('control'):
            continue
        if kind == 'queue':
            queued[seq] = (ts, typ)
        elif seq in queued:
            t0, ctyp = queued.pop(seq)
            lat.setdefault(ctyp, []).append((ts - t0) * 1e3)
    every = [v for vs in lat.values() for v in vs]
    rows = [('all control', every)] + [
        (NAMES.get(t, hex(t)), vs) for t, vs in sorted(lat.items(), key=lambda kv: -len(kv[1]))
        if t in NAMES
    ]
    print(f'{label}')
    print(f'  {"command":22} {"n":>6} {"p50 ms":>8} {"p95 ms":>8} {"p99 ms":>8} {"max ms":>8}'
          f' {">16ms":>6}')
    for name, vs in rows:
        if not vs:
            continue
        over = sum(v > 16.0 for v in vs)
        print(f'  {name:22} {len(vs):6d} {pct(vs, 50):8.2f} {pct(vs, 95):8.2f} {pct(vs, 99):8.2f}'
              f' {max(vs):8.2f} {over:6d}')


if __name__ == '__main__':
    main()
