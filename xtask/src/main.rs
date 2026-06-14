// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina dev tasks. Run via `cargo xtask <command>`.
//!
//! Currently: `bundle` — assemble a minimal, codesigned `Limina.app` and (optionally) launch
//! it through LaunchServices. This validates the *normal* launch path early: an app started
//! via `open`/double-click runs under launchd with a real GUI/GPU session, rather than
//! inheriting a terminal's (or sshd's) context — which is where the worker's virtio-gpu
//! init behaves differently. With `--open` and `LIMINA_WINDOW_CAPTURE` baked into the bundle,
//! a capture PNG appearing means the worker did *not* hang and the layer rendered.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

/// In-bundle path the supervisor writes its rendered-layer PNG to (LSEnvironment), so a
/// normal launch is self-verifiable without screen-recording permission.
const CAPTURE_PATH: &str = "/tmp/limina-app-capture.png";

#[derive(Parser)]
#[command(name = "xtask", about = "limina dev tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build + assemble + codesign `target/Limina.app`.
    Bundle {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
        /// Launch the bundle via LaunchServices (`open`) booting the L1 `limina.hold` guest.
        #[arg(long)]
        open: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Bundle { release, open } => bundle(release, open),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .to_path_buf()
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawning {cmd:?}"))?;
    if !status.success() {
        bail!("command failed ({status}): {cmd:?}");
    }
    Ok(())
}

fn bundle(release: bool, open: bool) -> Result<()> {
    let repo = repo_root();
    let profile = if release { "release" } else { "debug" };
    let target = repo.join("target");
    let profile_dir = target.join(profile);

    // 1. Build the supervisor + worker.
    eprintln!("==> building limina + limina-vmm ({profile})");
    let mut c = Command::new("cargo");
    c.current_dir(&repo)
        .args(["build", "-p", "limina", "-p", "limina-vmm"]);
    if release {
        c.arg("--release");
    }
    run(&mut c)?;

    // 2. Ensure the L1 test guest exists (build it if not).
    let kernel = target.join("test-guest/Image");
    let rootfs = target.join("test-guest/rootfs");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("==> building the L1 test guest");
        run(Command::new("bash")
            .current_dir(&repo)
            .arg("scripts/build-test-guest.sh"))?;
    }

    // 3. Assemble Limina.app/Contents/{MacOS,Info.plist}.
    let app = target.join("Limina.app");
    let macos = app.join("Contents/MacOS");
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("rm {app:?}"))?;
    }
    std::fs::create_dir_all(&macos).with_context(|| format!("mkdir {macos:?}"))?;
    for bin in ["limina", "limina-vmm"] {
        std::fs::copy(profile_dir.join(bin), macos.join(bin))
            .with_context(|| format!("copy {bin} into the bundle"))?;
    }
    std::fs::write(app.join("Contents/Info.plist"), info_plist()).context("writing Info.plist")?;
    eprintln!("==> assembled {}", app.display());

    // 4. Codesign: worker keeps the hypervisor entitlement; then ad-hoc the main exe and
    //    seal the bundle (no --deep, so the worker's entitled signature is preserved).
    let ents = repo.join("crates/limina-vmm/hvf-entitlements.plist");
    run(Command::new("codesign")
        .args(["--entitlements"])
        .arg(&ents)
        .args(["-s", "-", "--force"])
        .arg(macos.join("limina-vmm")))?;
    run(Command::new("codesign")
        .args(["-s", "-", "--force"])
        .arg(macos.join("limina")))?;
    run(Command::new("codesign")
        .args(["-s", "-", "--force"])
        .arg(&app))?;
    eprintln!("==> codesigned (worker: com.apple.security.hypervisor)");

    if open {
        eprintln!("==> launching via LaunchServices (open); capture -> {CAPTURE_PATH}");
        let _ = std::fs::remove_file(CAPTURE_PATH);
        run(Command::new("open")
            .arg(&app)
            .args(["--args", "--window", "--kernel"])
            .arg(&kernel)
            .arg("--rootfs")
            .arg(&rootfs)
            .args([
                "--cmdline",
                "console=ttyAMA0 rootfstype=virtiofs rw init=/init limina.hold",
            ]))?;
        eprintln!(
            "    launched. Watch dev-mac's screen; check {CAPTURE_PATH} for the rendered layer."
        );
    } else {
        eprintln!("==> done: {}", app.display());
        eprintln!(
            "    launch with: open {} --args --window --kernel ... --rootfs ... --cmdline ...",
            app.display()
        );
    }
    Ok(())
}

/// Minimal app Info.plist. `LSEnvironment` injects the capture path on a LaunchServices
/// launch (it doesn't apply when the binary is run directly), so a normal launch is
/// self-verifiable. The supervisor sets its own NSApplication activation policy in code.
fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>limina</string>
    <key>CFBundleDisplayName</key><string>limina</string>
    <key>CFBundleIdentifier</key><string>br.dev.kov.limina</string>
    <key>CFBundleVersion</key><string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>limina</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSEnvironment</key>
    <dict>
        <key>LIMINA_WINDOW_CAPTURE</key><string>{CAPTURE_PATH}</string>
        <key>RUST_LOG</key><string>info</string>
    </dict>
</dict>
</plist>
"#
    )
}
