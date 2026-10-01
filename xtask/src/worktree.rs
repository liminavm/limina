// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `cargo xtask worktree`: limina worktrees that build, test and run without touching each
//! other or the main checkout.
//!
//! What a bare `git worktree add` leaves out, and what this adds:
//! - `third_party/`: gitignored, so absent. `vendor` run in a linked worktree makes each fork a
//!   worktree of the main checkout's clone, detached at the pin: objects are shared, so it sees
//!   commits nobody has pushed yet, and it has a HEAD of its own.
//! - What is shared on purpose, as symlinks: the test images (the harness clones them before
//!   booting, never writes them), the Python venv, and the case-sensitive Mesa image (one host
//!   Mesa per machine).
//! - What is cloned (`cp -c`, free on APFS) so a rebuild here does not change main's: the L1 test
//!   guest, the firmware and the trap probe under `target/`.
//!
//! `target/` itself is per worktree: sharing it would relink one worktree's worker under
//! another's running suite. It is the expensive part, and `rm` deletes it with the worktree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;

/// Build outputs that are slow to make and safe to start from main's copy.
const CLONED_OUTPUTS: &[&str] = &[
    "target/test-guest",
    "target/krun-efi",
    "target/hvf-trap-probe",
];

/// `third_party/` entries that are host-wide state, not per-tree source.
const SHARED_THIRD_PARTY: &[&str] = &["venv-mesa", "mesa-cs.sparseimage", "epoxy-egl-prefix"];

/// Where a named worktree lives: beside the ones Claude Code creates, so either can enter it.
fn default_path(main: &Path, name: &str) -> PathBuf {
    main.join(".claude/worktrees").join(name)
}

pub fn new(root: &Path, name: &str, base: Option<&str>, path: Option<PathBuf>) -> Result<()> {
    let main = git::main_worktree(root)?;
    let path = path.unwrap_or_else(|| default_path(&main, name));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let path_s = path.to_str().context("worktree path is not UTF-8")?;
    eprintln!(
        "==> git worktree add -b {name} {path_s} {}",
        base.unwrap_or("HEAD")
    );
    git::run(
        root,
        &[
            "worktree",
            "add",
            "-b",
            name,
            path_s,
            base.unwrap_or("HEAD"),
        ],
    )?;
    init(&path)?;
    eprintln!("==> worktree ready: {path_s} (branch {name})");
    eprintln!("    remove with: cargo xtask worktree rm {name}");
    Ok(())
}

/// Make the linked worktree at `wt` buildable: share what is host-wide, clone the slow build
/// outputs, then vendor. Idempotent, so it also repairs a worktree made by plain `git worktree
/// add`.
pub fn init(wt: &Path) -> Result<()> {
    if !git::is_linked_worktree(wt)? {
        bail!(
            "{} is the main checkout, not a linked worktree — there is nothing to share it from",
            wt.display()
        );
    }
    let main = git::main_worktree(wt)?;
    fix_hooks_path(&main)?;

    // Before vendor: ensure-venv-mesa.sh would otherwise build a second venv from the network.
    for name in SHARED_THIRD_PARTY {
        link_if_absent(
            &main.join("third_party").join(name),
            &wt.join("third_party").join(name),
        )?;
    }
    for entry in std::fs::read_dir(&main).with_context(|| format!("listing {}", main.display()))? {
        let name = entry?.file_name();
        let Some(n) = name.to_str() else { continue };
        if is_shared_image(n) {
            link_if_absent(&main.join(n), &wt.join(n))?;
        }
    }
    for out in CLONED_OUTPUTS {
        let (from, to) = (main.join(out), wt.join(out));
        if from.exists() && !to.exists() {
            std::fs::create_dir_all(to.parent().unwrap())?;
            eprintln!("==> cloning {out} from the main checkout (cp -c)");
            crate::run(
                std::process::Command::new("cp")
                    .arg("-cR")
                    .arg(&from)
                    .arg(&to),
            )?;
        }
    }

    crate::vendor(wt, false)?;
    eprintln!("    note: target/ here is this worktree's own — a first build is a full one");
    Ok(())
}

/// The test images and ISOs the harness finds at the repository root. Backups stay behind.
fn is_shared_image(name: &str) -> bool {
    (name.starts_with("Fedora-") || name.starts_with("Debian-"))
        && (name.ends_with(".raw") || name.ends_with(".iso"))
        && !name.contains(".bak")
}

