// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `cargo xtask pins`: every fork pin, the tree that stands in for it here, and whether its
//! remote carries it — then the pushes that would make it so.
//!
//! This is what makes it safe to commit pins before their forks are pushed. A limina commit may
//! name revs that exist only on this machine for as long as it stays here; it must not be
//! published that way, because then nobody else can vendor it. `--check` exits nonzero while
//! any pin is unpushed, and the limina pre-push hook runs it on every commit being published.
//!
//! "Pushed" means the rev is an ancestor of the fork's `branch` on its remote. A rev reachable
//! only through some other remote ref (a backup tag after a rewrite, a side branch) is fetchable,
//! but the gate refuses it all the same: the `branch` a manifest names is where our work lives,
//! and a pin off it is work that branch does not carry.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

use crate::git;
use crate::manifest::{Dep, MANIFEST, Manifest};

pub struct Opts {
    /// Exit nonzero unless every pin is on its manifest branch on the remote.
    pub check: bool,
    /// Read the committed manifest from this commit instead of the working tree.
    pub at: Option<String>,
    /// Trust the remote-tracking refs already fetched instead of asking the remote.
    pub no_fetch: bool,
}

enum Remote {
    Pushed,
    /// On the remote, but only through another ref (`name`) than the manifest's branch —
    /// typically a branch rewritten after a backup tag was cut, or work left on a side branch.
    /// Fetchable, and still refused: the branch is meant to carry the work. `tip` as below.
    Elsewhere {
        name: String,
        tip: Option<String>,
    },
    /// Not on the remote at all; `tip` is the branch's head, when it could be read.
    NotPushed {
        tip: Option<String>,
    },
    Unknown(String),
}

struct Row {
    label: String,
    dep: Dep,
    rev: String,
    branch: String,
    repo: String,
    /// The tree that stands in for the dependency here, if there is one.
    tree: Option<PathBuf>,
    here: String,
    remote: Remote,
}

pub fn pins(root: &Path, opts: &Opts) -> Result<()> {
    let (text, what) = match &opts.at {
        Some(commit) => (
            git::out(root, &["show", &format!("{commit}:{MANIFEST}")])?,
            format!("{MANIFEST} at {commit}"),
        ),
        None => (
            std::fs::read_to_string(root.join(MANIFEST))?,
            MANIFEST.to_string(),
        ),
    };
    let manifest = Manifest::with_committed(root, &text, &what)?;

    let mut rows = Vec::new();
    for dep in manifest.deps().iter().filter(|d| d.is_fork_pin()) {
        let rev = dep.rev.clone().unwrap_or_default();
        // A dependency's own pins, as of the rev we pin it at (virglrs pins the C
        // virglrenderer): vendoring that rev fetches them, so they have to be pushed too. Listed
        // first, because they have to be pushed first.
        if git::is_tree(&dep.tree)
            && let Ok(nested) = git::out(&dep.tree, &["show", &format!("{rev}:{MANIFEST}")])
        {
            let nested = Manifest::parse(
                &dep.tree,
                &nested,
                &format!("{}'s {MANIFEST}", dep.name),
                None,
            )?;
            for n in nested.deps().iter().filter(|d| d.is_fork_pin()) {
                rows.push(row(&format!("{} › {}", dep.name, n.name), n, opts));
            }
        }
        rows.push(row(&dep.name, dep, opts));
    }

    print_table(&what, &rows);
    let unresolved = print_plan(&rows);
    if opts.check && unresolved > 0 {
        bail!("{unresolved} pin(s) are not known to be on their manifest branch");
    }
    Ok(())
}

fn row(label: &str, dep: &Dep, opts: &Opts) -> Row {
    let rev = dep.rev.clone().unwrap_or_default();
    let branch = dep.branch.clone().unwrap_or_default();
    let repo = dep.repo.clone().unwrap_or_default();
    let tree = git::is_tree(&dep.tree).then(|| dep.tree.clone());

    let here = match &tree {
        None => "no tree here".to_string(),
        Some(t) => relation(t, &rev),
    };
    let remote = match (&tree, opts.no_fetch) {
        (Some(t), true) => tracking_check(t, &repo, &branch, &rev),
        (Some(t), false) => fetch_check(t, &repo, &branch, &rev),
        (None, true) => Remote::Unknown("no tree here, and --no-fetch".into()),
        (None, false) => github_check(&repo, &branch, &rev),
    };
    Row {
        label: label.to_string(),
        dep: dep.clone(),
        rev,
        branch,
        repo,
        tree,
        here,
        remote,
    }
}

