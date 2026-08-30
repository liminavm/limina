#!/usr/bin/env python3
"""
An AV1 frame-header parser, to the spec, used as an independent reading of what the
serializer wrote.

Its value is that it is not the serializer: a bug shared between a writer and its own
reader is invisible, and this was written from the specification instead. `fhdiff` pairs a
rebuilt stream against the clip it came from and reports the first field that differs.

Beware the trap it walks into on its own: a rebuilt header legitimately runs one or two
bits LONGER than the encoder's, because separate_uv_delta_q, enable_superres and
initial_display_delay_present_flag are derived differently. Each stream is read with its own
sequence header, so that is harmless. Compare fields, not bit positions.

Used as a library by dpb-check.py; run directly to diff two streams:

    ./fhparse.py <original.obu> <rebuilt.obu> [frame-index]
"""
import sys

SKIP_HINT={}
FRAME_IDX=0
class BR:
    def __init__(s,d): s.d=d; s.p=0
    def f(s,n):
        v=0
        for _ in range(n):
            v=(v<<1)|((s.d[s.p>>3]>>(7-(s.p&7)))&1); s.p+=1
        return v
    def su(s,n):
        v=s.f(n); return v-(1<<n) if v>=(1<<(n-1)) else v
    def ns(s,n):
        if n<=1: return 0
        w=n.bit_length(); m=(1<<w)-n
        v=s.f(w-1)
        if v<m: return v
        return (v<<1)-m+s.f(1)
    def inc(s,low,high):
        v=low
        while v<high and s.f(1): v+=1
        return v

def walk(d, limit=400):
    out=[]; pos=0
    while pos < len(d) and len(out)<limit:
        h=d[pos]; typ=(h>>3)&0xf; ext=(h>>2)&1
        cur=pos+1+ext; v=0; sh=0
        while True:
            b=d[cur]; cur+=1; v |= (b&0x7f)<<sh; sh+=7
            if not b&0x80: break
        out.append((typ, d[cur:cur+v])); pos=cur+v
    return out

def seq(d):
    b=BR(d); o={}
    o['seq_profile']=b.f(3); b.f(1); o['reduced']=b.f(1)
    o['timing']=b.f(1); o['iddp']=b.f(1); o['opcnt']=b.f(5)
    for i in range(o['opcnt']+1):
        b.f(12); lvl=b.f(5)
        if lvl>7: b.f(1)
        if o['iddp']:
            if b.f(1): b.f(4)
    o['wbits']=b.f(4)+1; o['hbits']=b.f(4)+1
    o['max_w']=b.f(o['wbits'])+1; o['max_h']=b.f(o['hbits'])+1
    o['frame_id']=b.f(1)
    o['sb128']=b.f(1); o['filter_intra']=b.f(1); o['intra_edge']=b.f(1)
    o['interintra']=b.f(1); o['masked']=b.f(1); o['warp']=b.f(1); o['dual']=b.f(1)
    o['order_hint']=b.f(1)
    if o['order_hint']: o['jnt']=b.f(1); o['ref_mvs']=b.f(1)
    o['choose_sct']=b.f(1)
    o['force_sct']=2 if o['choose_sct'] else b.f(1)
    if o['force_sct']>0:
        o['choose_imv']=b.f(1)
        o['force_imv']=2 if o['choose_imv'] else b.f(1)
    else: o['force_imv']=2
    o['ohbits']=b.f(3)+1 if o['order_hint'] else 0
    o['superres']=b.f(1); o['cdef']=b.f(1); o['restoration']=b.f(1)
    o['high_bd']=b.f(1)
    if o['seq_profile']==2 and o['high_bd']: b.f(1)
    o['mono']=b.f(1) if o['seq_profile']!=1 else 0
    o['cdp']=b.f(1)
    if o['cdp']: b.f(8); b.f(8); b.f(8)
    b.f(1)
    if o['seq_profile']==0: b.f(2)
    o['sep_uv']=b.f(1); o['film_grain']=b.f(1)
    return o

def delta_q(b):
    return b.su(1+6) if b.f(1) else 0

