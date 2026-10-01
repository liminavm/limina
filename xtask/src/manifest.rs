// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The dependency pins: `third_party/manifest.toml` (committed — what a limina commit says it
//! builds) merged with `third_party/manifest.local.toml` (per-checkout overrides, never
//! committed).
//!
//! An override names a local tree, never a revision:
//! - `source = "<path>"` fetches the PINNED rev from a local repository instead of the network;
//!   what gets built is still exactly the pin.
//! - `checkout = "<path>"` makes that working tree the dependency, as it stands; its HEAD is
//!   what gets built. For a `third_party/` dependency, `vendor` materializes it as a symlink.
//!
//! `scripts/lib/manifest.sh` is the shell half and implements the same rules.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const MANIFEST: &str = "third_party/manifest.toml";
pub const MANIFEST_LOCAL: &str = "third_party/manifest.local.toml";

/// One dependency, with any local override already applied.
#[derive(Clone, Debug)]
pub struct Dep {
    pub name: String,
    pub repo: Option<String>,
    /// The project this is a fork of, added as a second remote. `None` for a repository that is
    /// ours outright and forks nothing.
    pub upstream: Option<String>,
    pub branch: Option<String>,
    pub rev: Option<String>,
    /// Multi-GB tree this host never builds: skipped by `vendor` unless `--heavy`, and cloned
    /// blobless when it is materialized.
    pub heavy: bool,
    /// The local tree that IS this dependency here: a `checkout` override, else the manifest's
    /// `tree`, else `third_party/<name>`.
    pub tree: PathBuf,
    pub source: Option<PathBuf>,
    pub checkout: Option<PathBuf>,
}

impl Dep {
    /// A fork we pin and push: it has the three fields a push needs. Pins with no branch
    /// (libclc's bottle, virglrs's upstream reference leg) are someone else's to publish.
    pub fn is_fork_pin(&self) -> bool {
        self.repo.is_some() && self.branch.is_some() && self.rev.is_some()
    }

    pub fn require(&self, field: &str, value: &Option<String>) -> Result<String> {
        value
            .clone()
            .with_context(|| format!("manifest [{}] missing `{field}`", self.name))
    }

    /// Where to fetch the pinned rev from when it is not already local: the override, if any.
    pub fn local_repo(&self) -> Option<&Path> {
        self.source.as_deref().or(self.checkout.as_deref())
    }
}

pub struct Manifest {
    deps: Vec<Dep>,
}

impl Manifest {
    /// The working tree's manifest plus its local overrides.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::with_committed(root, &text, MANIFEST)
    }

    /// A given committed manifest (say, one read out of another commit) plus THIS checkout's
    /// local overrides, which say where its trees are.
    pub fn with_committed(root: &Path, committed: &str, what: &str) -> Result<Self> {
        let local_path = root.join(MANIFEST_LOCAL);
        let local = match std::fs::read_to_string(&local_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", local_path.display())),
        };
        Self::parse(root, committed, what, local.as_deref())
    }

    pub fn parse(root: &Path, committed: &str, what: &str, local: Option<&str>) -> Result<Self> {
        let committed: toml::Table = committed
            .parse()
            .with_context(|| format!("parsing {what}"))?;
        let local: toml::Table = match local {
            Some(text) => text
                .parse()
                .with_context(|| format!("parsing {MANIFEST_LOCAL}"))?,
            None => toml::Table::new(),
        };

        // Reject what would otherwise be silently ignored: a misspelt dependency or key is an
        // override that does nothing, and the build then fetches from the network as if none
        // had been asked for.
        for (name, entry) in &local {
            let Some(entry) = entry.as_table() else {
                bail!("{MANIFEST_LOCAL}: `{name}` must be a table ([{name}])");
            };
            if !committed.contains_key(name) {
                bail!("{MANIFEST_LOCAL}: [{name}] names no dependency in {what}");
            }
            for (key, value) in entry {
                if key != "source" && key != "checkout" {
                    bail!(
                        "{MANIFEST_LOCAL}: [{name}] `{key}` — only `source` and `checkout` exist"
                    );
                }
                if !value.is_str() {
                    bail!("{MANIFEST_LOCAL}: [{name}] `{key}` must be a path string");
                }
            }
            if entry.contains_key("source") && entry.contains_key("checkout") {
                bail!(
                    "{MANIFEST_LOCAL}: [{name}] sets both `source` and `checkout` — a checkout is \
                     built as it stands, so a source for it would mean nothing"
                );
            }
        }

        let mut deps = Vec::new();
        for (name, entry) in &committed {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            let s = |table: &toml::Table, key: &str| {
                table.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            let over = local.get(name).and_then(|v| v.as_table());
            let source = over.and_then(|o| s(o, "source")).map(|p| anchor(root, &p));
            let checkout = over
                .and_then(|o| s(o, "checkout"))
                .map(|p| anchor(root, &p));
            let tree = checkout
                .clone()
                .or_else(|| s(entry, "tree").map(|p| anchor(root, &p)))
                .unwrap_or_else(|| root.join("third_party").join(name));
            deps.push(Dep {
                name: name.clone(),
                repo: s(entry, "repo"),
                upstream: s(entry, "upstream"),
                branch: s(entry, "branch"),
                rev: s(entry, "rev"),
                heavy: entry
                    .get("heavy")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                tree,
                source,
                checkout,
            });
        }
        Ok(Self { deps })
    }

    pub fn dep(&self, name: &str) -> Result<&Dep> {
        self.deps
            .iter()
            .find(|d| d.name == name)
            .with_context(|| format!("manifest has no [{name}] section"))
    }

    pub fn deps(&self) -> &[Dep] {
        &self.deps
    }
}

