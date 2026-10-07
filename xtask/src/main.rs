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
//!   `vendor` — materialize the gitignored `third_party/` source trees: fork-model deps
//!              (libkrun, virglrs, imago, linux) clone from github.com/liminavm at the rev
//!              pinned in `third_party/manifest.toml` — the fork's `limina` branch IS the delta,
//!              nothing to apply. `third_party/manifest.local.toml` overrides where they come
//!              from (`src/manifest.rs`). It also creates `third_party/venv-mesa`, which is what
//!              the bare `python3` in virglrs's code generators has to resolve to.
//!              `heavy = true` deps (the kernel) are skipped unless `--heavy` — nothing on this
//!              host builds them. Run it first.
//!   `pins`   — every fork pin, the tree standing in for it, and whether its remote has it;
//!              then the pushes that would publish it (`src/pins.rs`). `--check` is the
//!              pre-push gate.
//!   `worktree` — `new`/`init`/`rm` limina worktrees that build and test without touching each
//!              other or the main checkout (`src/worktree.rs`).
//!   `mesa`   — build the HOST Mesa (KosmicKrisp + zink-on-KK), creating the case-sensitive
//!              volume and cloning the tree when they do not exist yet. `build` links libEGL
//!              out of its prefix, so on a machine that has never been handed that volume this
//!              runs once between `setup` and `build` (wraps `scripts/build-host-mesa.sh`).
//!   `build`  — build `limina` + `limina-vmm` and codesign the worker (hypervisor entitlement).
//!              The inner-loop "make a runnable worker" step.
//!
//! Linux-side builds — all of them in the ONE `limina-build` container image
//! (`scripts/build-image.sh`, Fedora 44 by default, `FEDORA_REL` to move it):
//!   `firmware` — the GOP `KRUN_EFI` the EFI boot path and the test suite default to
//!              (wraps `scripts/build-krun-efi.sh`).
//!   `enhanced` — the enhanced-tier guest RPMs + payload: 16 KiB kernel, venus mesa, agents
//!              (wraps `scripts/build-enhanced-rpms.sh`, which runs the very same
//!              `scripts/provision/f44/*` a booted guest runs).
//!   `sign`   — codesign an already-built worker (just the hypervisor-entitlement step).
//!   `test`   — build + sign + run the HVF-gated boot tests (wraps
//!              `scripts/test-boot.sh`). The canonical "did I break boot" command.
//!
//! Run / package:
//!   `run`    — boot an enhanced-tier image to the seated venus desktop in a window (EFI+venus, the
//!              documented default boot; wraps `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`).
//!   `app`    — assemble the full self-contained `target/Limina.app` (the shipping bundle with the
//!              whole host venus/GL closure) plus the `target/Limina.dmg` that carries it to
//!              another Mac intact; wraps `scripts/build-app.sh`.
//!   `bundle` — assemble a *minimal* codesigned `target/Limina.app` and (optionally) launch it
//!              through LaunchServices. This validates the *normal* launch path early: an app
//!              started via `open`/double-click runs under launchd with a real GUI/GPU session,
//!              rather than inheriting a terminal's (or sshd's) context — which is where the
//!              worker's virtio-gpu init behaves differently. With `--open` and
//!              `LIMINA_WINDOW_CAPTURE` baked into the bundle, a capture PNG appearing means the
//!              worker did *not* hang and the layer rendered. (Distinct from `app`: `bundle` is a
//!              launch-path smoke test booting the L1 guest, `app` is the real deliverable.)

mod git;
mod manifest;
mod pins;
mod worktree;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::manifest::{MANIFEST_LOCAL, Manifest};

