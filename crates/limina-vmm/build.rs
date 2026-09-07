// SPDX-License-Identifier: MIT

//! The worker's build script.
//!
//! It used to exist to steer the link line: rutabaga's vendored bindings declared
//! `#[link(name = "virglrenderer")]`, so the *final* binary's `-L` had to be made to resolve to
//! our prefix rather than to whatever `virglrenderer.pc` pkg-config found first. There is no
//! dylib to steer any more -- the renderer is a Rust dependency of `rutabaga_gfx`, and it emits
//! its own link directives for the two libraries it does link (Mesa's EGL and the Vulkan loader).
//! `LIMINA_VIRGL_PREFIX` and `check-virgl-link.sh` had the same reason to exist and go with it.

use std::path::Path;

fn main() {
    // macOS: embed our Info.plist into the worker Mach-O (__TEXT,__info_plist). The worker is the
    // process that opens CoreAudio for `--mic`, but an app bundle's Info.plist only covers its
    // MAIN executable (limina) — not this helper. Without the microphone usage string in the
    // worker's OWN Info.plist, macOS TCC cannot present a prompt and silently denies capture. The
    // section is present before build-app.sh codesigns the binary, so it is covered by the
    // signature. See crates/limina-vmm/Info.plist and the audio memo.
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
