// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
// Copyright © 2026 Marcelo Jorge Vieira

//! Compile the Secure-Enclave Swift shim (`swift/fido_sep.swift`) into a dylib the
//! supervisor links, for the M14 virtual FIDO authenticator. CryptoKit has no C
//! surface, so the SEP key create/sign/pubkey path (Spike A) must be Swift; this
//! bridges it to the Rust CTAP2 core over a plain C ABI.
//!
//! Dev/test link only bakes an rpath to OUT_DIR; the app bundle copies the dylib
//! into `Contents/Frameworks` and fixes the rpath in `scripts/build-app.sh`.

use std::path::{Path, PathBuf};
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

/// The dependency trees THIS build carries, and where each one lives (relative to the
/// repository root, or absolute).
///
/// Scoped to the host stack: libkrun, virglrs and imago are compiled into the binaries,
/// KosmicKrisp ships as dylibs inside the `.app` and edk2 as its firmware. The guest's own
/// kernel and mesa are whatever was last delivered into a guest image — no part of this
/// build — so listing them here would describe a stack that is not the one running.
///
/// `None` means there is no tree on this host to read: the edk2 firmware is built from the
/// manifest pin inside a container (`scripts/build-krun-efi.sh` clones that rev itself), so
/// for it the pin IS what was built, and a local checkout — kept only for fork surgery —
/// would be the less truthful answer.
const DEPS: &[(&str, Option<&str>)] = &[
    ("libkrun", Some("third_party/libkrun")),
    ("virglrs", Some("third_party/virglrs")),
    ("imago", Some("third_party/imago")),
    // Not under third_party/: Mesa needs a case-sensitive filesystem, so this tree lives on
    // the sparse image `scripts/ensure-mesa-cs.sh` mounts.
    ("kosmickrisp", Some("/Volumes/mesa-cs/mesa")),
    ("edk2", None),
];

/// Bake the build date, our source revision and the dependency revisions in for the About
/// menu (`src/about.rs`).
///
/// The date honors `LIMINA_BUILD_STAMP` — `scripts/build-app.sh` sets it to the release
/// build's own timestamp, so a shipped bundle carries the date it was actually cut rather
/// than whenever this script last happened to run.
fn build_stamp() {
    println!("cargo:rerun-if-env-changed=LIMINA_BUILD_STAMP");
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    watch_git(&root);

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

    // No `-dirty` suffix. Cargo has no trigger that reliably catches an edit to an arbitrary
    // tracked file, so the flag would be wrong in both directions — absent after editing a
    // clean build, and left over after committing — and a provenance field that lies in both
    // directions is worse than no field: it is read as evidence. `scripts/park-bundle.sh`
    // records dirtiness at the moment a bundle is cut, where it can be observed for real.
    let rev = git(&root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=LIMINA_GIT_REV={rev}");

    println!("cargo:rustc-env=LIMINA_DEPS={}", dep_stamp(&root));
}

/// Resolve every dependency in [`DEPS`] to the revision this build carries.
///
/// THE PIN IS A CLAIM; THE CHECKOUT HEAD IS THE FACT — the same rule
/// `scripts/park-bundle.sh` is built on. `third_party/manifest.toml` says what a tree
/// *should* be on; what got compiled in is whatever its `HEAD` was, and the two have
/// disagreed repeatedly in practice (a checkout a commit ahead of its pin, one moved onto a
/// local branch mid-session). So each tree is asked directly, and only a dependency with no
/// tree to ask falls back to the pin — marked `pin`, so the dialog can say which rows were
/// observed and which were merely claimed.
///
/// One record per dependency, `;`-separated, fields `|`-separated: name, revision, branch,
/// and `head` or `pin`. Parsed back by `about::deps`.
fn dep_stamp(root: &Path) -> String {
    let manifest_path = root.join("third_party/manifest.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest: toml::Table = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or_default();

    let mut records = Vec::new();
    for (name, tree) in DEPS {
        let pinned = manifest.get(*name).and_then(|entry| entry.as_table());
        let pin = |key: &str| {
            pinned
                .and_then(|entry| entry.get(key))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };

        let dir = tree.map(|tree| {
            let tree = Path::new(tree);
            if tree.is_absolute() {
                tree.to_path_buf()
            } else {
                root.join(tree)
            }
        });
        // A tree that is not vendored here (or sits on an unmounted volume) simply has no
        // HEAD to read; git tells us so by failing, and the pin is then all we have.
        let head = dir.as_deref().and_then(|dir| {
            let rev = git(dir, &["rev-parse", "HEAD"])?;
            watch_git(dir);
            let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
            Some((rev, branch))
        });

        let (rev, branch, source) = match head {
            Some((rev, branch)) => (rev, branch.or_else(|| pin("branch")), "head"),
            None => match pin("rev") {
                Some(rev) => (rev, pin("branch"), "pin"),
                // Neither a checkout nor a pin: the manifest was reshaped out from under this
                // list. Say nothing rather than show a dependency with no revision.
                None => continue,
            },
        };
        records.push(format!(
            "{}|{}|{}|{source}",
            field(name),
            field(&rev),
            field(branch.as_deref().unwrap_or_default()),
        ));
    }
    records.join(";")
}

/// One field of a stamp record. Hashes and our branch names carry neither separator, but git
/// does permit both in a ref name, and a branch that split a record in two would garble every
/// row after it.
fn field(text: &str) -> String {
    text.replace(['|', ';'], "-")
}

/// Rerun this script whenever `dir`'s HEAD moves.
///
/// `.git/HEAD` alone is NOT enough, and that is the whole trap: on a branch it holds the
/// text `ref: refs/heads/<branch>`, which a commit does not touch — so an incremental build
/// after committing kept baking in the previous revision. What moves is the ref file itself,
/// or `packed-refs` when the ref is packed and no loose file exists yet. Watching the whole
/// `refs` directory (cargo walks a directory it is given) covers both a ref being updated and
/// a loose one appearing; `HEAD` still matters for a branch switch.
///
/// Only paths that exist are declared: cargo treats a missing one as reason to rerun, which
/// would rebuild the crate on every single `cargo build`.
fn watch_git(dir: &Path) {
    for name in ["HEAD", "refs", "packed-refs"] {
        let Some(path) = git(dir, &["rev-parse", "--git-path", name]) else {
            continue;
        };
        // `--git-path` answers relative to the repository it was asked in.
        let path = dir.join(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Run git in `dir`; `None` if git is missing, the directory is not a checkout, or the
/// command failed (a source tarball with no repo still builds — About just falls back to the
/// manifest pin, or says "unknown" for our own revision).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