/// Where the smoke bundle's supervisor writes its rendered-layer PNG (LSEnvironment), so a
/// normal launch is self-verifiable without screen-recording permission. Under this checkout's
/// `target/`, so two worktrees' smoke tests do not read each other's frames.
fn capture_path(repo: &Path) -> PathBuf {
    repo.join("target/limina-smoke-capture.png")
}

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
    /// Materialize the gitignored `third_party/` source trees (fork clones at the manifest-pinned
    /// revs). Run once after a fresh clone (or a libkrun re-clone) before building.
    Vendor {
        /// Also clone the `heavy = true` fork-model deps (the kernel tree — multi-GB, and this
        /// host never builds it). Needed only to author kernel commits or export its series.
        #[arg(long)]
        heavy: bool,
    },
    /// Show every fork pin, the tree that stands in for it here, and whether its remote branch
    /// carries it — then the pushes that would publish it.
    Pins {
        /// Exit nonzero unless every pin is pushed (the pre-push hook's gate).
        #[arg(long)]
        check: bool,
        /// Check the manifest as committed at this revision instead of the working tree's.
        #[arg(long, value_name = "COMMIT")]
        at: Option<String>,
        /// Don't ask the remotes; trust the remote-tracking refs already fetched.
        #[arg(long)]
        no_fetch: bool,
    },
    /// Create, initialize or remove limina worktrees (forks as worktrees of main's clones,
    /// shared test images and venv, own target/).
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Build the host Mesa (KosmicKrisp + zink-on-KK) that `build` and `run` need, creating
    /// the case-sensitive volume and cloning the tree if absent. Run once per machine.
    Mesa {
        /// Which half to build: `kk`, `zink`, or `both` (the default).
        #[arg(value_name = "kk|zink|both")]
        what: Option<String>,
    },
    /// Build `limina` + `limina-vmm` and codesign the worker (hypervisor entitlement).
    Build {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
    },
    /// Build the GOP KRUN_EFI firmware (the EFI boot path's, and the test suite's, default).
    Firmware,
    /// Write the third-party notices About → Licenses shows (`app` does this into the bundle).
    Notices {
        /// Where to write them.
        #[arg(long, default_value = "target/THIRD-PARTY-NOTICES.txt")]
        out: PathBuf,
    },
    /// Build the enhanced-tier guest RPMs + payload in the unified Linux build container.
    Enhanced {
        /// Which half to build: `kernel`, `mesa`, or `all` (the default, which also assembles
        /// the install-ready payload).
        #[arg(value_name = "kernel|mesa|all")]
        what: Option<String>,
    },
    /// Codesign the already-built worker with the hypervisor entitlement (no build).
    Sign {
        /// Sign the release-profile worker.
        #[arg(long)]
        release: bool,
    },
    /// Build + sign, then run the HVF-gated boot tests (wraps scripts/test-boot.sh).
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
        /// Extra flags forwarded to `limina` (e.g. `--no-normalize-modifiers`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
    /// Assemble the full self-contained `target/Limina.app` + `target/Limina.dmg`
    /// (wraps scripts/build-app.sh).
    /// Builds RELEASE by default — the .app is the deployable artifact, and an explicit
    /// debug profile passed here once overrode build-app.sh's release default and shipped
    /// a debug bundle to the dogfood Mac (it happened twice).
    App {
        /// Build a debuggable (unoptimized, debug-assertions) bundle instead of release.
        #[arg(long)]
        debug: bool,
    },
    /// Run the in-crate checkers (wraps scripts/check.py): `kani [crate-dir ...]`,
    /// `loom [crate-dir ...]`, `miri [crate-dir ...] [--stall N]`, `fuzz [target ...]
    /// [--seconds N]`, or `sabotage [pattern ...]`. No HVF, no signing; see
    /// docs/design/in-crate-checkers.md.
    Check {
        /// `kani`, `loom`, `miri`, `fuzz` or `sabotage`, then that tool's arguments, forwarded
        /// verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Build + assemble + codesign a minimal `target/Limina-smoke.app` (launch-path smoke test).
    Bundle {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
        /// Launch the bundle via LaunchServices (`open`) booting the L1 `limina.hold` guest.
        #[arg(long)]
        open: bool,
    },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// `git worktree add` a new branch, then `init` it.
    New {
        /// Branch name; also the directory name under `.claude/worktrees/`.
        name: String,
        /// Revision to branch from (default: HEAD of the checkout you run this in).
        #[arg(long)]
        base: Option<String>,
        /// Put the worktree here instead of `.claude/worktrees/<name>`.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Make the worktree this runs in buildable: share the test images, venv and Mesa image,
    /// clone the slow build outputs, vendor the forks as worktrees of main's clones.
    Init,
    /// Remove a worktree, its fork worktrees and its target/. Refuses when anything in it would
    /// be lost, unless `--force`.
    Rm {
        /// The name given to `new`, or a path.
        name: String,
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup => setup(),
        Cmd::Vendor { heavy } => vendor(&repo_root(), heavy),
        Cmd::Pins {
            check,
            at,
            no_fetch,
        } => pins::pins(
            &repo_root(),
            &pins::Opts {
                check,
                at,
                no_fetch,
            },
        ),
        Cmd::Worktree { cmd } => match cmd {
            WorktreeCmd::New { name, base, path } => {
                worktree::new(&repo_root(), &name, base.as_deref(), path)
            }
            WorktreeCmd::Init => worktree::init(&repo_root()),
            WorktreeCmd::Rm { name, force } => worktree::rm(&repo_root(), &name, force),
        },
        Cmd::Mesa { what } => mesa(what.as_deref()),
        Cmd::Build { release } => build(release),
        Cmd::Firmware => bash_script(&repo_root(), "scripts/build-krun-efi.sh", &[] as &[&str]),
        Cmd::Notices { out } => run(Command::new("python3")
            .current_dir(repo_root())
            .arg("scripts/gen-third-party-notices.py")
            .arg("--out")
            .arg(out)),
        Cmd::Enhanced { what } => enhanced(what.as_deref()),
        Cmd::Sign { release } => sign_worker(&repo_root(), release),
        Cmd::Test { release, args } => test(release, &args),
        Cmd::Run {
            disk,
            no_net,
            cpus,
            ram_mib,
            extra,
        } => run_vm(disk, no_net, cpus, ram_mib, &extra),
        Cmd::App { debug } => app(!debug),
        Cmd::Bundle { release, open } => bundle(release, open),
        Cmd::Check { args } => {
            let repo = repo_root();
            run(Command::new("python3")
                .current_dir(&repo)
                .arg(repo.join("scripts/check.py"))
                .args(&args))
        }
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
    if release { "release" } else { "debug" }
}

// --- bootstrap ---------------------------------------------------------------------------------

/// One-command fresh-clone bootstrap: vendor `third_party/`, then point git at the in-repo hooks.
fn setup() -> Result<()> {
    vendor(&repo_root(), false)?;
    eprintln!("==> enabling git hooks (core.hooksPath = .githooks)");
    bash_script(&repo_root(), "scripts/setup-hooks.sh", &[] as &[&str])?;
    eprintln!("==> setup complete");
    Ok(())
}

/// Materialize every gitignored `third_party/` source tree under `root`, so the workspace can
/// build.
///
/// Every dep is fork-model now (github.com/liminavm): clone our fork and check out the rev
/// pinned in `third_party/manifest.toml` — the fork's `limina` branch IS the delta, no patch
/// series. (edk2 is fork-model too but not vendored here: `scripts/build-krun-efi.sh` clones
/// its pinned rev inside its own container build volume.)
///
/// `third_party/manifest.local.toml` changes where a fork comes from (`src/manifest.rs`). And in
/// a linked limina worktree, a fork that is absent becomes a worktree of the main checkout's
/// clone instead of a fresh clone: no network, and commits nobody has pushed are already there.
///
/// Idempotent — re-running resets/refreshes each tree.
fn vendor(root: &Path, heavy: bool) -> Result<()> {
    let manifest = Manifest::load(root)?;
    let main = if git::is_linked_worktree(root)? {
        Some(git::main_worktree(root)?)
    } else {
        None
    };

    // The generators virglrs's build.rs runs (venus-gen, vrend-gen) import mako and yaml
    // through a bare `python3`. Materialize the venv that provides them here, so a fresh clone
    // builds without anything having been installed into the host's own Python -- which is
    // what "it builds on my machine" turned out to mean.
    bash_script(root, "scripts/ensure-venv-mesa.sh", &[] as &[&str])?;

    // libkrun: the VMM library. limina consumes its crates by path ([workspace.dependencies]),
    // so the checkout is the build input directly — fork model (task #14).
    vendor_fork(root, &manifest, "libkrun", main.as_deref())?;

    // virglrs: the host GPU renderer for both accelerated tiers, consumed by rutabaga as a Rust
    // crate. It pins and materializes its own dependencies — the C virglrenderer it generates
    // format tables from and records goldens against, and the venus-protocol the wire comes from
    // — so limina pins virglrs and nothing underneath it.
    vendor_fork(root, &manifest, "virglrs", main.as_deref())?;
    let virglrs = root.join("third_party/virglrs");
    let nested = virglrs.join("third_party/virglrenderer");
    if manifest.dep("virglrs")?.checkout.is_some() && git::is_tree(&nested) {
        // A checkout override is used as it stands, and vendor.sh would move its
        // virglrenderer onto virglrs's pin.
        eprintln!("==> virglrs is a checkout override: leaving its own third_party/ as it is");
    } else {
        eprintln!("==> vendoring virglrs's own dependencies");
        // Absolute program path: a relative one resolves against the parent's cwd on some
        // platforms and the child's on others, and `current_dir` is set here.
        let mut c = Command::new(virglrs.join("scripts/vendor.sh"));
        c.current_dir(&virglrs);
        // In a worktree, virglrs's pin of the C tree comes from main's copy of it — the same
        // fetch-from-local rule, through the variable virglrs's own script already honours. Only
        // when main's copy has that pin: a virglrs bump main has not vendored yet names a rev it
        // lacks, and the network has it. The rev is fetched here, by SHA: the script fetches
        // branches only, and main's copy holds its pin as a detached HEAD.
        if let Some(main) = &main {
            let src = main.join("third_party/virglrs/third_party/virglrenderer");
            let dest = virglrs.join("third_party/virglrenderer");
            if std::env::var_os("VIRGLRENDERER_SRC").is_none()
                && git::is_tree(&src)
                && let Some(rev) = nested_pin(&virglrs, "virglrenderer")?
                && git::has_commit(&src, &rev)
            {
                if git::is_tree(&dest) && !git::has_commit(&dest, &rev) {
                    git::run(&dest, &["fetch", "--quiet", &src.to_string_lossy(), &rev])?;
                }
                c.env("VIRGLRENDERER_SRC", src);
            }
        }
        run(&mut c)?;
    }

    // imago: the fork-model pilot ([patch.crates-io] path override; the tree is a clone of our
    // fork pinned by third_party/manifest.toml — no patch series, the `limina` branch IS the delta).
    vendor_fork(root, &manifest, "imago", main.as_deref())?;

    // janus: the TPM 2.0 engine libkrun's TIS device runs, consumed as a path dependency of
    // krun-devices. Ours outright, like virglrs: its `main` branch is the whole thing.
    vendor_fork(root, &manifest, "janus", main.as_deref())?;

    // linux: the enhanced-tier guest kernel fork. Marked `heavy` in the manifest — a multi-GB
    // tree this host never builds (the kernel builds in a Linux container / build guest, which
    // fetches the pinned rev itself), so it is opt-in.
    if heavy {
        vendor_fork(root, &manifest, "linux", main.as_deref())?;
    }

    eprintln!("==> vendor complete");
    // Vendoring is not the whole bootstrap: `build` links libEGL out of the host Mesa prefix,
    // which lives on a volume this repo cannot carry. Name the step that is actually next
    // rather than declaring a readiness we have not checked.
    if host_mesa_egl().exists() {
        eprintln!("    next: `cargo xtask build` / `cargo xtask test`");
    } else {
        eprintln!("    next: `cargo xtask mesa` — the host Mesa prefix `build` links libEGL");
        eprintln!("          from is absent or unmounted");
    }
    Ok(())
}

/// Materialize a fork-model dependency under `root/third_party/<name>`.
///
/// - A `checkout` override: a symlink to that tree, used as it stands.
/// - Absent, in a linked worktree whose main checkout has the fork: a worktree of that clone,
///   detached at the pin.
/// - Absent otherwise: clone the fork (from the `source` override, when there is one), adding
///   upstream as a second remote, and check out the pinned rev.
/// - Present: fetch only if the pinned rev is absent, then check it out. Local work is left
///   alone when it already contains the pin (we only move HEAD when the pin is missing from it,
///   i.e. after a `manifest.toml` bump).
fn vendor_fork(root: &Path, manifest: &Manifest, name: &str, main: Option<&Path>) -> Result<()> {
    let dep = manifest.dep(name)?;
    let dir = root.join("third_party").join(name);
    if let Some(checkout) = &dep.checkout {
        return link_checkout(&dir, name, checkout, dep.rev.as_deref());
    }
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        bail!(
            "third_party/{name} is a symlink, but {MANIFEST_LOCAL} has no `checkout` for it: \
             restore the override, or remove the link to vendor the pin"
        );
    }
    let repo_url = dep.require("repo", &dep.repo)?;
    let branch = dep.require("branch", &dep.branch)?;
    let rev = dep.require("rev", &dep.rev)?;
    let dir_s = dir.to_str().context("third_party path is not UTF-8")?;

    let main_clone = main
        .map(|m| m.join("third_party").join(name))
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| git::is_tree(p));
    if !git::is_tree(&dir) {
        if let Some(main_clone) = &main_clone {
            if !git::has_commit(main_clone, &rev) {
                fetch_pin(main_clone, dep, &rev)?;
            }
            eprintln!(
                "==> {name}: worktree of {} detached at the pin {rev}",
                main_clone.display()
            );
            git::run(main_clone, &["worktree", "add", "--detach", dir_s, &rev])?;
        } else {
            let from = dep
                .source
                .as_deref()
                .map(|s| s.display().to_string())
                .unwrap_or_else(|| repo_url.clone());
            eprintln!("==> cloning {name} fork ({from}) — third_party/{name} is absent");
            let mut c = git::command(root);
            c.args(["clone", "--branch", &branch]);
            // Heavy trees (the kernel) clone blobless: full history, file contents fetched on
            // demand. A plain clone of linux is several GB before you have touched anything.
            if dep.heavy {
                c.arg("--filter=blob:none");
            }
            c.args([&from, dir_s]);
            run(&mut c)?;
            if dep.source.is_some() {
                git::run(&dir, &["remote", "set-url", "origin", &repo_url])?;
                git::run(&dir, &["remote", "add", "local", &from])?;
            }
            if let Some(upstream) = &dep.upstream {
                git::run(&dir, &["remote", "add", "upstream", upstream])?;
            }
        }
    }
    if !git::has_commit(&dir, &rev) {
        fetch_pin(&dir, dep, &rev)?;
    }

    if git::is_linked_worktree(&dir)? {
        // Detached, never on the fork's branch: a branch can be checked out in only one worktree
        // of a repository, and main's clone already has it.
        if git::is_ancestor(&dir, &rev, "HEAD") {
            let head = git::out(&dir, &["rev-parse", "HEAD"])?;
            if head != rev {
                eprintln!(
                    "==> {name}: HEAD {head} carries the pin plus local commits — left alone"
                );
            }
        } else if git::is_dirty(&dir)? {
            bail!(
                "{name}: HEAD does not contain the pin {rev} and the tree has uncommitted \
                 changes — commit them, then re-run"
            );
        } else {
            eprintln!("==> {name}: moving the detached HEAD to the pin {rev}");
            git::run(&dir, &["checkout", "--detach", &rev])?;
        }
        // Hooks: config is shared with main's clone, which already has its own set.
        return Ok(());
    }

    if git::is_ancestor(&dir, &rev, &branch) {
        git::run(&dir, &["checkout", &branch])?;
    } else {
        eprintln!("==> {name}: moving {branch} to the pinned rev {rev}");
        git::run(&dir, &["checkout", "-B", &branch, &rev])?;
    }

    // A fork with hooks of its own in `.githooks/<name>/` gets them wired here rather than in
    // scripts/setup-hooks.sh: this is the only path that can create the clone, and a re-vendor
    // that re-clones would otherwise silently drop the config. `core.hooksPath` is local, so it
    // has to be set per clone. Absolute, so git worktrees off the clone find it too.
    let hooks = root.join(".githooks").join(name);
    if hooks.is_dir() {
        git::run(
            &dir,
            &["config", "core.hooksPath", &hooks.display().to_string()],
        )?;
    }
    Ok(())
}

