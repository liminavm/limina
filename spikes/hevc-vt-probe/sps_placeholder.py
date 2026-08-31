#!/usr/bin/env python3
"""
Rewrite an HEVC SPS the way the backend's serializer will have to: keep
num_short_term_ref_pic_sets, but replace every set with an EMPTY placeholder, and
over-declare the level.

The set contents are not on the VA-API wire, so the backend cannot reproduce them. This is
only sound because slices that inline their own reference picture set never read them --
which is what the probe measured, and what the backend refuses to proceed without.

  st_ref_pic_set(0)   = ue(0) ue(0)                       -- 2 bits
  st_ref_pic_set(i>0) = 0, ue(0) ue(0)                    -- 3 bits, the leading 0 being
                        inter_ref_pic_set_prediction_flag, which exists for every i != 0
"""
import sys
from rps_edit import unescape, escape, BR      # reuse the readers

class BW:
    def __init__(s): s.bits = []
    def u(s, v, n): s.bits += [(v >> (n - 1 - i)) & 1 for i in range(n)]
    def ue(s, v):
        v += 1; n = v.bit_length()
        s.bits += [0] * (n - 1) + [(v >> (n - 1 - i)) & 1 for i in range(n)]
    def copy(s, r, a, b):
        for p in range(a, b): s.bits.append((r.b[p >> 3] >> (7 - (p & 7))) & 1)
    def bytes(s):
        s.bits.append(1)                                   # rbsp_stop_one_bit
        while len(s.bits) % 8: s.bits.append(0)
        return bytes(int(''.join(map(str, s.bits[i:i+8])), 2) for i in range(0, len(s.bits), 8))

def locate(rbsp):
    """Bit range of the short_term_ref_pic_set list, plus the set count."""
    r = BR(rbsp); r.p = 16
    r.u(4); msl = r.u(3); r.u(1)
    r.u(2); r.u(1); r.u(5); r.u(32); r.u(4); r.u(44); lvl_at = r.p; r.u(8)
    if msl: raise SystemExit("sub-layer PTL not handled")
    r.ue()
    if r.ue() == 3: r.u(1)
    r.ue(); r.ue()
    if r.u(1): [r.ue() for _ in range(4)]
    r.ue(); r.ue(); r.ue()
    sub = r.u(1)
    for _ in range(0 if sub else msl, msl + 1): r.ue(); r.ue(); r.ue()
    for _ in range(6): r.ue()
    if r.u(1):
        if r.u(1): raise SystemExit("scaling list data present; not handled")
    r.u(1); r.u(1)
    if r.u(1): r.u(4); r.u(4); r.ue(); r.ue(); r.u(1)
    start = r.p
    n = r.ue()
    prev = 0
    for idx in range(n):
        if idx != 0 and r.u(1):
            r.u(1); r.ue()
            for _ in range(prev + 1):
                if not r.u(1): r.u(1)
            continue
        neg, pos = r.ue(), r.ue()
        for _ in range(neg + pos): r.ue(); r.u(1)
        prev = neg + pos
    return n, start, r.p, lvl_at

data = open(sys.argv[1], 'rb').read()
i, sps = 0, None
while True:
    j = data.find(b'\x00\x00\x01', i)
    if j < 0: break
    k = data.find(b'\x00\x00\x01', j + 3); k = len(data) if k < 0 else k
    nal = data[j + 3:k]
    if nal and ((nal[0] >> 1) & 0x3f) == 33:
        sps = nal[:-1] if nal.endswith(b'\x00') else nal; break
    i = j + 3
rbsp = bytearray(unescape(sps))
n, a, b, lvl_at = locate(rbsp)
rbsp[lvl_at >> 3] = 153                                   # over-declare the level
r = BR(bytes(rbsp))
stop = max(p for p in range(len(rbsp) * 8) if (rbsp[p >> 3] >> (7 - (p & 7))) & 1)

w = BW()
w.copy(r, 0, a)
w.ue(n)
for idx in range(n):
    if idx: w.u(0, 1)                                     # inter_ref_pic_set_prediction_flag
    w.ue(0); w.ue(0)                                      # num_negative_pics, num_positive_pics
w.copy(r, b, stop)
out = escape(w.bytes())
print(f"  {n} sets, originally {b-a} bits -> {2 + sum(3 if i else 2 for i in range(n)) - 1} bits of placeholder")
print(f"  general_level_idc over-declared to 153")
print(f"  SPS {len(sps)} -> {len(out)} bytes")
open(sys.argv[2], 'wb').write(b'\x00\x00\x00\x01' + out)
