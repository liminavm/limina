// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Compile the Secure-Enclave Swift shim (`swift/fido_sep.swift`) into a dylib the
//! supervisor links, for the M14 virtual FIDO authenticator. CryptoKit has no C
//! surface, so the SEP key create/sign/pubkey path (Spike A) must be Swift; this
//! bridges it to the Rust CTAP2 core over a plain C ABI.
//!
//! Dev/test link only bakes an rpath to OUT_DIR; the app bundle copies the dylib
//! into `Contents/Frameworks` and fixes the rpath in `scripts/build-app.sh`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let src = "swift/fido_sep.swift";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=build.rs");

    build_stamp();

    // macOS-only; on any other host leave the dylib absent and let the Rust side
    // compile the FFI declarations without linking (the whole app is macOS, so this
    // branch only keeps `cargo check` honest on foreign CI).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dylib = out_dir.join("liblimina_sep.dylib");

    let status = Command::new("swiftc")
        .args([
            "-emit-library",
            "-O",
            "-module-name",
            "limina_sep",
            // Match CryptoKit SecureEnclave availability (macOS 10.15+); 12 is safe.
            "-target",
            "arm64-apple-macos12",
            "-framework",
            "CryptoKit",
            "-framework",
            "LocalAuthentication",
            "-framework",
            "Security",
            "-framework",
            "Foundation",
            src,
            "-o",
        ])
        .arg(&dylib)
        .args([
            "-Xlinker",
            "-install_name",
            "-Xlinker",
            "@rpath/liblimina_sep.dylib",
        ])
        .status()
        .expect("running swiftc for the SEP shim");
    assert!(status.success(), "swiftc failed building {src}");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=dylib=limina_sep");
    // Frameworks the shim pulls in (belt-and-suspenders — autolink usually covers it).
    println!("cargo:rustc-link-lib=framework=CryptoKit");
    println!("cargo:rustc-link-lib=framework=LocalAuthentication");
    // Find the dylib at runtime from the dev/test binary location.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", out_dir.display());
    // The Swift runtime dylibs (libswiftCore etc.) live here on macOS.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}

/// Bake the build date and the source revision in for the About menu (`src/about.rs`).
///
/// Reruns on every commit/checkout (`.git/HEAD` moves) and whenever `LIMINA_BUILD_STAMP`
/// changes — `scripts/build-app.sh` sets that to the release build's own timestamp, so a
/// shipped bundle always carries the date it was actually cut rather than whenever this
/// script last happened to run.
fn build_stamp() {
    println!("cargo:rerun-if-env-changed=LIMINA_BUILD_STAMP");
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }

    let date = std::env::var("LIMINA_BUILD_STAMP").ok().unwrap_or_else(|| {
        Command::new("date")
            .args(["-u", "+%Y-%m-%d %H:%M UTC"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into())
    });
    println!("cargo:rustc-env=LIMINA_BUILD_DATE={date}");

    // A dirty tree gets a `-dirty` suffix: the rev alone would name a build that does not
    // exist anywhere, and a bug report from one is worth knowing about.
    let rev = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(rev) if dirty() => format!("{rev}-dirty"),
        Some(rev) => rev,
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=LIMINA_GIT_REV={rev}");
}

/// Whether the tracked tree carries uncommitted changes.
fn dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty())
}

/// Run git in the crate's directory; `None` if git is missing or the command failed
/// (a source tarball with no repo still builds — the About menu just says "unknown").
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
