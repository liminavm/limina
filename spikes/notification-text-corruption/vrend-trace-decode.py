#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Decode a dump from vrend's in-memory command tracer (LIMINA_VREND_TRACE, see
# third_party/virglrenderer/src/vrend/vrend_trace.[ch]).
#
# The tracer buffers in RAM and writes only on request, because the fault it exists to observe is
# cured by a bare glFlush -- a tracer doing per-command I/O would move the very boundary under
# test. That means the dump is a WINDOW, not a session: `evicted` in the header is the count of
# records that aged out, and a nonzero value means the trace no longer reaches the start.
#
#   vrend-trace-decode.py <dump>                    summary: record and command histogram
#   vrend-trace-decode.py <dump> --fb 968x44        every draw into a target of that size
#   vrend-trace-decode.py <dump> --around <seq> [n] full context around one record
#   vrend-trace-decode.py <dump> --list             every record, one per line
#   vrend-trace-decode.py <dump> --ctx              draw targets per virgl context
#   vrend-trace-decode.py <dump> --uploads          how each glyph draw's vertex data arrived
#   vrend-trace-decode.py <dump> --fingerprint [ctx] full draw state of every glyph draw
#   vrend-trace-decode.py <dump> --resources       the resource create/destroy log
#   vrend-trace-decode.py <dump> --replayable      can this trace be replayed at all?
#
# --uploads is the comparison that characterised this bug. For every draw whose vertex bindings
# include a zero stride (the constant-colour attribute unique to the glyph pipeline) it reports
# the buffer, the upload that fed it, and -- the load-bearing number -- how many SUBMIT batch
# boundaries sit between that upload and the draw. gnome-shell holds 0 on every glyph draw.
#
# Always split by CONTEXT, never by sequence window: separate processes get separate contexts.
# And context ids are REUSED, because the ring outlives a process -- two consecutive runs of one
# probe land in the same ctx, and reading that as a single run silently mixes arms.
import struct, sys, collections

CCMD = {
    0: "NOP",
    1: "CREATE_OBJECT",
    2: "BIND_OBJECT",
    3: "DESTROY_OBJECT",
    4: "SET_VIEWPORT_STATE",
    5: "SET_FRAMEBUFFER_STATE",
    6: "SET_VERTEX_BUFFERS",
    7: "CLEAR",
    8: "DRAW_VBO",
    9: "RESOURCE_INLINE_WRITE",
    10: "SET_SAMPLER_VIEWS",
    11: "SET_INDEX_BUFFER",
    12: "SET_CONSTANT_BUFFER",
    13: "SET_STENCIL_REF",
    14: "SET_BLEND_COLOR",
    15: "SET_SCISSOR_STATE",
    16: "BLIT",
    17: "RESOURCE_COPY_REGION",
    18: "BIND_SAMPLER_STATES",
    19: "BEGIN_QUERY",
    20: "END_QUERY",
    21: "GET_QUERY_RESULT",
    22: "SET_POLYGON_STIPPLE",
    23: "SET_CLIP_STATE",
    24: "SET_SAMPLE_MASK",
    25: "SET_STREAMOUT_TARGETS",
    26: "SET_RENDER_CONDITION",
    27: "SET_UNIFORM_BUFFER",
    28: "SET_SUB_CTX",
    29: "CREATE_SUB_CTX",
    30: "DESTROY_SUB_CTX",
    31: "BIND_SHADER",
    32: "SET_TESS_STATE",
    33: "SET_MIN_SAMPLES",
    34: "SET_SHADER_BUFFERS",
    35: "SET_SHADER_IMAGES",
    36: "MEMORY_BARRIER",
    37: "LAUNCH_GRID",
    38: "SET_FRAMEBUFFER_STATE_NO_ATTACH",
    39: "TEXTURE_BARRIER",
    40: "SET_ATOMIC_BUFFERS",
    41: "SET_DEBUG_FLAGS",
    42: "GET_QUERY_RESULT_QBO",
    43: "TRANSFER3D",
    44: "END_TRANSFERS",
    45: "COPY_TRANSFER3D",
    46: "SET_TWEAKS",
    47: "CLEAR_TEXTURE",
    48: "PIPE_RESOURCE_CREATE",
    49: "PIPE_RESOURCE_SET_TYPE",
    50: "GET_MEMORY_INFO",
    51: "SEND_STRING_MARKER",
    52: "LINK_SHADER",
    53: "CREATE_VIDEO_CODEC",
    54: "DESTROY_VIDEO_CODEC",
    55: "CREATE_VIDEO_BUFFER",
    56: "DESTROY_VIDEO_BUFFER",
    57: "BEGIN_FRAME",
    58: "DECODE_MACROBLOCK",
    59: "DECODE_BITSTREAM",
    60: "ENCODE_BITSTREAM",
    61: "END_FRAME",
    62: "CLEAR_SURFACE",
    63: "GET_PIPE_RESOURCE_LAYOUT",
}