/// The rev a dependency's checkout at `tree` pins `name` at, from its own committed manifest.
fn nested_pin(tree: &Path, name: &str) -> Result<Option<String>> {
    let path = tree.join(manifest::MANIFEST);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let nested = Manifest::parse(tree, &text, &path.display().to_string(), None)?;
    Ok(nested.dep(name).ok().and_then(|d| d.rev.clone()))
}

/// Bring the pinned rev into `dir`: from the local override when there is one, else origin.
fn fetch_pin(dir: &Path, dep: &manifest::Dep, rev: &str) -> Result<()> {
    if let Some(src) = dep.local_repo() {
        let src = src.to_str().context("override path is not UTF-8")?;
        eprintln!("==> {}: fetching the pin {rev} from {src}", dep.name);
        git::run(dir, &["fetch", src, rev])
    } else {
        eprintln!("==> {}: fetching (pinned rev {rev} not present)", dep.name);
        git::run(dir, &["fetch", "origin", "--tags"])
    }
}

/// Make `third_party/<name>` a symlink to a `checkout` override. Never over a real tree: that
/// one may hold work, and moving it aside is the user's call.
fn link_checkout(dir: &Path, name: &str, checkout: &Path, pin: Option<&str>) -> Result<()> {
    if !git::is_tree(checkout) {
        bail!(
            "{MANIFEST_LOCAL}: [{name}] checkout {} is not a git working tree",
            checkout.display()
        );
    }
    match dir.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(dir)? != checkout {
                std::fs::remove_file(dir)?;
                std::os::unix::fs::symlink(checkout, dir)?;
            }
        }
        Ok(_) => bail!(
            "third_party/{name} is a real tree, and a checkout override takes its place as a \
             symlink — move it aside first (it may hold work), then re-run"
        ),
        Err(_) => std::os::unix::fs::symlink(checkout, dir)?,
    }
    let head = git::out(checkout, &["rev-parse", "HEAD"])?;
    let relation = match pin {
        Some(pin) if pin == head => "at the pin".to_string(),
        Some(pin) if git::is_ancestor(checkout, pin, &head) => {
            format!("the pin {pin} plus local commits")
        }
        Some(pin) => format!("does NOT contain the pin {pin}"),
        None => "no pin".to_string(),
    };
    eprintln!(
        "==> {name}: LOCAL CHECKOUT OVERRIDE -> {} (HEAD {head}, {relation})",
        checkout.display()
    );
    Ok(())
}

