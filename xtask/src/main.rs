// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina dev tasks. Run via `cargo xtask <command>`.
//!
//! One obvious command per task, each shelling out to the tested `scripts/` (which stay the
//! source of truth) so there's a single discoverable surface instead of a spread of scripts you
//! have to know by name. `cargo xtask --help` lists them.
//!
//! Bootstrap / build loop:
//!   `setup`  — one-command fresh-clone bootstrap: `vendor` + enable the git hooks.
//!   `vendor` — materialize the gitignored `third_party/` source trees (libkrun + virglrenderer
//!              checkouts + the patched imago) from the committed patch series. Run it first.
//!   `build`  — build `limina` + `limina-vmm`, verify the worker links our virglrenderer (the
//!              venus link trap), and codesign the worker (hypervisor entitlement). The inner-loop
//!              "make a runnable worker" step.
//!   `sign`   — codesign an already-built worker (just the hypervisor-entitlement step).
//!   `test`   — build + sign + link-check + run the HVF-gated boot tests (wraps
//!              `scripts/test-boot.sh`). The canonical "did I break boot" command.
//!
//! Run / package:
//!   `run`    — boot an enhanced-tier image to the seated venus desktop in a window (EFI+venus, the
//!              documented default boot; wraps `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`).
//!   `app`    — assemble the full self-contained `target/Limina.app` (the shipping bundle with the
//!              whole host venus/GL closure; wraps `scripts/build-app.sh`).
//!   `bundle` — assemble a *minimal* codesigned `target/Limina.app` and (optionally) launch it
//!              through LaunchServices. This validates the *normal* launch path early: an app
//!              started via `open`/double-click runs under launchd with a real GUI/GPU session,
//!              rather than inheriting a terminal's (or sshd's) context — which is where the
//!              worker's virtio-gpu init behaves differently. With `--open` and
//!              `LIMINA_WINDOW_CAPTURE` baked into the bundle, a capture PNG appearing means the
//!              worker did *not* hang and the layer rendered. (Distinct from `app`: `bundle` is a
//!              launch-path smoke test booting the L1 guest, `app` is the real deliverable.)

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
    /// Fresh-clone bootstrap: `vendor` (materialize `third_party/`) + enable the git hooks.
    Setup,
    /// Materialize the gitignored `third_party/` source trees by applying the committed patch
    /// series. Run once after a fresh clone (or a libkrun re-clone) before building.
    Vendor,
    /// Build `limina` + `limina-vmm`, verify the virgl link, and codesign the worker.
    Build {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
    },
    /// Codesign the already-built worker with the hypervisor entitlement (no build).
    Sign {
        /// Sign the release-profile worker.
        #[arg(long)]
        release: bool,
    },
    /// Build + sign + link-check, then run the HVF-gated boot tests (wraps scripts/test-boot.sh).
    Test {
        /// Test in release mode.
        #[arg(long)]
        release: bool,
        /// Extra args passed through to the test run (e.g. a `--test <name>` filter or a
        /// `testname` substring after `--`). Everything here is forwarded verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Boot an enhanced-tier image to the seated venus desktop in a window (EFI+venus default).
    Run {
        /// The enhanced-tier `.raw` disk to boot (required). Booted in place — clone first if you
        /// want to keep it pristine.
        #[arg(long)]
        disk: PathBuf,
        /// Boot without user-mode NAT networking (default: `--net` on).
        #[arg(long)]
        no_net: bool,
        /// vCPU count (default: the boot script's 6).
        #[arg(long)]
        cpus: Option<u32>,
        /// Guest RAM in MiB (default: the boot script's 8192).
        #[arg(long)]
        ram_mib: Option<u32>,
        /// Extra flags forwarded to `limina` (e.g. `--swap-cmd-opt`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
    /// Assemble the full self-contained `target/Limina.app` (wraps scripts/build-app.sh).
    App {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
    },
    /// Build + assemble + codesign a minimal `target/Limina.app` (launch-path smoke test).
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
        Cmd::Setup => setup(),
        Cmd::Vendor => vendor(),
        Cmd::Build { release } => build(release),
        Cmd::Sign { release } => sign_worker(&repo_root(), release),
        Cmd::Test { release, args } => test(release, &args),
        Cmd::Run {
            disk,
            no_net,
            cpus,
            ram_mib,
            extra,
        } => run_vm(disk, no_net, cpus, ram_mib, &extra),
        Cmd::App { release } => app(release),
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

/// Run one of the repo's `bash` scripts from the repo root, forwarding `args`.
fn bash_script(repo: &Path, script: &str, args: &[impl AsRef<std::ffi::OsStr>]) -> Result<()> {
    let mut c = Command::new("bash");
    c.current_dir(repo).arg(repo.join(script));
    for a in args {
        c.arg(a);
    }
    run(&mut c)
}

fn profile_name(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

// --- bootstrap ---------------------------------------------------------------------------------

/// libkrun upstream — cloned into `third_party/libkrun` when absent; the apply script then resets
/// it to `patches/libkrun/UPSTREAM_BASE` and applies our series.
const LIBKRUN_GIT: &str = "https://github.com/containers/libkrun.git";

/// virglrenderer upstream — cloned into `third_party/virglrenderer` when absent; the apply script
/// then resets it to `patches/virglrenderer/UPSTREAM_BASE` and applies our series. Built separately
/// into `third_party/virgl-prefix` by `scripts/build-virglrenderer.sh` (the worker links it).
const VIRGL_GIT: &str = "https://gitlab.freedesktop.org/virgl/virglrenderer.git";

/// One-command fresh-clone bootstrap: vendor `third_party/`, then point git at the in-repo hooks.
fn setup() -> Result<()> {
    vendor()?;
    eprintln!("==> enabling git hooks (core.hooksPath = .githooks)");
    bash_script(&repo_root(), "scripts/setup-hooks.sh", &[] as &[&str])?;
    eprintln!("==> setup complete — `cargo xtask build` / `cargo xtask test` are ready");
    Ok(())
}

/// Apply every patch series onto its vendored source tree, so the workspace can build.
///
/// We consume libkrun's internal crates and override `imago` by path, and link our patched
/// virglrenderer — all under the gitignored `third_party/`. A fresh clone has none of these trees;
/// this recreates them from the committed patch series (`patches/libkrun`, `patches/virglrenderer`,
/// `patches/imago`) via the per-dependency apply scripts. Idempotent — re-running just resets each
/// tree to its base and re-applies. (The imago step is self-sufficient: it downloads the pristine
/// crate if the cargo cache is empty, since the `[patch.crates-io]` override would otherwise block
/// `cargo fetch`.)
fn vendor() -> Result<()> {
    let repo = repo_root();

    // libkrun: a from-source git checkout (path deps in [workspace.dependencies]). Clone if absent.
    let libkrun = repo.join("third_party/libkrun");
    if !libkrun.join(".git").exists() {
        eprintln!("==> cloning libkrun ({LIBKRUN_GIT}) — third_party/libkrun is absent");
        run(Command::new("git").current_dir(&repo).args([
            "clone",
            LIBKRUN_GIT,
            "third_party/libkrun",
        ]))?;
    }
    eprintln!("==> applying the libkrun patch series");
    bash_script(&repo, "scripts/apply-libkrun-patches.sh", &[] as &[&str])?;

    // virglrenderer: a from-source git checkout built into third_party/virgl-prefix (the worker
    // links it — see the limina-virgl-link-trap memory). Clone if absent, then apply our series.
    let virgl = repo.join("third_party/virglrenderer");
    if !virgl.join(".git").exists() {
        eprintln!("==> cloning virglrenderer ({VIRGL_GIT}) — third_party/virglrenderer is absent");
        run(Command::new("git").current_dir(&repo).args([
            "clone",
            VIRGL_GIT,
            "third_party/virglrenderer",
        ]))?;
    }
    eprintln!("==> applying the virglrenderer patch series");
    bash_script(&repo, "scripts/apply-virgl-patches.sh", &[] as &[&str])?;

    // imago: vendored from crates.io + our discard/vm-memory patches ([patch.crates-io] override).
    eprintln!("==> vendoring + patching imago");
    bash_script(&repo, "scripts/apply-imago-patch.sh", &[] as &[&str])?;

    eprintln!("==> vendor complete — `cargo xtask build` / `cargo xtask test` are ready");
    Ok(())
}

// --- build loop --------------------------------------------------------------------------------

fn cargo_build_binaries(repo: &Path, release: bool) -> Result<()> {
    eprintln!(
        "==> building limina + limina-vmm ({})",
        profile_name(release)
    );
    let mut c = Command::new("cargo");
    c.current_dir(repo)
        .args(["build", "-p", "limina", "-p", "limina-vmm"]);
    if release {
        c.arg("--release");
    }
    run(&mut c)
}

/// Codesign the worker with the hypervisor entitlement (required for `hv_vm_*`). Wraps the
/// canonical `crates/limina-vmm/sign.sh` so the entitlement plist stays in one place.
fn sign_worker(repo: &Path, release: bool) -> Result<()> {
    eprintln!("==> codesigning the worker (hypervisor entitlement)");
    bash_script(repo, "crates/limina-vmm/sign.sh", &[profile_name(release)])
}

/// Verify the worker links our `third_party/virgl-prefix` virglrenderer, not Homebrew's — the
/// costly silent venus link trap (see the limina-virgl-link-trap memory).
fn check_virgl_link(repo: &Path, release: bool) -> Result<()> {
    eprintln!("==> checking the worker links our virglrenderer (venus guard)");
    let worker = format!("target/{}/limina-vmm", profile_name(release));
    bash_script(repo, "scripts/check-virgl-link.sh", &[worker])
}

/// The inner-loop "make a runnable worker": build both binaries, guard the virgl link, sign the
/// worker. Everything a `cargo xtask run` / a manual boot needs, minus the tests.
fn build(release: bool) -> Result<()> {
    let repo = repo_root();
    cargo_build_binaries(&repo, release)?;
    sign_worker(&repo, release)?;
    check_virgl_link(&repo, release)?;
    eprintln!(
        "==> build complete: target/{}/{{limina,limina-vmm}} (worker signed, virgl link OK)",
        profile_name(release)
    );
    Ok(())
}

/// The canonical "did I break boot" command: build + codesign worker + link-check + build the L1
/// guest + the trap probe + run the HVF-gated boot tests. All of it lives in test-boot.sh; we just
/// forward the profile and any extra filter args.
fn test(release: bool, args: &[String]) -> Result<()> {
    let repo = repo_root();
    let mut script_args: Vec<String> = vec![profile_name(release).to_string()];
    script_args.extend(args.iter().cloned());
    bash_script(&repo, "scripts/test-boot.sh", &script_args)
}

// --- run / package -----------------------------------------------------------------------------

/// Boot an enhanced-tier image to the seated venus desktop in a window. Builds + signs a debug
/// worker, ensures the case-sensitive Mesa volume is mounted (the host KK/zink builds live there),
/// then hands off to the blessed default boot script, which owns all the KK/zink env.
fn run_vm(
    disk: PathBuf,
    no_net: bool,
    cpus: Option<u32>,
    ram_mib: Option<u32>,
    extra: &[String],
) -> Result<()> {
    let repo = repo_root();

    // The boot script runs `target/debug/limina{,-vmm}` directly (debug only) and needs the worker
    // signed for hv_vm_*, so make a runnable debug worker first.
    build(false)?;

    // The default boot uses KosmicKrisp from /Volumes/mesa-cs; attach it if macOS dropped the mount.
    eprintln!("==> ensuring the case-sensitive Mesa volume is mounted");
    bash_script(&repo, "scripts/ensure-mesa-cs.sh", &[] as &[&str])?;

    // Resolve the disk against the caller's cwd (the boot script cds to the repo root, so a bare
    // relative path would otherwise resolve there). canonicalize also surfaces a missing image now.
    let disk = std::fs::canonicalize(&disk)
        .with_context(|| format!("disk image not found: {}", disk.display()))?;

    eprintln!("==> booting {} (EFI+venus, windowed)", disk.display());
    let mut c = Command::new("bash");
    c.current_dir(&repo)
        .arg(repo.join("spikes/venus-draw-probe/boot-enhanced-efi-kk.sh"))
        .env("LIMINA_DISK", &disk);
    if no_net {
        c.env("LIMINA_NET", "0");
    }
    if let Some(cpus) = cpus {
        c.env("LIMINA_CPUS", cpus.to_string());
    }
    if let Some(ram) = ram_mib {
        c.env("LIMINA_RAM_MIB", ram.to_string());
    }
    if !extra.is_empty() {
        c.env("LIMINA_EXTRA_ARGS", extra.join(" "));
    }
    run(&mut c)
}

/// Assemble the full self-contained `Limina.app` — the shipping deliverable, with the whole host
/// venus/GL dylib closure vendored into `Contents/Frameworks`. All of it lives in build-app.sh.
fn app(release: bool) -> Result<()> {
    bash_script(
        &repo_root(),
        "scripts/build-app.sh",
        &[profile_name(release)],
    )
}

fn bundle(release: bool, open: bool) -> Result<()> {
    let repo = repo_root();
    let profile = profile_name(release);
    let target = repo.join("target");
    let profile_dir = target.join(profile);

    // 1. Build the supervisor + worker.
    cargo_build_binaries(&repo, release)?;

    // 2. Ensure the L1 test guest exists (build it if not).
    let kernel = target.join("test-guest/Image");
    let rootfs = target.join("test-guest/rootfs");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("==> building the L1 test guest");
        bash_script(&repo, "scripts/build-test-guest.sh", &[] as &[&str])?;
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
