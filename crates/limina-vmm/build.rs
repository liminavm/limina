// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Emit the native link search path for virglrenderer.
//!
//! The vendored rutabaga bindings declare `#[link(name = "virglrenderer")]`, which puts
//! `-lvirglrenderer` on the link line — but rutabaga only runs its pkg-config probe (the
//! thing that emits the `-L` search path) under its own `gpu` feature, which our build
//! does not enable (we use `virgl_renderer`/`virgl_renderer_next`). So the *final* binary
//! link can't find the library. Probe it here, in the binary crate, to supply the search
//! path — the portable equivalent of hardcoding Homebrew's lib dir.
//!
//! ⚠️ COSTLY-MISTAKE GUARD (see CLAUDE.md): the worker MUST link our patched
//! `third_party/virgl-prefix` virglrenderer (venus + in-process render-server), NOT
//! Homebrew's stock one. If it links Homebrew's, `virgl_renderer_init` returns -1
//! ("Render server support was not enabled") and the GPU **silently degrades to
//! software-2D** — venus never enumerates in the guest, with no obvious error. A plain
//! `cargo build` used to pick up whichever virglrenderer.pc pkg-config found first
//! (Homebrew's). We now force our prefix to the FRONT of PKG_CONFIG_PATH here so the
//! probe — and therefore the final link's `-L` / install_name — always resolves to ours,
//! no matter how the build is invoked.
use std::path::Path;

fn main() {
    // Prepend our virgl-prefix pkgconfig dir so OUR virglrenderer.pc wins. An explicitly
    // exported PKG_CONFIG_PATH (e.g. from a build script) still takes precedence after it.
    let prefix_pc =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/virgl-prefix/lib/pkgconfig");
    if prefix_pc.is_dir() {
        let existing = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
        let combined = if existing.is_empty() {
            prefix_pc.display().to_string()
        } else {
            format!("{}:{}", prefix_pc.display(), existing)
        };
        std::env::set_var("PKG_CONFIG_PATH", combined);
    } else {
        println!(
            "cargo:warning=third_party/virgl-prefix not found ({}); the worker will link \
             whatever virglrenderer pkg-config finds (likely Homebrew's, which LACKS venus \
             render-server → GPU silently degrades to software-2D). Run \
             scripts/build-virglrenderer.sh.",
            prefix_pc.display()
        );
    }

    match pkg_config::Config::new().probe("virglrenderer") {
        Ok(lib) => {
            // Surface which virglrenderer we resolved so a wrong link is visible at build
            // time, not discovered hours later as "venus won't enumerate".
            let where_ = lib
                .link_paths
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unknown>".into());
            if where_.contains("virgl-prefix") {
                println!("cargo:warning=virglrenderer: linking OUR prefix ({where_})");
            } else {
                println!(
                    "cargo:warning=virglrenderer: linking {where_} — NOT third_party/virgl-prefix! \
                     venus will degrade to software-2D. Rebuild with \
                     PKG_CONFIG_PATH=third_party/virgl-prefix/lib/pkgconfig:... or run \
                     scripts/build-virglrenderer.sh first."
                );
            }
        }
        Err(e) => println!("cargo:warning=virglrenderer pkg-config probe failed: {e}"),
    }
    // epoxy is a transitive dep of virglrenderer; probe it too so its search path is
    // present if the linker ever needs it directly.
    let _ = pkg_config::Config::new().probe("epoxy");
}