/// The one file whose presence decides whether the workspace can link: virglrs links `libEGL`
/// from the zink-on-KosmicKrisp prefix, and that prefix lives on a case-sensitive sparse image
/// (Mesa does not check out on APFS), so it can never be carried by the repo.
fn host_mesa_egl() -> PathBuf {
    PathBuf::from("/Volumes/mesa-cs/zink-kk-prefix/lib/libEGL.dylib")
}

/// Build the host Mesa: the KosmicKrisp ICD venus renders through, and the zink-on-KK prefix
/// `build` links `libEGL` from. The script creates the case-sensitive volume and clones the
/// tree at its manifest pin when they are absent -- the bootstrap step that used to exist only
/// in one developer's home directory.
fn mesa(what: Option<&str>) -> Result<()> {
    let repo = repo_root();
    let args: Vec<&str> = what.into_iter().collect();
    bash_script(&repo, "scripts/build-host-mesa.sh", &args)
}

/// Build the enhanced-tier guest RPMs in the unified Linux build container.
///
/// The script runs `scripts/provision/f44/*` unchanged -- the same files a booted guest runs.
/// They need an F44 aarch64 system, which the build image now is; that they once needed a guest
/// was a fact about the image being pinned to Fedora 43, not about containers.
fn enhanced(what: Option<&str>) -> Result<()> {
    let repo = repo_root();
    let args: Vec<&str> = what.into_iter().collect();
    bash_script(&repo, "scripts/build-enhanced-rpms.sh", &args)
}

