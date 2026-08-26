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
    //
    // Our virglrenderer is now built with the EGL platform (vrend GL via zink-on-KK), so its
    // virglrenderer.pc has `Requires.private: epoxy` and our epoxy.pc has `Requires.private: egl`.
    // pkg-config --cflags traverses those, so the worker build also needs our epoxy-with-EGL and
    // the zink-on-KK Mesa (which provides egl.pc) on PKG_CONFIG_PATH — else "could not generate
    // cflags for epoxy/egl". MESA_PREFIX overrides the zink-on-KK Mesa location (see
    // docs/drivers/kosmickrisp.rst); default matches build-mesa-zink-kk.sh.
    let tp = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party");
    // LIMINA_VIRGL_PREFIX picks a different virglrenderer install than the default, the way
    // MESA_PREFIX does for the zink-on-KK Mesa. Its reason to exist is sanitizer builds: a
    // TSan-instrumented virglrenderer has to live in its own prefix so the working one stays
    // usable, and the worker bakes the dylib's absolute path in at link time, so choosing it has
    // to happen here rather than at run time.
    let virgl_prefix = std::env::var("LIMINA_VIRGL_PREFIX")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| tp.join("virgl-prefix"));
    let prefix_pc = virgl_prefix.join("lib/pkgconfig");
    let mesa_prefix = std::env::var("MESA_PREFIX")
        .unwrap_or_else(|_| "/Volumes/mesa-cs/zink-kk-prefix".to_string());
    let extra_pc = [
        prefix_pc.clone(),
        tp.join("epoxy-egl-prefix/lib/pkgconfig"),
        Path::new(&mesa_prefix).join("lib/pkgconfig"),
    ];
    if prefix_pc.is_dir() {
        let mut parts: Vec<String> = extra_pc
            .iter()
            .filter(|p| p.is_dir())
            .map(|p| p.display().to_string())
            .collect();
        let existing = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
        if !existing.is_empty() {
            parts.push(existing);
        }
        std::env::set_var("PKG_CONFIG_PATH", parts.join(":"));
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

    // macOS: embed our Info.plist into the worker Mach-O (__TEXT,__info_plist). The worker
    // is the process that opens CoreAudio for `--mic`, but an app bundle's Info.plist only
    // covers its MAIN executable (limina) — not this helper. Without the microphone usage
    // string in the worker's OWN Info.plist, macOS TCC cannot present a prompt and silently
    // denies capture. The section is present before build-app.sh codesigns the binary, so
    // it is covered by the signature. See crates/limina-vmm/Info.plist and the audio memo.
    #[cfg(target_os = "macos")]
    {
        let plist = Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