fn link_if_absent(target: &Path, link: &Path) -> Result<()> {
    if !target.exists() || link.symlink_metadata().is_ok() {
        return Ok(());
    }
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}

/// `core.hooksPath` lives in the config every worktree shares. Set absolute, it makes every
/// worktree run the MAIN checkout's hook scripts; relative, git resolves it per worktree, which
/// is what scripts/setup-hooks.sh writes. Repair only the exact absolute form it replaces.
fn fix_hooks_path(main: &Path) -> Result<()> {
    let Ok(current) = git::out(main, &["config", "--get", "core.hooksPath"]) else {
        return Ok(());
    };
    if Path::new(&current) == main.join(".githooks") {
        eprintln!(
            "==> core.hooksPath was {current} (main's hooks for every worktree); setting .githooks"
        );
        git::run(main, &["config", "core.hooksPath", ".githooks"])?;
    }
    Ok(())
}

pub fn rm(root: &Path, which: &str, force: bool) -> Result<()> {
    let main = git::main_worktree(root)?;
    let path = if which.contains('/') {
        PathBuf::from(which)
    } else {
        default_path(&main, which)
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("no worktree at {}", path.display()))?;
    if !git::worktrees(root)?
        .iter()
        .skip(1)
        .any(|w| w.canonicalize().ok().as_ref() == Some(&path))
    {
        bail!(
            "{} is not a linked worktree of this repository",
            path.display()
        );
    }
    if !force && git::is_dirty(&path)? {
        bail!(
            "{} has uncommitted changes — commit them, or pass --force",
            path.display()
        );
    }

    // The forks first: each is a worktree of a repository that outlives this one, so it has to
    // be deregistered there, and a detached HEAD carrying commits no ref holds would leave them
    // reachable from nothing.
    let third_party = path.join("third_party");
    let mut fork_trees = Vec::new();
    for entry in std::fs::read_dir(&third_party).into_iter().flatten() {
        let p = entry?.path();
        if p.symlink_metadata()?.file_type().is_symlink() {
            continue;
        }
        if git::is_tree(&p) && git::is_linked_worktree(&p)? {
            if !force {
                if git::is_dirty(&p)? {
                    bail!(
                        "{} has uncommitted changes — commit them, or pass --force",
                        p.display()
                    );
                }
                let holders = git::out(&p, &["for-each-ref", "--contains", "HEAD", "--count=1"])?;
                if holders.is_empty() {
                    bail!(
                        "{}: HEAD carries commits no branch or tag holds; branch them first \
                         (git -C {} branch <name>), or pass --force",
                        p.display(),
                        p.display()
                    );
                }
            }
            fork_trees.push(p);
        }
    }
    for p in &fork_trees {
        let repo = git::common_dir(p)?;
        let p_s = p.to_str().context("path is not UTF-8")?;
        eprintln!("==> removing fork worktree {p_s}");
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(p_s);
        git::run(repo.parent().unwrap_or(&repo), &args)?;
    }
    // The links `init` made point at shared state: remove the links, never what they name, and
    // only those — the tree tracks symlinks of its own (AGENTS.md), and deleting one is a change
    // `git worktree remove` then refuses over.
    let ours = [path.clone(), third_party].into_iter().flat_map(|dir| {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect::<Vec<_>>()
    });
    for p in ours {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let made_by_init = is_shared_image(name) || SHARED_THIRD_PARTY.contains(&name);
        if made_by_init && p.symlink_metadata()?.file_type().is_symlink() {
            std::fs::remove_file(&p)?;
        }
    }

    let path_s = path.to_str().context("path is not UTF-8")?;
    eprintln!("==> removing {path_s} (with its target/)");
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_s);
    git::run(&main, &args)?;
    eprintln!("    the branch is kept; delete it with git branch -d once it is merged");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_shared_image;

    #[test]
    fn shares_test_images_not_backups() {
        assert!(is_shared_image("Fedora-Workstation-44.enhanced.test.raw"));
        assert!(is_shared_image("Fedora-Server-netinst-aarch64-43-1.6.iso"));
        assert!(is_shared_image("Debian-testing.luks.raw"));
        assert!(!is_shared_image(
            "Fedora-Workstation-44.enhanced.bak-pre-r27.raw"
        ));
        assert!(!is_shared_image("Fedora-Workstation-44.vanilla.raw.xz"));
        assert!(!is_shared_image("tp-poke.raw"));
    }
}
