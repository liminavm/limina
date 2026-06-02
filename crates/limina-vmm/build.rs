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
fn main() {
    if let Err(e) = pkg_config::Config::new().probe("virglrenderer") {
        println!("cargo:warning=virglrenderer pkg-config probe failed: {e}");
    }
    // epoxy is a transitive dep of virglrenderer; probe it too so its search path is
    // present if the linker ever needs it directly.
    let _ = pkg_config::Config::new().probe("epoxy");
}
