//! The C rendering of the protocol, `include/limina_gpu_wire.h`, generated from the Rust
//! definitions so the two cannot drift. `LIMINA_WIRE_BLESS=1 cargo test -p limina-gpu-wire`
//! rewrites it; otherwise the test fails when it is stale.

extern crate std;

use std::fmt::Write as _;
use std::string::String;

use crate::cmd::{self, C_EXTRA_OPS, C_STRUCTS, barrier_kind, render_state, sync_kind};
use crate::ring::{self, RingControl};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/include/limina_gpu_wire.h");

fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The C declaration of a field of type `ty` named `name`.
fn c_decl(ty: &str, name: &str) -> String {
    let ty = ty.replace(' ', "");
    if let Some(inner) = ty.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        let (elem, n) = inner.split_once(';').expect("array type");
        return c_decl(elem, &std::format!("{name}[{n}]"));
    }
    let base = match ty.as_str() {
        "u8" => "uint8_t".into(),
        "u16" => "uint16_t".into(),
        "u32" => "uint32_t".into(),
        "i32" => "int32_t".into(),
        "u64" => "uint64_t".into(),
        "i64" => "int64_t".into(),
        "f32" => "float".into(),
        "f64" => "double".into(),
        other => std::format!("struct lmn_{}", snake(other)),
    };
    std::format!("{base} {name}")
}