/// Fail before the compile, in the vocabulary of the fix, when the host Mesa prefix is missing.
///
/// Without it `cargo build` dies in virglrs's build script several hundred crates in, naming a
/// `/Volumes/…` path the reader has no way to produce and no script in the tree ever created.
/// `EGL_LIB_DIR` and `MESA_PREFIX` are build.rs's own escapes, so anyone who set one is left
/// alone; macOS drops the mount on every reboot, so try remounting before judging it absent.
fn ensure_host_mesa(repo: &Path) -> Result<()> {
    if std::env::var_os("EGL_LIB_DIR").is_some() || std::env::var_os("MESA_PREFIX").is_some() {
        return Ok(());
    }
    if !host_mesa_egl().exists() {
        let _ = bash_script(repo, "scripts/ensure-mesa-cs.sh", &[] as &[&str]);
    }
    if host_mesa_egl().exists() {
        return Ok(());
    }
    bail!(
        "the host Mesa prefix is missing: no {}\n\
         virglrs links libEGL from there, so the workspace cannot link without it.\n\
         Run `cargo xtask mesa` once on this machine — it creates the case-sensitive volume,\n\
         clones Mesa at its manifest pin and builds KosmicKrisp + zink-on-KK.\n\
         (Or point EGL_LIB_DIR at a Mesa prefix you already have.)",
        host_mesa_egl().display()
    )
}