TYPES = {1: "SUBMIT", 2: "CMD", 3: "DRAW_FB", 4: "TRANSFER", 5: "FENCE", 6: "RETIRE", 7: "PAD",
         9: "XFERDATA"}
RES_KIND = {0: "create", 1: "blob", 2: "unref"}
# struct vrend_trace_res: u64 seq, then kind + 11 u32 fields.
RES = struct.Struct("<Q12I")
HDR = struct.Struct("<IBBHQQII")   # total_len, type, cmd, ctx, seq, mono_ns, payload_len, aux_count


def load(path):
    blob = open(path, "rb").read()
    head = struct.unpack("<16I", blob[:64])
    if head[0] != 0x4C4D5654:
        sys.exit("not a vrend trace dump (bad magic)")
    meta = {
        "version": head[1], "ring_mb": head[2], "used": head[3],
        "records": head[4] | (head[5] << 32),
        "evicted": head[6] | (head[7] << 32),
        "base_mono_ns": head[8] | (head[9] << 32),
        "base_real_ns": head[10] | (head[11] << 32),
    }
    # v2 puts the resource log between the header and the ring: resources are created on the
    # CONTROL path, never in the command stream, so a trace without this cannot be replayed.
    meta["res_full"] = bool(head[13]) if meta["version"] >= 2 else False
    resources, off = [], 64
    if meta["version"] >= 2:
        for _ in range(head[12]):
            f = RES.unpack_from(blob, off)
            resources.append({"seq": f[0], "kind": f[1], "handle": f[2], "target": f[3],
                              "format": f[4], "bind": f[5], "width": f[6], "height": f[7],
                              "depth": f[8], "array_size": f[9], "last_level": f[10],
                              "nr_samples": f[11], "flags": f[12]})
            off += RES.size
    meta["resources"] = resources
    recs = []
    while off + HDR.size <= len(blob):
        total, typ, cmd, ctx, seq, mono, plen, aux_n = HDR.unpack_from(blob, off)
        if total < HDR.size or off + total > len(blob):
            break
        a_off = off + HDR.size
        aux = struct.unpack_from("<%dI" % aux_n, blob, a_off) if aux_n else ()
        payload = blob[a_off + aux_n * 4: a_off + aux_n * 4 + plen]
        if typ != 7:
            recs.append((seq, typ, cmd, ctx, mono, aux, payload))
        off += total
    return meta, recs