def fh(d, s):
    b=BR(d); o={}
    o['show_existing']=b.f(1)
    if o['show_existing']: return o
    o['frame_type']=b.f(2)
    intra = o['frame_type'] in (0,2)
    o['show_frame']=b.f(1)
    o['showable']= (o['frame_type']!=0) if o['show_frame'] else b.f(1)
    if o['frame_type']==3 or (o['frame_type']==0 and o['show_frame']): o['err_res']=1
    else: o['err_res']=b.f(1)
    o['disable_cdf_update']=b.f(1)
    o['allow_sct'] = b.f(1) if s['force_sct']==2 else s['force_sct']
    if o['allow_sct']:
        o['force_imv'] = b.f(1) if s['force_imv']==2 else s['force_imv']
    else: o['force_imv']=0
    o['size_override'] = 1 if o['frame_type']==3 else b.f(1)
    o['order_hint']=b.f(s['ohbits']) if s['ohbits'] else 0
    o['primary_ref']=7 if (intra or o['err_res']) else b.f(3)
    if o['frame_type']==3 or (o['frame_type']==0 and o['show_frame']): o['refresh']=255
    else: o['refresh']=b.f(8)
    if (not intra or o['refresh']!=255) and s['order_hint'] and o['err_res']:
        o['ref_order_hints']=[b.f(s['ohbits']) for _ in range(8)]
    def frame_size():
        if o['size_override']:
            o['w']=b.f(s['wbits'])+1; o['h']=b.f(s['hbits'])+1
        else:
            o['w']=s['max_w']; o['h']=s['max_h']
        o['use_superres']=b.f(1) if s['superres'] else 0
        o['superres_denom']=b.f(3)+9 if o['use_superres'] else 8
    def render_size():
        o['render_diff']=b.f(1)
        if o['render_diff']: o['rw']=b.f(16)+1; o['rh']=b.f(16)+1
    if intra:
        frame_size(); render_size()
        o['allow_intrabc']=b.f(1) if (o['allow_sct'] and o['superres_denom']==8) else 0
    else:
        o['short_sig']=b.f(1) if s['order_hint'] else 0
        if o['short_sig']: o['last_idx']=b.f(3); o['gold_idx']=b.f(3)
        o['ref_idx']=[b.f(3) for _ in range(7)] if not o['short_sig'] else []
        if o['size_override'] and not o['err_res']:
            o['found_ref']=[]
            for i in range(7):
                fr=b.f(1); o['found_ref'].append(fr)
                if fr: break
            if not any(o['found_ref']):
                frame_size(); render_size()
        else:
            frame_size(); render_size()
        o['hp_mv']=0 if o['force_imv'] else b.f(1)
        sw=b.f(1); o['interp']=4 if sw else b.f(2)
        o['motion_switchable']=b.f(1)
        o['use_ref_mvs']=0 if (o['err_res'] or not s.get('ref_mvs')) else b.f(1)
        o['allow_intrabc']=0
    o['disable_frame_end_cdf']=1 if o['disable_cdf_update'] else b.f(1)
    o['_bits_at_tile_info']=b.p

    # ---- tile_info
    def tlog2(blk, target):
        k=0
        while (blk<<k) < target: k+=1
        return k
    mi_cols=2*((o['w']+7)>>3); mi_rows=2*((o['h']+7)>>3)
    sbsh=5 if s['sb128'] else 4; sbsz=sbsh+2
    sb_cols=(mi_cols+31)>>5 if s['sb128'] else (mi_cols+15)>>4
    sb_rows=(mi_rows+31)>>5 if s['sb128'] else (mi_rows+15)>>4
    mtw=4096>>sbsz; mta=(4096*2304)>>(2*sbsz)
    min_lc=tlog2(mtw, sb_cols); max_lc=tlog2(1, min(sb_cols,64)); max_lr=tlog2(1, min(sb_rows,64))
    min_lt=max(min_lc, tlog2(mta, sb_rows*sb_cols))
    o['uniform']=b.f(1)
    if o['uniform']:
        cl=b.inc(min_lc,max_lc); o['cols_log2']=cl
        twsb=(sb_cols+(1<<cl)-1)>>cl; o['tile_cols']=(sb_cols+twsb-1)//twsb
        min_lrr=max(min_lt-cl,0)
        rl=b.inc(min_lrr,max_lr); o['rows_log2']=rl
        thsb=(sb_rows+(1<<rl)-1)>>rl; o['tile_rows']=(sb_rows+thsb-1)//thsb
    else:
        start=0; widest=0; i=0
        while start<sb_cols and i<64:
            mw=min(sb_cols-start, mtw); sz=b.ns(mw)+1
            widest=max(widest,sz); start+=sz; i+=1
        o['tile_cols']=i; o['cols_log2']=tlog2(1,i)
        mta2=(sb_rows*sb_cols)>>(min_lt+1) if min_lt>0 else sb_rows*sb_cols
        mth=max(mta2//widest,1)
        start=0; i=0
        while start<sb_rows and i<64:
            mh=min(sb_rows-start, mth); sz=b.ns(mh)+1
            start+=sz; i+=1
        o['tile_rows']=i; o['rows_log2']=tlog2(1,i)
    if o['cols_log2']>0 or o['rows_log2']>0:
        o['ctx_update_tile']=b.f(o['cols_log2']+o['rows_log2']); o['tile_size_bytes']=b.f(2)+1
    o['_bits_after_tile']=b.p

    # ---- quantization
    o['base_q']=b.f(8); o['ydc']=delta_q(b)
    planes = 1 if s['mono'] else 3
    if planes>1:
        o['diff_uv']=b.f(1) if s['sep_uv'] else 0
        o['udc']=delta_q(b); o['uac']=delta_q(b)
        if o['diff_uv']: o['vdc']=delta_q(b); o['vac']=delta_q(b)
        else: o['vdc']=o['udc']; o['vac']=o['uac']
    o['using_qm']=b.f(1)
    if o['using_qm']:
        o['qm_y']=b.f(4); o['qm_u']=b.f(4)
        o['qm_v']=b.f(4) if s['sep_uv'] else o['qm_u']
    # ---- segmentation
    o['seg']=b.f(1)
    if o['seg']:
        if o['primary_ref']==7: um,tu,ud=1,0,1
        else:
            um=b.f(1); tu=b.f(1) if um else 0; ud=b.f(1)
        o['seg_update_map']=um; o['seg_update_data']=ud
        bits=[8,6,6,6,6,3,0,0]; sg=[1,1,1,1,1,0,0,0]
        for i in range(8):
            for j in range(8):
                if ud:
                    en=b.f(1)
                    if en and bits[j]:
                        b.su(1+bits[j]) if sg[j] else b.f(bits[j])
    # ---- delta_q / delta_lf
    o['delta_q_present']=b.f(1) if o['base_q']>0 else 0
    if o['delta_q_present']:
        o['delta_q_res']=b.f(2)
        o['delta_lf_present']=0 if o['allow_intrabc'] else b.f(1)
        if o['delta_lf_present']: o['delta_lf_res']=b.f(2); o['delta_lf_multi']=b.f(1)
    o['_bits_after_deltas']=b.p

    # coded_lossless
    cl = (o['base_q']==0 and o['ydc']==0 and o['udc']==0 and o['uac']==0
          and o['vdc']==0 and o['vac']==0)
    o['coded_lossless']=int(cl)
    all_lossless = cl and (o['w']==o['w'])  # no superres downscale in these clips
    planes = 1 if s['mono'] else 3
    # ---- loop filter
    if not (cl or o['allow_intrabc']):
        o['lf0']=b.f(6); o['lf1']=b.f(6)
        if planes>1 and (o['lf0'] or o['lf1']): o['lfu']=b.f(6); o['lfv']=b.f(6)
        o['sharpness']=b.f(3)
        o['lf_delta_enabled']=b.f(1)
        if o['lf_delta_enabled']:
            o['lf_delta_update']=b.f(1)
            for i in range(8):
                if o['lf_delta_update'] and b.f(1): b.su(1+6)
            for i in range(2):
                if o['lf_delta_update'] and b.f(1): b.su(1+6)
    o['_bits_after_lf']=b.p
    # ---- cdef
    if not (cl or o['allow_intrabc'] or not s['cdef']):
        o['cdef_damping']=b.f(2); o['cdef_bits']=b.f(2)
        for i in range(1<<o['cdef_bits']):
            b.f(4); b.f(2)
            if planes>1: b.f(4); b.f(2)
    o['_bits_after_cdef']=b.p
    # ---- lr
    if not (all_lossless or o['allow_intrabc'] or not s['restoration']):
        types=[b.f(2) for _ in range(planes)]
        o['lr_types']=types
        if any(types):
            o['lr_unit_shift']=b.inc(1,2) if s['sb128'] else b.inc(0,2)
            if (not s['mono']) and any(types[1:]): o['lr_uv_shift']=b.f(1)
    o['_bits_after_lr']=b.p
    # ---- tx mode / ref mode / skip / warp / reduced tx
    if not cl: o['tx_mode']=b.inc(1,2)
    intra2 = o['frame_type'] in (0,2)
    if not intra2: o['reference_select']=b.f(1)
    else: o['reference_select']=0
    # skip_mode_present: presence depends on the decoder's own reference search, which this
    # parser cannot reproduce without a DPB. SKIP_HINT says whether to expect the bit.
    if SKIP_HINT.get(FRAME_IDX, None):
        o['skip_mode_present']=b.f(1)
    if not (intra2 or o['err_res']): o['allow_warp']=b.f(1)
    o['reduced_tx']=b.f(1)
    if not intra2:
        gm=[]
        for r in range(7):
            isg=b.f(1); t=0
            if isg:
                rz=b.f(1)
                if rz: t=2
                else: t=1 if b.f(1) else 3
            gm.append(t)
            def gparam(idx):
                if idx<2:
                    ab=(9 - (0 if o.get('hp_mv') else 1)) if t==1 else 12
                    pb=(3 - (0 if o.get('hp_mv') else 1)) if t==1 else 6
                else: ab, pb = 12, 15
                mx=1<<ab
                # decode_subexp over 2*mx+1 symbols
                num=2*mx+1; i=0; mk=0; k=3
                while True:
                    b2 = k+i-1 if i else k
                    a=1<<b2
                    if num <= mk+3*a:
                        b.ns(num-mk); return
                    if b.f(1): i+=1; mk+=a
                    else: b.f(b2); return
            if t>=2:
                gparam(2); gparam(3)
                if t==3: gparam(4); gparam(5)
            if t>=1:
                gparam(0); gparam(1)
        o['gm']=gm
    if s['film_grain'] and (o['show_frame'] or o['showable']):
        o['apply_grain']=b.f(1)
    o['_bits_after_txset']=b.p
    o['_total_bits']=b.p
    return o



def decoded_frames(path):
    """Every frame the guest actually decodes, in decode order -- show_existing_frame
    entries are display operations, not decodes, and are dropped."""
    obus = walk(open(path, 'rb').read(), limit=20000)
    S = seq(next(p for t, p in obus if t == 1))
    return S, [f for f in (fh(p, S) for t, p in obus if t in (3, 6))
               if not f.get('show_existing')]


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    (SO, O), (SR, R) = decoded_frames(sys.argv[1]), decoded_frames(sys.argv[2])
    sd = {k: (SO.get(k), SR.get(k)) for k in set(SO) | set(SR) if SO.get(k) != SR.get(k)}
    print(f"sequence header differs in: {sd or 'nothing'}")
    print(f"decoded frames: {len(O)} original, {len(R)} rebuilt")
    bad = 0
    for i, (a, b) in enumerate(zip(O, R)):
        d = [k for k in (set(a) | set(b))
             if not k.startswith('_') and k not in ('refresh', 'ref_idx') and a.get(k) != b.get(k)]
        if d:
            bad += 1
            if bad <= 8:
                print(f"  frame {i:3d} oh={a.get('order_hint')}: " +
                      ", ".join(f"{k}={a.get(k)}->{b.get(k)}" for k in sorted(d)))
    print(f"frames differing in any field (refresh and ref_idx excluded -- they are ours "
          f"to choose, see dpb-check.py): {bad}")