/// Put `third_party/venv-mesa` at the front of a child's `PATH`.
///
/// virglrs's build script runs its venus and vrend generators through a bare `python3`, so mako
/// and yaml have to be importable from whatever `python3` resolves to *in the build script's*
/// environment -- not in the shell someone typed `cargo` into. Prepending the repo venv is what
/// makes that true without installing into the host's Python.
fn with_venv_mesa(repo: &Path, cmd: &mut Command) {
    let bin = repo.join("third_party/venv-mesa/bin");
    if !bin.is_dir() {
        return;
    }
    let mut dirs = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        cmd.env("PATH", joined);
    }
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
    with_venv_mesa(repo, &mut c);
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

/// The inner-loop "make a runnable worker": build both binaries, sign the worker. Everything a
/// `cargo xtask run` / a manual boot needs, minus the tests.
fn build(release: bool) -> Result<()> {
    let repo = repo_root();
    ensure_host_mesa(&repo)?;
    cargo_build_binaries(&repo, release)?;
    sign_worker(&repo, release)?;
    eprintln!(
        "==> build complete: target/{}/{{limina,limina-vmm}} (worker signed)",
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
/// venus/GL dylib closure vendored into `Contents/Frameworks` — and the `Limina.dmg` that carries
/// it to another Mac intact. All of it lives in build-app.sh.
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

    // 3. Assemble Limina-smoke.app/Contents/{MacOS,Info.plist}. Deliberately NOT
    //    target/Limina.app: this bundle is debug-by-default and ad-hoc signed, and ad-hoc
    //    pins TCC's grants to a CDHash (Accessibility dies, CGEventTap returns NULL). At the
    //    deliverable's own path it would be indistinguishable from `xtask app`'s output, so
    //    "run it from target/Limina.app" would mean two different things depending on which
    //    command ran last.
    let app = target.join("Limina-smoke.app");
    let macos = app.join("Contents/MacOS");
    if app.exists() {
        std::fs::remove_dir_all(&app).with_context(|| format!("rm {app:?}"))?;
    }
    std::fs::create_dir_all(&macos).with_context(|| format!("mkdir {macos:?}"))?;
    for bin in ["limina", "limina-vmm"] {
        std::fs::copy(profile_dir.join(bin), macos.join(bin))
            .with_context(|| format!("copy {bin} into the bundle"))?;
    }
    let capture = capture_path(&repo);
    std::fs::write(app.join("Contents/Info.plist"), info_plist(&capture))
        .context("writing Info.plist")?;
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
        eprintln!(
            "==> launching via LaunchServices (open); capture -> {}",
            capture.display()
        );
        let _ = std::fs::remove_file(&capture);
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
            "    launched. Watch this Mac's screen; check {} for the rendered layer.",
            capture.display()
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
fn info_plist(capture: &Path) -> String {
    let capture = capture.display();
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
        <key>LIMINA_WINDOW_CAPTURE</key><string>{capture}</string>
        <key>RUST_LOG</key><string>info</string>
    </dict>
</dict>
</plist>
"#
    )
}