fn generate() -> String {
    let mut h = String::new();
    let w = &mut h;
    let _ = writeln!(
        w,
        "/* Generated from crates/limina-gpu-wire by its c_header test; do not edit.\n \
         * SPDX-License-Identifier: MIT\n \
         *\n \
         * The guest<->replayer protocol of limina's Metal-level GPU transport. The Rust crate is\n \
         * the definition; its doc comments are the documentation. */\n\
         #ifndef LIMINA_GPU_WIRE_H\n#define LIMINA_GPU_WIRE_H\n\n\
         #include <stddef.h>\n#include <stdint.h>\n"
    );

    let _ = writeln!(w, "#define LMN_WIRE_VERSION {}u\n", crate::VERSION);

    let _ = writeln!(w, "/* Ring (crates/limina-gpu-wire/src/ring.rs). */");
    let _ = writeln!(w, "#define LMN_RING_MAGIC 0x{:08x}u", ring::MAGIC);
    let _ = writeln!(w, "#define LMN_RING_DATA_OFFSET {}u", ring::DATA_OFFSET);
    let _ = writeln!(w, "#define LMN_RING_CMD_ALIGN {}u", ring::CMD_ALIGN);
    let _ = writeln!(w, "#define LMN_RING_MIN_DATA_SIZE {}u", ring::MIN_DATA_SIZE);
    let _ = writeln!(w, "#define LMN_RING_STATE_RUNNING {}u", ring::STATE_RUNNING);
    let _ = writeln!(w, "#define LMN_RING_STATE_PARKED {}u", ring::STATE_PARKED);
    for (name, off) in [
        ("MAGIC", core::mem::offset_of!(RingControl, magic)),
        ("VERSION", core::mem::offset_of!(RingControl, version)),
        ("TAIL", core::mem::offset_of!(RingControl, tail)),
        ("HEAD", core::mem::offset_of!(RingControl, head)),
        ("COMPLETED", core::mem::offset_of!(RingControl, completed)),
        ("STATE", core::mem::offset_of!(RingControl, state)),
        ("ERROR", core::mem::offset_of!(RingControl, error)),
        ("ERROR_AT", core::mem::offset_of!(RingControl, error_at)),
    ] {
        let _ = writeln!(w, "#define LMN_RING_OFF_{name} {off}u");
    }
    let _ = writeln!(w);

    let _ = writeln!(
        w,
        "/* ring::Error: why a context was lost, or why a reply failed. */"
    );
    for e in 1..=64 {
        if let Some(err) = ring::Error::from_u32(e) {
            let _ = writeln!(
                w,
                "#define LMN_ERROR_{} {e}u",
                snake(&std::format!("{err:?}")).to_uppercase()
            );
        }
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "#define LMN_ARENA_PAGE {}u", cmd::ARENA_PAGE);
    let _ = writeln!(
        w,
        "#define LMN_MAX_COLOR_ATTACHMENTS {}u",
        cmd::MAX_COLOR_ATTACHMENTS
    );
    let _ = writeln!(
        w,
        "#define LMN_MAX_SAMPLE_POSITIONS {}u",
        cmd::MAX_SAMPLE_POSITIONS
    );
    let _ = writeln!(w, "#define LMN_REPLY_DONE {}u", cmd::REPLY_DONE);
    for (name, v) in [
        ("SYNC_FENCE", sync_kind::FENCE),
        ("SYNC_EVENT", sync_kind::EVENT),
        ("SYNC_SHARED_EVENT", sync_kind::SHARED_EVENT),
        ("BARRIER_AFTER_STAGES", barrier_kind::AFTER_STAGES),
        (
            "BARRIER_AFTER_ENCODER_STAGES",
            barrier_kind::AFTER_ENCODER_STAGES,
        ),
        (
            "BARRIER_AFTER_QUEUE_STAGES",
            barrier_kind::AFTER_QUEUE_STAGES,
        ),
        ("RS_STENCIL_REFERENCE", render_state::STENCIL_REFERENCE),
        ("RS_WINDING", render_state::WINDING),
        ("RS_CULL_MODE", render_state::CULL_MODE),
        ("RS_DEPTH_CLIP_MODE", render_state::DEPTH_CLIP_MODE),
        ("RS_DEPTH_BIAS", render_state::DEPTH_BIAS),
        ("RS_DEPTH_TEST_BOUNDS", render_state::DEPTH_TEST_BOUNDS),
        ("RS_VISIBILITY_RESULT", render_state::VISIBILITY_RESULT),
        ("RS_COLOR_STORE_ACTION", render_state::COLOR_STORE_ACTION),
        ("RS_DEPTH_STORE_ACTION", render_state::DEPTH_STORE_ACTION),
        (
            "RS_STENCIL_STORE_ACTION",
            render_state::STENCIL_STORE_ACTION,
        ),
    ] {
        let _ = writeln!(w, "#define LMN_{name} {v}u");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "/* Opcodes. */");
    for (name, v) in C_EXTRA_OPS {
        let _ = writeln!(w, "#define LMN_OP_{name} 0x{v:04x}u");
    }
    for s in C_STRUCTS {
        if let Some((name, v)) = s.op {
            let _ = writeln!(w, "#define LMN_OP_{name} 0x{v:04x}u");
        }
    }
    let _ = writeln!(w);

    let _ = writeln!(
        w,
        "struct lmn_cmd_header {{\n   uint16_t op;\n   uint16_t flags;\n   uint32_t len;\n}};\n\
         _Static_assert(sizeof(struct lmn_cmd_header) == 8, \"lmn_cmd_header\");\n"
    );

    for s in C_STRUCTS {
        let cname = std::format!("lmn_{}", snake(s.name));
        if s.fields.is_empty() {
            // C has no empty structs; the command's payload is empty.
            let _ = writeln!(w, "/* {cname}: no payload. */\n");
            continue;
        }
        for line in s.doc.lines() {
            let _ = writeln!(w, "/*{line} */");
        }
        let _ = writeln!(w, "struct {cname} {{");
        for f in s.fields {
            let _ = writeln!(w, "   {};", c_decl(f.ty, f.name));
        }
        let _ = writeln!(w, "}};");
        let _ = writeln!(
            w,
            "_Static_assert(sizeof(struct {cname}) == {}, \"{cname} size\");",
            s.size
        );
        for f in s.fields {
            let _ = writeln!(
                w,
                "_Static_assert(offsetof(struct {cname}, {}) == {}, \"{cname}.{}\");",
                f.name, f.offset, f.name
            );
        }
        let _ = writeln!(w);
    }

    let _ = writeln!(w, "#endif /* LIMINA_GPU_WIRE_H */");
    h
}

#[test]
fn c_header_is_current() {
    let want = generate();
    if std::env::var_os("LIMINA_WIRE_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(PATH).parent().unwrap()).unwrap();
        std::fs::write(PATH, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(PATH).unwrap_or_default();
    assert!(
        have == want,
        "{PATH} is stale; regenerate it with LIMINA_WIRE_BLESS=1 cargo test -p limina-gpu-wire"
    );
}

/// The C compiler agrees with Rust on every size and offset (the header's static asserts).
#[test]
fn c_header_compiles() {
    let out = std::process::Command::new("cc")
        .args([
            "-std=c11",
            "-Wall",
            "-Wpedantic",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c",
        ])
        .arg(PATH)
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
