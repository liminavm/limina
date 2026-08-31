#!/usr/bin/env python3
"""
Rewrite one short_term_ref_pic_set flag inside an HEVC SPS, keeping every bit length the
same, so the only difference is the CONTENT of a reference picture set.

This is the change the backend design needs VideoToolbox to tolerate on a live session:
refining a set it has already been told about. A count change or a level change are both
already known to be refused, and neither answers this question.
"""
import sys

def unescape(b):
    out, z = bytearray(), 0
    for x in b:
        if z == 2 and x == 3: z = 0; continue
        out.append(x); z = z + 1 if x == 0 else 0
    return bytes(out)

def escape(b):
    out, z = bytearray(), 0
    for x in b:
        if z == 2 and x <= 3: out.append(3); z = 0
        out.append(x); z = z + 1 if x == 0 else 0
    return bytes(out)

class BR:
    def __init__(s, b): s.b, s.p = b, 0
    def u(s, n):
        v = 0
        for _ in range(n):
            v = (v << 1) | ((s.b[s.p >> 3] >> (7 - (s.p & 7))) & 1); s.p += 1
        return v
    def ue(s):
        z = 0
        while s.u(1) == 0: z += 1
        return (1 << z) - 1 + (s.u(z) if z else 0)
    def se(s):
        k = s.ue(); return (k + 1) // 2 if k % 2 else -(k // 2)

def find_first_used_flag(rbsp):
    """Return the bit position of used_by_curr_pic_s0_flag[0] in st_ref_pic_set(0)."""
    r = BR(rbsp); r.p = 16                       # skip the 2-byte NAL header
    r.u(4); msl = r.u(3); r.u(1)                 # vps_id, max_sub_layers_minus1, nesting
    r.u(2); r.u(1); r.u(5); r.u(32); r.u(4); r.u(44); r.u(8)   # profile_tier_level
    if msl: raise SystemExit("sub-layer PTL not handled; clip has max_sub_layers_minus1=%d" % msl)
    r.ue()                                       # sps_seq_parameter_set_id
    if r.ue() == 3: r.u(1)                       # chroma_format_idc
    r.ue(); r.ue()                               # pic_width / pic_height_in_luma_samples
    if r.u(1): [r.ue() for _ in range(4)]        # conformance window
    r.ue(); r.ue()                               # bit depths
    r.ue()                                       # log2_max_pic_order_cnt_lsb_minus4
    sub = r.u(1)
    for _ in range(0 if sub else msl, msl + 1): r.ue(); r.ue(); r.ue()
    for _ in range(6): r.ue()                    # CTB / transform sizes and depths
    if r.u(1):                                   # scaling_list_enabled_flag
        if r.u(1): raise SystemExit("scaling list data present; not handled")
    r.u(1); r.u(1)                               # amp, sao
    if r.u(1):                                   # pcm_enabled_flag
        r.u(4); r.u(4); r.ue(); r.ue(); r.u(1)
    n = r.ue()                                   # num_short_term_ref_pic_sets
    if n == 0: raise SystemExit("this SPS declares 0 short term ref pic sets")
    flags, prev_delta = [], 0
    for idx in range(n):
        if idx != 0 and r.u(1):                  # inter_ref_pic_set_prediction_flag
            r.u(1); r.ue()                       # delta_rps_sign, abs_delta_rps_minus1
            for _ in range(prev_delta + 1):
                if not r.u(1):                   # used_by_curr_pic_flag
                    r.u(1)                       # use_delta_flag
            continue                             # count unchanged for our purposes
        neg, pos = r.ue(), r.ue()
        for _ in range(neg):
            r.ue(); flags.append(r.p); r.u(1)    # delta_poc_s0_minus1, used_by_curr_pic_s0_flag
        for _ in range(pos):
            r.ue(); flags.append(r.p); r.u(1)
        prev_delta = neg + pos
    return n, flags

if __name__ == '__main__':
    src, dst = sys.argv[1], sys.argv[2]
    data = open(src, 'rb').read()
    i, sps = 0, None
    while True:
        j = data.find(b'\x00\x00\x01', i)
        if j < 0: break
        k = data.find(b'\x00\x00\x01', j + 3); k = len(data) if k < 0 else k
        nal = data[j + 3:k]
        if nal and ((nal[0] >> 1) & 0x3f) == 33:
            sps = nal[:-1] if nal.endswith(b'\x00') else nal
            break
        i = j + 3
    if sps is None: raise SystemExit("no SPS")

    rbsp = bytearray(unescape(sps))
    n, flags = find_first_used_flag(rbsp)
    for bit in flags:
        rbsp[bit >> 3] ^= (1 << (7 - (bit & 7)))
    out = escape(bytes(rbsp))
    print(f"  {n} ref pic sets; flipped {len(flags)} used_by_curr_pic flags")
    print(f"  SPS {len(sps)} -> {len(out)} bytes (length preserved: {len(sps) == len(out)})")
    open(dst, 'wb').write(b'\x00\x00\x00\x01' + out)
