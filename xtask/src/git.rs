// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The few git questions xtask asks, each as one function.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The variables `git rev-parse --local-env-vars` lists: they bind git to ONE repository, and
/// a git hook exports them for the repository it runs in. `current_dir` does not override them,
/// so a `cargo xtask` run from a hook would ask every question of limina's repository instead
/// of the fork it named — or, for a write, change limina's.
const LOCAL_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// `git`, in `dir`, bound to whatever repository `dir` is in and nothing inherited.
pub fn command(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir);
    for var in LOCAL_ENV {
        c.env_remove(var);
    }
    c
}

/// Run git in `dir` and return its trimmed stdout; an error carries git's stderr.
pub fn out(dir: &Path, args: &[&str]) -> Result<String> {
    let o = command(dir)
        .args(args)
        .output()
        .with_context(|| format!("spawning git {args:?} in {}", dir.display()))?;
    if !o.status.success() {
        bail!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Run git in `dir`, inheriting the terminal, and fail loudly.
pub fn run(dir: &Path, args: &[&str]) -> Result<()> {
    crate::run(command(dir).args(args))
}

/// Whether git in `dir` succeeds (for the yes/no questions: is-ancestor, cat-file -e).
pub fn ok(dir: &Path, args: &[&str]) -> bool {
    command(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `dir` is (the top of) a git working tree. `.git` is a directory in a clone and a file in a
/// linked worktree, so test for either.
pub fn is_tree(dir: &Path) -> bool {
    dir.join(".git").exists()
}

pub fn has_commit(dir: &Path, rev: &str) -> bool {
    ok(dir, &["cat-file", "-e", &format!("{rev}^{{commit}}")])
}

pub fn is_ancestor(dir: &Path, ancestor: &str, of: &str) -> bool {
    ok(dir, &["merge-base", "--is-ancestor", ancestor, of])
}

pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!out(dir, &["status", "--porcelain"])?.is_empty())
}

fn absolute(dir: &Path, p: String) -> PathBuf {
    let p = PathBuf::from(p);
    let p = if p.is_absolute() { p } else { dir.join(p) };
    p.canonicalize().unwrap_or(p)
}

/// The repository's shared git directory (the same for every worktree of it).
pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    Ok(absolute(dir, out(dir, &["rev-parse", "--git-common-dir"])?))
}

/// `dir` is a linked worktree: its own git dir is not the repository's shared one. Config is
/// shared across a repository's worktrees, so these must not have per-tree settings written.
pub fn is_linked_worktree(dir: &Path) -> Result<bool> {
    let own = absolute(dir, out(dir, &["rev-parse", "--absolute-git-dir"])?);
    Ok(own != common_dir(dir)?)
}

/// The main working tree of the repository `dir` belongs to: the first entry of
/// `git worktree list`, which git always lists first.
pub fn main_worktree(dir: &Path) -> Result<PathBuf> {
    let list = out(dir, &["worktree", "list", "--porcelain"])?;
    let first = list
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        .context("git worktree list printed no worktree")?;
    Ok(PathBuf::from(first))
}

/// Every worktree path of the repository `dir` belongs to, main first.
pub fn worktrees(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(out(dir, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}
