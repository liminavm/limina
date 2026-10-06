#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Instrument KosmicKrisp's Metal bridge with a per-function call/byte/time tally.

Every public entry point declared in src/kosmickrisp/bridge/*.h gets a scope at the top of its
body that counts the call, an estimate of what it would cost on a wire to a host replayer, and
the time spent inside it. See RESULTS.md for what the numbers mean and what they do not.

Usage: gen_tally.py <mesa source tree>
Writes bridge/mtl_tally.{h,c}, edits the bridge .m files and meson.build in place. Refuses a tree
that is already instrumented; reset it with `git checkout src/kosmickrisp/bridge` first.
"""
import pathlib
import re
import sys

HANDLE_TYPES = None  # filled from mtl_types.h: every `typedef void mtl_*;`

# Classes, in the order the report prints them.
#   async  - void return: can be queued and never waited on
#   create - returns a new object: async too, with a guest-allocated id
#   sync   - the guest needs an answer: a round trip in a split design
#   local  - would be free in the guest (its own memory) or never cross at all
#   diag   - limina diagnostics and debug labels: not part of the protocol
CLASSES = ["async", "create", "sync", "local", "diag"]

# Functions whose class is not what the return type says.
CLASS_OVERRIDE = {
    "mtl_get_contents": "local",  # guest memory itself in a guest-KK design
    "mtl_retain": "async",
    "mtl_release": "async",
    "mtl_device_create": "sync",
    "mtl_shared_event_wait_until_signaled_value": "sync",
    "mtl_texture_get_bytes": "sync",
    "mtl_heap_buffer_size_and_align_with_length": "sync",
    "mtl_heap_texture_size_and_align_with_descriptor": "sync",
    "mtl_texture_get_props": "sync",
    "mtl_device_get_name": "sync",
    "mtl_device_get_architecture_name": "sync",
    # Read Metal-chosen ids/addresses back: the guest writes them into descriptors.
    "mtl_buffer_get_gpu_address": "sync",
    "mtl_texture_get_gpu_resource_id": "sync",
    "mtl_sampler_get_gpu_resource_id": "sync",
    # Getters of a sub-object the guest could name itself.
    "mtl_render_pass_descriptor_get_color_attachment": "local",
    "mtl_render_pass_descriptor_get_depth_attachment": "local",
    "mtl_render_pass_descriptor_get_stencil_attachment": "local",
}
DIAG_PREFIXES = ("limina_", "mtl_limina_", "mtl_encoder_note_", "mtl_encoder_guard_",
                 "mtl_encoder_report_", "mtl_encoder_generation", "mtl_start_gpu_capture",
                 "mtl_stop_gpu_capture", "mtl_handle_is_iosurface")
DIAG_NAMES = {"mtl_command_buffer_set_label", "mtl_encoder_set_label",
              "mtl_encoder_insert_debug_signpost", "mtl_encoder_push_debug_group",
              "mtl_encoder_pop_debug_group", "mtl_heap_set_label", "mtl_resource_set_label",
              "mtl_render_pipeline_descriptor_set_label", "mtl_command_allocator_allocated_size"}

# Per-parameter wire-byte overrides (C expressions over the parameter names).
PARAM_WIRE = {
    ("mtl_command_queue_commit", "command_buffers"): "(uint64_t)count * 4",
    ("limina_kk_alloc_track_commit", "cbs"): "(uint64_t)count * 4",
    ("mtl_set_viewports", "viewports"): "(uint64_t)count * sizeof(*viewports)",
    ("mtl_set_scissor_rects", "scissor_rects"): "(uint64_t)count * sizeof(*scissor_rects)",
    ("mtl_set_vertex_amplification_count", "layer_ids"): "(uint64_t)id_count * sizeof(*layer_ids)",
    ("mtl_render_pass_descriptor_set_sample_positions", "positions"):
        "(uint64_t)count * sizeof(*positions)",
    # A CPU upload carries its pixels.
    ("mtl_texture_replace_region", "host_ptr"):
        "(uint64_t)data->buffer_2d_image_size_B * (data->image_size.z ? data->image_size.z : 1)",
    ("mtl_texture_get_bytes", "host_ptr"): "8",
    # Guest memory handed to Metal: an address and a length, never the bytes.
    ("mtl_new_buffer_with_bytes_no_copy", "ptr"): "8",
    ("mtl_command_queue_wait_for_drawable", "drawable"): "8",
    ("mtl_command_queue_signal_drawable", "drawable"): "8",
    ("mtl_drawable_get_texture", "drawable_ptr"): "8",
    ("mtl_commit_options_add_feedback_handler", "data"): "8",
    ("mtl_new_texture_with_descriptor_iosurface", "iosurface"): "4",
}
# Reply bytes beyond the return value (out-parameters, readbacks).
REPLY = {
    "mtl_texture_get_bytes":
        "(uint64_t)data->buffer_2d_image_size_B * (data->image_size.z ? data->image_size.z : 1)",
    "mtl_heap_buffer_size_and_align_with_length": "16",
    "mtl_heap_texture_size_and_align_with_descriptor": "16",
    "mtl_texture_get_props": "sizeof(*props)",
    "mtl_device_get_name": "256",
    "mtl_device_get_architecture_name": "256",
}
OUT_PARAMS = {("mtl_heap_buffer_size_and_align_with_length", "size_B"),
              ("mtl_heap_buffer_size_and_align_with_length", "align_B"),
              ("mtl_heap_texture_size_and_align_with_descriptor", "size_B"),
              ("mtl_heap_texture_size_and_align_with_descriptor", "align_B"),
              ("mtl_texture_get_props", "props"),
              ("mtl_device_get_name", "buffer"),
              ("mtl_device_get_architecture_name", "buffer"),
              ("mtl_encoder_guard_stats", "out")}

HEADER_BYTES = 8  # opcode + length per call


def split_params(s):
    out, depth, cur = [], 0, ""
    for ch in s:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return [p.strip() for p in out if p.strip() and p.strip() != "void"]


def param_name_type(p):
    p = re.sub(r"\[\d*\]$", "*", p.strip())  # char buffer[256] -> pointer
    m = re.match(r"(.*?)([A-Za-z_][A-Za-z0-9_]*)\s*(\*?)$", p)
    typ, name = m.group(1).strip(), m.group(2)
    if m.group(3):
        typ += "*"
    return name, typ


def wire_expr(fn, name, typ):
    if (fn, name) in PARAM_WIRE:
        return PARAM_WIRE[(fn, name)]
    if (fn, name) in OUT_PARAMS:
        return "0"
    stars = typ.count("*")
    base = re.sub(r"\b(const|struct|enum)\b", "", typ.replace("*", "")).strip()
    if stars == 0:
        return f"sizeof({name})"
    if stars == 1 and base == "char":
        return f"({name} ? strlen({name}) + 1 : 0)"
    if stars == 1 and (base in HANDLE_TYPES or base == "void"):
        return "4"  # an object id
    if stars == 1 and "struct" in typ:
        return f"sizeof(*{name})"
    if stars == 1:
        return f"sizeof(*{name})"
    raise SystemExit(f"no wire rule for {fn}({typ} {name})")


def classify(fn, ret):
    if fn in DIAG_NAMES or fn.startswith(DIAG_PREFIXES):
        return "diag"
    if fn in CLASS_OVERRIDE:
        return CLASS_OVERRIDE[fn]
    r = ret.replace("const", "").strip()
    if r == "void":
        return "async"
    base = r.replace("*", "").strip()
    if r.endswith("*") and base in HANDLE_TYPES:
        return "create"
    return "sync"


def reply_expr(fn, ret, cls):
    extra = REPLY.get(fn)
    parts = []
    if cls == "sync" and ret.strip() != "void":
        parts.append(f"sizeof({ret})")
    if extra:
        parts.append(extra)
    return " + ".join(parts) if parts else "0"


def main():
    global HANDLE_TYPES
    tree = pathlib.Path(sys.argv[1])
    bridge = tree / "src/kosmickrisp/bridge"
    if (bridge / "mtl_tally.h").exists():
        raise SystemExit("already instrumented")
    HANDLE_TYPES = set(re.findall(r"typedef void (mtl_[a-z0-9_]+);",
                                  (bridge / "mtl_types.h").read_text()))

    declared = set()
    for h in bridge.glob("*.h"):
        declared |= set(re.findall(r"\b((?:mtl|limina)_[a-z0-9_]+)\s*\(", h.read_text()))

    entries = []  # (name, class)
    for m_file in sorted(bridge.glob("*.m")):
        lines = m_file.read_text().split("\n")
        out, i, touched = [], 0, False
        while i < len(lines):
            line = lines[i]
            # The return type is on the line above, or (a few) on the same line.
            dm = re.match(r"^(?:([A-Za-z_][A-Za-z0-9_ ]*?[ *]))?((?:mtl|limina)_[a-z0-9_]+)\(", line)
            if dm and dm.group(2) in declared and i > 0 and not lines[i - 1].startswith("static") \
                    and not (dm.group(1) or "").startswith("static"):
                fn = dm.group(2)
                ret = (dm.group(1) or lines[i - 1]).strip()
                # collect the signature up to the body's opening brace
                sig, j = line, i
                while "{" not in lines[j]:
                    j += 1
                    sig += " " + lines[j]
                out.extend(lines[i:j + 1])
                params = sig[sig.index("(") + 1: sig.rindex(")")]
                plist = [param_name_type(p) for p in split_params(params)]
                cls = classify(fn, ret)
                wire = " + ".join([str(HEADER_BYTES)] + [f"({wire_expr(fn, n, t)})" for n, t in plist])
                reply = reply_expr(fn, ret, cls)
                idx = len(entries)
                entries.append((fn, cls))
                out.append(f"   MTL_TALLY_SCOPE({idx}, {wire}, {reply});")
                touched = True
                i = j + 1
                continue
            out.append(line)
            i += 1
        if touched:
            text = "\n".join(out)
            first_inc = text.index("#include")
            text = text[:first_inc] + '#include "mtl_tally.h"\n' + text[first_inc:]
            m_file.write_text(text)

    missing = declared - {e[0] for e in entries}
    print(f"instrumented {len(entries)} functions; declared but not found as definitions "
          f"({len(missing)}): {' '.join(sorted(missing))}")

    names = ",\n".join(f'   {{"{fn}", MTL_TALLY_{cls.upper()}}}' for fn, cls in entries)
    tmpl = pathlib.Path(__file__).with_name("mtl_tally.c.in").read_text()
    (bridge / "mtl_tally.c").write_text(tmpl.replace("@COUNT@", str(len(entries)))
                                            .replace("@ENTRIES@", names))
    (bridge / "mtl_tally.h").write_text(pathlib.Path(__file__).with_name("mtl_tally.h").read_text())

    mb = bridge / "meson.build"
    t = mb.read_text()
    t = t.replace("    'mtl_argument_table.m',", "    'mtl_tally.c',\n    'mtl_argument_table.m',", 1)
    mb.write_text(t)


if __name__ == "__main__":
    main()