def describe(r):
    seq, typ, cmd, ctx, mono, aux, payload = r
    t = TYPES.get(typ, "?%d" % typ)
    if typ == 2:
        return "%-8s %-24s ctx=%d dwords=%d" % (t, CCMD.get(cmd, "CMD%d" % cmd), ctx, len(payload) // 4)
    if typ == 3:
        return "%-8s %dx%d cbufs=%d gl_id=%d" % (t, aux[0], aux[1], aux[2], aux[3])
    if typ == 4:
        return ("%-8s res=%d mode=%d level=%d box=%d,%d %dx%d stride=%d"
                % (t, aux[0], aux[1], aux[2], aux[3], aux[4], aux[5], aux[6], aux[7]))
    if typ == 9:
        off = aux[1] | (aux[2] << 32) if len(aux) > 2 else 0
        return "%-8s res=%d offset=%d bytes=%d" % (t, aux[0], off, len(payload))
    if typ == 5:
        fid = struct.unpack("<Q", payload)[0] if len(payload) == 8 else 0
        return "%-8s ctx=%d flags=%d id=%d" % (t, ctx, aux[0] if aux else 0, fid)
    if typ == 6:
        fid = struct.unpack("<Q", payload)[0] if len(payload) == 8 else 0
        return "%-8s ctx=%d id=%d" % (t, ctx, fid)
    if typ == 1:
        return "%-8s ctx=%d bytes=%d" % (t, ctx, aux[0] if aux else 0)
    return t


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    path = sys.argv[1]
    meta, recs = load(path)
    args = sys.argv[2:]

    if meta.get("resources"):
        print("resource log: %d events%s"
              % (len(meta["resources"]),
                 "  <-- FULL, trace is NOT replayable" if meta["res_full"] else ""))
    print("ring %d MB, %d bytes live, %d records total, %d EVICTED%s"
          % (meta["ring_mb"], meta["used"], meta["records"], meta["evicted"],
             "  <-- window does not reach the start" if meta["evicted"] else ""))
    print("decoded %d records, seq %s..%s"
          % (len(recs), recs[0][0] if recs else "-", recs[-1][0] if recs else "-"))

    # Integrity gate, not a nicety. Records are appended from more than one thread (fences retire
    # on vrend's poll thread), and the first version of the recorder had no lock -- which showed up
    # here, and ONLY here, as one sequence number claimed by two records. A trace that silently
    # invents or tears records would corrupt every conclusion drawn from it, so check every time.
    seen = collections.Counter(r[0] for r in recs)
    dups = [s for s, n in seen.items() if n > 1]
    if dups:
        print("  !! %d DUPLICATE seq (torn append -- trace is NOT trustworthy): %s"
              % (len(dups), dups[:8]))
    gaps = len(seen) != len(recs)
    if not dups and not gaps:
        print("  integrity: %d distinct seqs, no duplicates" % len(seen))

    if not recs:
        return

    if "--fb" in args:
        want = args[args.index("--fb") + 1].lower()
        w, h = (int(x) for x in want.split("x"))
        # A DRAW_FB record is emitted immediately before its draw command, so the pair reads
        # naturally in sequence order.
        hits = [r for r in recs if r[1] == 3 and r[5][0] == w and r[5][1] == h]
        print("\n%d draws into a %dx%d target:" % (len(hits), w, h))
        for r in hits:
            print("  seq %-9d +%.6fs  %s" % (r[0], (r[4] - meta["base_mono_ns"]) / 1e9, describe(r)))
        return

    if "--around" in args:
        i = args.index("--around")
        target = int(args[i + 1])
        span = int(args[i + 2]) if len(args) > i + 2 and not args[i + 2].startswith("-") else 40
        for r in recs:
            if abs(r[0] - target) <= span:
                mark = "->" if r[0] == target else "  "
                print("%s seq %-9d +%.6fs  %s" % (mark, r[0], (r[4] - meta["base_mono_ns"]) / 1e9, describe(r)))
        return

    if "--ctx" in args or "--uploads" in args:
        # Split by virgl CONTEXT, never by sequence window: different processes get different
        # contexts, and gnome-shell's stream and a probe's are only separable this way. Beware
        # that context ids are REUSED -- the ring outlives a process, so two consecutive runs of
        # the same probe land in one ctx and their draws must be separated by hand.
        bycx = collections.Counter(r[3] for r in recs)
        print("\nrecords per virgl context:", dict(bycx.most_common()))
        tg = collections.defaultdict(collections.Counter)
        for r in recs:
            if r[1] == 3:
                tg[r[3]]["%dx%d" % (r[5][0], r[5][1])] += 1
        for cx in sorted(tg):
            print("  ctx %-4d draw targets: %s" % (cx, dict(tg[cx].most_common(6))))
        if "--uploads" not in args:
            return

        # For every draw whose vertex bindings include a ZERO stride -- the constant-colour
        # attribute that marks the glyph pipeline -- report how its vertex data arrived.
        print("\nglyph-pipeline draws (a zero-stride binding), by context:")
        for cx in sorted(bycx):
            g = [r for r in recs if r[3] == cx]
            cur, last, rows = None, {}, []
            for i, r in enumerate(g):
                if r[1] == 4:
                    last[r[5][0]] = (i, r[4], r[5][5])
                if r[1] == 2 and CCMD.get(r[2]) == "SET_VERTEX_BUFFERS":
                    dw = struct.unpack("<%dI" % (len(r[6]) // 4), r[6])
                    n = (len(dw) - 1) // 3
                    cur = (tuple(dw[1 + 3 * k] for k in range(n)), dw[3] if n else None)
                if r[1] == 3 and cur and len(cur[0]) == 3 and 0 in cur[0] and cur[1] in last:
                    ui, ut, usz = last[cur[1]]
                    nsub = sum(1 for x in g[ui + 1:i] if x[1] == 1)
                    rows.append((cur[1], (r[4] - ut) / 1e6, usz, nsub, ui))
            if not rows:
                continue
            ms = sorted(x[1] for x in rows)
            print("  ctx %-4d %d draws | %d distinct vertex buffers | %d distinct uploads"
                  % (cx, len(rows), len({x[0] for x in rows}), len({x[4] for x in rows})))
            print("           upload->draw ms: median %.3f (min %.3f max %.3f) | sizes %s"
                  % (ms[len(ms) // 2], ms[0], ms[-1],
                     dict(collections.Counter(x[2] for x in rows).most_common(3))))
            # The load-bearing one: gnome-shell holds 0 on every draw.
            print("           SUBMIT batches between upload and draw: %s"
                  % dict(sorted(collections.Counter(x[3] for x in rows).items())))
        return

    if "--fingerprint" in args:
        # The per-round validity gate for the mimic. For every glyph-pipeline draw it prints the
        # full state the draw actually executes under -- framebuffer attachments (INCLUDING the
        # depth surface, which the mimic lacked for a long time because nothing printed it),
        # whether those surface objects are FRESH or reused, the vertex-binding triplets with
        # their resources and offsets, the index buffer, and the draw's own counts.
        #
        # Diff this between the gnome-shell context and the mimic's. Matching strides is not
        # matching the draw: the constant attribute arrives from a DIFFERENT resource at a rolling
        # offset in the real stream, and that is invisible unless the resources are printed.
        want = None
        i = args.index("--fingerprint")
        if len(args) > i + 1 and not args[i + 1].startswith("-"):
            want = int(args[i + 1])
        surf, fb, vbs, ib, created = {}, None, None, None, set()
        for r in recs:
            if want is not None and r[3] != want:
                continue
            if r[1] != 2:
                if r[1] == 3 and vbs and len(vbs) == 3 and any(s == 0 for s, _, _ in vbs):
                    print("\nseq %-9d +%.6fs  glyph draw into %dx%d (gl_id=%d) ctx=%d"
                          % (r[0], (r[4] - meta["base_mono_ns"]) / 1e9, r[5][0], r[5][1], r[5][3], r[3]))
                    if fb:
                        z, c = fb[2], fb[3] if len(fb) > 3 else 0
                        print("  framebuffer: nr_cbufs=%d cbuf0=%d%s zsurf=%d%s"
                              % (fb[1], c, "(fresh)" if c in created else "", z,
                                 "(fresh)" if z in created else "" if z else " NONE"))
                        for h in (c, z):
                            if h in surf:
                                print("    surface %-5d -> res %-5d fmt %d level %d layers %d"
                                      % ((h,) + surf[h]))
                    for k, (s, o, res) in enumerate(vbs):
                        print("  binding %d: stride=%-4d offset=%-6d res=%d" % (k, s, o, res))
                    if ib:
                        print("  index buffer: res=%d index_size=%d offset=%d" % ib)
                continue
            name = CCMD.get(r[2])
            dw = struct.unpack("<%dI" % (len(r[6]) // 4), r[6]) if len(r[6]) >= 4 else ()
            if name == "CREATE_OBJECT" and dw and ((dw[0] >> 8) & 0xFF) == 8 and len(dw) >= 6:
                surf[dw[1]] = (dw[2], dw[3], dw[4], dw[5])
                created.add(dw[1])
            elif name == "SET_FRAMEBUFFER_STATE":
                fb = dw
            elif name == "SET_VERTEX_BUFFERS":
                n = (len(dw) - 1) // 3
                vbs = [(dw[1 + 3 * k], dw[2 + 3 * k], dw[3 + 3 * k]) for k in range(n)]
            elif name == "SET_INDEX_BUFFER" and len(dw) >= 4:
                ib = (dw[1], dw[2], dw[3])
            elif name == "DRAW_VBO" and len(dw) >= 5:
                if vbs and len(vbs) == 3 and any(s == 0 for s, _, _ in vbs):
                    print("  draw: start=%d count=%d mode=%d indexed=%d instances=%d"
                          % (dw[1], dw[2], dw[3], dw[4], dw[5] if len(dw) > 5 else 1))
                created.clear()
        return

    if "--replayable" in args:
        # Completeness gate for stream replay, the same class of check as the duplicate-seq gate
        # above: a trace that LOOKS complete but is not must announce itself, or the replayer's
        # failures get chased as replayer bugs.
        #
        # Every handle the command stream touches must have a create event that precedes its first
        # use. The way this fails is structural, not random: resources are created on the CONTROL
        # path, and the guest KMS driver makes its scanout and cursor resources at boot -- long
        # before any 3D client submits. Anything created while the tracer was still disarmed is
        # referenced forever and logged never.
        births, deaths = {}, {}
        for e in meta.get("resources", []):
            if e["kind"] == 2:
                deaths.setdefault(e["handle"], []).append(e["seq"])
            else:
                births.setdefault(e["handle"], []).append(e["seq"])
        refs = collections.defaultdict(list)   # handle -> [(seq, what)]

        def ref(h, seq, what):
            if h:
                refs[h].append((seq, what))

        for r in recs:
            if r[1] != 2 or len(r[6]) < 4:
                continue
            dw = struct.unpack("<%dI" % (len(r[6]) // 4), r[6])
            name = CCMD.get(r[2])
            if name == "TRANSFER3D" and len(dw) > 1:
                ref(dw[1], r[0], "TRANSFER3D")
            elif name == "COPY_TRANSFER3D":
                if len(dw) > 1:
                    ref(dw[1], r[0], "COPY_TRANSFER3D dst")
                if len(dw) > 12:
                    ref(dw[12], r[0], "COPY_TRANSFER3D src")
            elif name == "SET_VERTEX_BUFFERS":
                for k in range((len(dw) - 1) // 3):
                    ref(dw[3 + 3 * k], r[0], "SET_VERTEX_BUFFERS[%d]" % k)
            elif name == "SET_INDEX_BUFFER" and len(dw) > 1:
                ref(dw[1], r[0], "SET_INDEX_BUFFER")
            elif name == "RESOURCE_INLINE_WRITE" and len(dw) > 1:
                ref(dw[1], r[0], "RESOURCE_INLINE_WRITE")
            elif name in ("BLIT", "RESOURCE_COPY_REGION") and len(dw) > 2:
                ref(dw[1], r[0], name + " dst")
                ref(dw[2], r[0], name + " src")
            elif name == "CREATE_OBJECT" and len(dw) > 2:
                kind = (dw[0] >> 8) & 0xFF
                if kind in (3, 8):   # sampler view, surface -- both name a resource
                    ref(dw[2], r[0], "CREATE_OBJECT %s" % ("sampler_view" if kind == 3 else "surface"))

        missing, late = [], []
        for h, uses in refs.items():
            first = min(u[0] for u in uses)
            b = births.get(h)
            if not b:
                missing.append((h, first, len(uses), uses[0][1]))
            elif min(b) > first:
                late.append((h, first, min(b), len(uses)))

        print("\nreplayability: %d distinct handles referenced by the stream, %d created in the log"
              % (len(refs), len(births)))
        if late:
            print("  !! %d handles first used BEFORE their create event (seq skew or reuse)" % len(late))
            for h, f, b, n in sorted(late)[:10]:
                print("     res %-6d first use seq %-9d create seq %-9d (%d uses)" % (h, f, b, n))
        if missing:
            tot = sum(m[2] for m in missing)
            print("  !! %d handles NEVER created in the log (%d references) -- trace is NOT replayable"
                  % (len(missing), tot))
            for h, f, n, what in sorted(missing, key=lambda m: -m[2])[:15]:
                print("     res %-6d %5d refs, first at seq %-9d as %s" % (h, n, f, what))
        else:
            print("  every referenced handle has a create event: replayable")
        return

    if "--resources" in args:
        for r in meta.get("resources", []):
            if r["kind"] == 2:
                print("seq %-9d unref  res=%d" % (r["seq"], r["handle"]))
            else:
                print("seq %-9d %-6s res=%-6d target=%d format=%d bind=0x%x %dx%dx%d "
                      "array=%d levels=%d samples=%d flags=0x%x"
                      % (r["seq"], RES_KIND.get(r["kind"], "?"), r["handle"], r["target"],
                         r["format"], r["bind"], r["width"], r["height"], r["depth"],
                         r["array_size"], r["last_level"], r["nr_samples"], r["flags"]))
        return

    if "--list" in args:
        for r in recs:
            print("seq %-9d +%.6fs  %s" % (r[0], (r[4] - meta["base_mono_ns"]) / 1e9, describe(r)))
        return

    by_type = collections.Counter(TYPES.get(r[1], "?") for r in recs)
    by_cmd = collections.Counter(CCMD.get(r[2], "CMD%d" % r[2]) for r in recs if r[1] == 2)
    fbs = collections.Counter("%dx%d" % (r[5][0], r[5][1]) for r in recs if r[1] == 3)
    span = (recs[-1][4] - recs[0][4]) / 1e9
    print("\nwindow spans %.3f s" % span)
    print("\nrecords by type:")
    for k, v in by_type.most_common():
        print("  %-10s %d" % (k, v))
    print("\ncommands:")
    for k, v in by_cmd.most_common(20):
        print("  %-28s %d" % (k, v))
    print("\ndraw targets (size: count):")
    for k, v in fbs.most_common(15):
        print("  %-14s %d" % (k, v))


main()