/// How the tree's HEAD relates to the pin. The pin is a claim; the HEAD is what builds.
fn relation(tree: &Path, rev: &str) -> String {
    let Ok(head) = git::out(tree, &["rev-parse", "HEAD"]) else {
        return "unreadable".to_string();
    };
    let dirty = if git::is_dirty(tree).unwrap_or(false) {
        ", dirty"
    } else {
        ""
    };
    if !git::has_commit(tree, rev) {
        return format!("{} (pin not in tree{dirty})", short(&head));
    }
    if head == rev {
        return format!("= pin{dirty}");
    }
    if git::is_ancestor(tree, rev, &head) {
        let n = git::out(tree, &["rev-list", "--count", &format!("{rev}..{head}")])
            .unwrap_or_else(|_| "?".into());
        return format!("{} = pin + {n}{dirty}", short(&head));
    }
    if git::is_ancestor(tree, &head, rev) {
        return format!("{} behind pin{dirty}", short(&head));
    }
    format!("{} DIVERGED from pin{dirty}", short(&head))
}

/// Ask the remote for the branch head and test ancestry locally. Fetching into FETCH_HEAD only:
/// no ref of the tree moves.
fn fetch_check(tree: &Path, repo: &str, branch: &str, rev: &str) -> Remote {
    if let Err(e) = git::out(
        tree,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            repo,
            &format!("refs/heads/{branch}"),
        ],
    ) {
        return Remote::Unknown(format!("fetch failed: {e}"));
    }
    let tip = git::out(tree, &["rev-parse", "FETCH_HEAD"]).ok();
    // A rev the fetch did not bring in is not on the branch, so has_commit failing is a "no".
    if git::has_commit(tree, rev) && git::is_ancestor(tree, rev, "FETCH_HEAD") {
        return Remote::Pushed;
    }
    match on_other_ref(tree, repo, rev) {
        Some(name) => Remote::Elsewhere { name, tip },
        None => Remote::NotPushed { tip },
    }
}

/// The first remote branch or tag that contains `rev`, asked only once the manifest's own
/// branch has said no. Tips this tree lacks are fetched by SHA into FETCH_HEAD alone, so no
/// local ref moves.
fn on_other_ref(tree: &Path, repo: &str, rev: &str) -> Option<String> {
    if !git::has_commit(tree, rev) {
        return None;
    }
    let listing = git::out(tree, &["ls-remote", "--heads", "--tags", repo]).ok()?;
    // `<sha>\t<ref>`, with `^{}` lines carrying the commit an annotated tag points at.
    let tips: Vec<(&str, &str)> = listing.lines().filter_map(|l| l.split_once('\t')).collect();
    let missing: Vec<&str> = tips
        .iter()
        .map(|(sha, _)| *sha)
        .filter(|sha| !git::has_commit(tree, sha))
        .collect();
    if !missing.is_empty() {
        let mut args = vec!["fetch", "--quiet", "--no-tags", repo];
        args.extend(&missing);
        // Best effort: a tag object that is not a commit fails has_commit and is refetched in
        // vain; whatever did arrive is still checked below.
        let _ = git::out(tree, &args);
    }
    tips.iter().find_map(|(sha, name)| {
        (git::has_commit(tree, sha) && git::is_ancestor(tree, rev, sha))
            .then(|| name.trim_end_matches("^{}").to_string())
    })
}

/// Offline: only the remote-tracking refs this tree already has. They go stale, so a "no" here
/// is "not seen", which `--check` still treats as unresolved.
fn tracking_check(tree: &Path, repo: &str, branch: &str, rev: &str) -> Remote {
    let refs = match git::out(tree, &["branch", "-r", "--contains", rev]) {
        Ok(s) => s,
        Err(e) => return Remote::Unknown(e.to_string()),
    };
    // Only the remotes that ARE the manifest's repo, whatever this clone named them: a `local`
    // remote (a source override's clone) or upstream is not where anyone fetches the pin from.
    // `x/HEAD -> x/main` is an alias, not a ref of its own.
    let same = |url: &str| url.trim_end_matches(".git") == repo.trim_end_matches(".git");
    let remotes: Vec<String> = git::out(tree, &["remote"])
        .unwrap_or_default()
        .lines()
        .filter(|name| git::out(tree, &["remote", "get-url", name]).is_ok_and(|url| same(&url)))
        .map(|name| format!("{name}/"))
        .collect();
    let refs: Vec<&str> = refs
        .lines()
        .map(str::trim)
        .filter(|r| !r.contains("->") && remotes.iter().any(|p| r.starts_with(p.as_str())))
        .collect();
    if refs.iter().any(|r| r.ends_with(&format!("/{branch}"))) {
        Remote::Pushed
    } else if let Some(other) = refs.first() {
        Remote::Elsewhere {
            name: other.to_string(),
            tip: None,
        }
    } else {
        Remote::Unknown("in no fetched remote ref (offline check)".into())
    }
}