/// `~/` expands to the home directory; a relative path is anchored at the limina checkout.
fn anchor(root: &Path, path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED: &str = r#"
[libkrun]
repo = "https://example.invalid/libkrun.git"
branch = "limina"
rev = "aaaa"

[kosmickrisp]
repo = "https://example.invalid/mesa.git"
branch = "limina-kk"
rev = "bbbb"
tree = "/Volumes/mesa-cs/mesa"

[libclc]
version = "22.1.3"
"#;

    fn parse(local: Option<&str>) -> Result<Manifest> {
        Manifest::parse(Path::new("/repo"), COMMITTED, "test", local)
    }

    #[test]
    fn no_overrides() {
        let m = parse(None).unwrap();
        let k = m.dep("libkrun").unwrap();
        assert_eq!(k.tree, Path::new("/repo/third_party/libkrun"));
        assert!(k.source.is_none() && k.checkout.is_none());
        assert_eq!(
            m.dep("kosmickrisp").unwrap().tree,
            Path::new("/Volumes/mesa-cs/mesa")
        );
        assert!(m.dep("libkrun").unwrap().is_fork_pin());
        assert!(!m.dep("libclc").unwrap().is_fork_pin());
    }

    #[test]
    fn overrides_never_change_the_pin() {
        let m = parse(Some(
            "[libkrun]\nsource = \"/src/libkrun\"\n[kosmickrisp]\ncheckout = \"mesa\"\n",
        ))
        .unwrap();
        let k = m.dep("libkrun").unwrap();
        assert_eq!(k.rev.as_deref(), Some("aaaa"));
        assert_eq!(k.source.as_deref(), Some(Path::new("/src/libkrun")));
        assert_eq!(k.tree, Path::new("/repo/third_party/libkrun"));
        let kk = m.dep("kosmickrisp").unwrap();
        assert_eq!(kk.rev.as_deref(), Some("bbbb"));
        assert_eq!(
            kk.tree,
            Path::new("/repo/mesa"),
            "checkout replaces the tree"
        );
        assert_eq!(kk.local_repo(), Some(Path::new("/repo/mesa")));
    }

    #[test]
    fn rejects_what_would_be_ignored() {
        for (local, why) in [
            ("[libkrn]\nsource = \"/x\"\n", "misspelt dependency"),
            ("[libkrun]\nsrc = \"/x\"\n", "misspelt key"),
            ("[libkrun]\nrev = \"cccc\"\n", "a rev override"),
            ("[libkrun]\nsource = 1\n", "not a path"),
            ("libkrun = \"/x\"\n", "not a table"),
            (
                "[libkrun]\nsource = \"/x\"\ncheckout = \"/y\"\n",
                "both kinds",
            ),
        ] {
            assert!(parse(Some(local)).is_err(), "should reject {why}");
        }
    }
}