/// No tree here to fetch into (a dependency vendored only on demand, like the kernel): ask
/// GitHub's compare API whether the branch contains the rev, without cloning anything.
fn github_check(repo: &str, branch: &str, rev: &str) -> Remote {
    let Some(slug) = repo
        .strip_prefix("https://github.com/")
        .map(|s| s.trim_end_matches(".git").trim_end_matches('/'))
    else {
        return Remote::Unknown(format!("no local tree, and {repo} is not on GitHub"));
    };
    let url = format!("https://api.github.com/repos/{slug}/compare/{branch}...{rev}");
    let out = Command::new("curl")
        .args([
            "-sS",
            "-H",
            "Accept: application/vnd.github+json",
            "-w",
            "\n%{http_code}",
            &url,
        ])
        .output();
    let Ok(out) = out else {
        return Remote::Unknown("curl failed to start".into());
    };
    let body = String::from_utf8_lossy(&out.stdout);
    let code = body.lines().last().unwrap_or("").trim().to_string();
    match code.as_str() {
        // compare base...head: "behind"/"identical" means the branch already contains the rev.
        "200" => match json_str(&body, "status").as_deref() {
            Some("behind") | Some("identical") => Remote::Pushed,
            // GitHub has the object, but objects outlive refs there (and forks share them), so
            // this says nothing about whether any ref still carries it.
            Some(_) => Remote::Unknown(format!(
                "on GitHub but not on {branch}; vendor it to see which ref holds it"
            )),
            None => Remote::Unknown("unreadable GitHub compare reply".into()),
        },
        // GitHub answers 404 for a rev it does not have at all.
        "404" => Remote::NotPushed { tip: None },
        _ => Remote::Unknown(format!("GitHub compare API answered {code}")),
    }
}

/// The string value of the first `"key": "value"` in a JSON body. Enough for one field.
fn json_str(body: &str, key: &str) -> Option<String> {
    let at = body.find(&format!("\"{key}\""))?;
    let rest = &body[at + key.len() + 2..];
    let rest = &rest[rest.find(':')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}

fn print_table(what: &str, rows: &[Row]) {
    println!("pins in {what}:");
    println!();
    let w = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    for r in rows {
        let over = match (&r.dep.source, &r.dep.checkout) {
            (_, Some(c)) => format!("  [checkout override: {}]", c.display()),
            (Some(s), _) => format!("  [source override: {}]", s.display()),
            _ => String::new(),
        };
        let remote = match &r.remote {
            Remote::Pushed => "pushed".to_string(),
            Remote::Elsewhere { name, .. } => format!("NOT on {} (only {name})", r.branch),
            Remote::NotPushed { .. } => "NOT PUSHED".to_string(),
            Remote::Unknown(why) => format!("UNKNOWN ({why})"),
        };
        println!(
            "  {:w$}  {}  {}  here: {}{over}",
            r.label,
            short(&r.rev),
            remote,
            r.here,
            w = w
        );
    }
    println!();
}

/// Print what publishing would take. Returns the number of pins the gate refuses: everything not
/// known to be on its manifest branch.
fn print_plan(rows: &[Row]) -> usize {
    let open: Vec<&Row> = rows
        .iter()
        .filter(|r| !matches!(r.remote, Remote::Pushed))
        .collect();
    if open.is_empty() {
        println!("every pin is on its remote branch.");
        return 0;
    }
    println!("to publish — forks first, in this order, then limina (by SHA, never a branch tip):");
    for r in &open {
        if let Remote::Unknown(why) = &r.remote {
            println!(
                "  ?? {}: could not tell ({why}) — re-run online, or check by hand",
                r.label
            );
            continue;
        }
        // Somewhere local that has the rev to push from: the tree, else the source override.
        let holder = r
            .tree
            .iter()
            .chain(r.dep.source.iter())
            .find(|p| git::is_tree(p) && git::has_commit(p, &r.rev));
        let Some(holder) = holder else {
            println!(
                "  !! {}: {} is in no local tree — it was committed somewhere else, or lost",
                r.label,
                short(&r.rev)
            );
            continue;
        };
        if let Remote::Elsewhere { name, .. } = &r.remote {
            println!(
                "  (note) {}: {} is on the remote only through {name} — it belongs on {}",
                r.label,
                short(&r.rev),
                r.branch
            );
        }
        let tip = match &r.remote {
            Remote::NotPushed { tip } | Remote::Elsewhere { tip, .. } => tip.as_deref(),
            _ => None,
        };
        match tip {
            Some(tip) if git::has_commit(holder, tip) && !git::is_ancestor(holder, tip, &r.rev) => {
                println!(
                    "  !! {}: {} does not descend from {}'s tip {} — publishing it REWRITES the \
                     branch: tag the old tip first, and force-push only with explicit authorization",
                    r.label,
                    short(&r.rev),
                    r.branch,
                    short(tip)
                );
            }
            _ => println!(
                "  git -C {} push {} {}:refs/heads/{}",
                holder.display(),
                r.repo,
                r.rev,
                r.branch
            ),
        }
    }
    println!("  …then push limina.");
    open.len()
}

#[cfg(test)]
mod tests {
    use super::json_str;

    #[test]
    fn reads_the_compare_status() {
        let body = "{\n  \"url\": \"x\",\n  \"status\": \"behind\",\n  \"ahead_by\": 0\n}\n200";
        assert_eq!(json_str(body, "status").as_deref(), Some("behind"));
        assert_eq!(json_str(body, "nope"), None);
    }
}
