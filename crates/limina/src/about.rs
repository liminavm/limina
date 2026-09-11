// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What this build IS — the About menu's contents.
//!
//! limina is not a wrapper over fixed dependencies: the behavior a user sees is as much
//! the pinned libkrun/virglrenderer/Mesa/kernel revisions as it is our own source. So a
//! bug report that names only "limina 0.1.0" is nearly useless, and the About menu names
//! the whole stack: our version and revision, when the binary was built, and the exact
//! fork revision of every dependency it was built against.
//!
//! The dependency revisions come from `third_party/manifest.toml` — the same committed
//! pins `cargo xtask vendor` checks out — baked in at compile time with `include_str!`,
//! so the list cannot drift from the tree that produced the binary and needs no file to
//! be found at runtime (a shipped `.app` has no repo).
//!
//! The presentation is the Limina menu's "About Limina" item, in both the VM window and
//! the control center: a modal dialog showing the whole stamp as fixed-pitch text, with a
//! button that copies it. What the dialog shows IS what the button copies — full hashes,
//! no abbreviation — so a screenshot and a paste say the same thing.

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSFont, NSPasteboard, NSPasteboardTypeString, NSTextField,
};
use objc2_foundation::NSString;

/// The pinned revision of one dependency fork, as `third_party/manifest.toml` records it.
pub(crate) struct Dep {
    /// The manifest's table name: `libkrun`, `virglrenderer`, `mesa-guest`, …
    pub(crate) name: String,
    /// The full pinned commit (or, for a heavy dep, whatever `rev` holds).
    pub(crate) rev: String,
    /// The fork branch our delta lives on.
    pub(crate) branch: Option<String>,
}

/// Everything the About menu shows.
pub(crate) struct BuildInfo {
    /// The workspace version (`CARGO_PKG_VERSION`).
    pub(crate) version: &'static str,
    /// The source revision this was built from, `-dirty` if the tree had uncommitted
    /// changes; `unknown` when built outside a git checkout (see `build.rs`).
    pub(crate) git_rev: &'static str,
    /// When the binary was built, UTC.
    pub(crate) built: &'static str,
    /// The dependency pins, alphabetically (the manifest's own order is not preserved by
    /// the TOML parser, and alphabetical is the stable, predictable one to read).
    pub(crate) deps: Vec<Dep>,
}

/// The manifest as it stood when this binary was compiled.
const MANIFEST: &str = include_str!("../../../third_party/manifest.toml");

pub(crate) fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_rev: env!("LIMINA_GIT_REV"),
        built: env!("LIMINA_BUILD_DATE"),
        deps: deps(MANIFEST),
    }
}

/// Every top-level table in the manifest that carries a `rev`. Written to tolerate a
/// malformed or reshaped manifest rather than panic: an About menu is a diagnostic, and
/// one that takes the app down with it is worse than one that comes up short.
fn deps(manifest: &str) -> Vec<Dep> {
    let Ok(table) = manifest.parse::<toml::Table>() else {
        return Vec::new();
    };
    table
        .iter()
        .filter_map(|(name, value)| {
            let entry = value.as_table()?;
            Some(Dep {
                name: name.clone(),
                rev: entry.get("rev")?.as_str()?.to_string(),
                branch: entry
                    .get("branch")
                    .and_then(|b| b.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

impl BuildInfo {
    /// The full text behind "Copy Build Info" — the menu rows abbreviate the revisions,
    /// and a bug report wants them whole.
    pub(crate) fn plain_text(&self) -> String {
        let mut out = format!(
            "limina {} ({})\nBuilt {}\n\nDependency pins (third_party/manifest.toml):\n",
            self.version, self.git_rev, self.built
        );
        let width = self.deps.iter().map(|d| d.name.len()).max().unwrap_or(0);
        for dep in &self.deps {
            let branch = dep
                .branch
                .as_deref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {:width$}  {}{}\n",
                dep.name,
                dep.rev,
                branch,
                width = width
            ));
        }
        out
    }
}

/// Show the About dialog: the build stamp as selectable fixed-pitch text, plus a button
/// that copies it.
///
/// Modal on the main thread. In the VM window that pauses the render timer for as long as
/// the dialog is up — the same deal as the close-policy dialog, and fine for the same
/// reason: the guest keeps running and frames resume on the next tick.
pub(crate) fn show(mtm: MainThreadMarker) {
    let text = build_info().plain_text();
    // Copying re-presents the dialog with the confirmation, so the click has a visible
    // answer instead of just dismissing.
    let mut copied = false;
    loop {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("About Limina"));
        if copied {
            alert.setInformativeText(&NSString::from_str("Copied to the clipboard."));
        }
        // Fixed pitch: the pins are a table of hashes, and a proportional font turns the
        // columns into noise. Selectable so a reader can grab one line without the button.
        let body = NSTextField::wrappingLabelWithString(&NSString::from_str(&text), mtm);
        if let Some(font) = NSFont::userFixedPitchFontOfSize(11.0) {
            body.setFont(Some(&font));
        }
        body.setSelectable(true);
        body.setPreferredMaxLayoutWidth(620.0);
        body.sizeToFit();
        alert.setAccessoryView(Some(&body));
        // The first button is the default (rightmost, Return): dismissing, not copying —
        // hitting Return on a dialog you opened to read should not touch the pasteboard.
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.addButtonWithTitle(&NSString::from_str("Copy"));
        if alert.runModal() != NSAlertSecondButtonReturn {
            return;
        }
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            pb.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
        }
        copied = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pins the shipped binary reports must be the ones the tree actually carries —
    /// the whole point of baking the manifest in is that the two cannot drift.
    #[test]
    fn reads_the_manifest_pins() {
        let info = build_info();
        let libkrun = info
            .deps
            .iter()
            .find(|d| d.name == "libkrun")
            .expect("the manifest pins libkrun");
        assert_eq!(libkrun.rev.len(), 40, "a full commit hash");
        assert_eq!(libkrun.branch.as_deref(), Some("limina"));
        // Every fork in the manifest shows up, none of them empty.
        assert!(info.deps.len() >= 6, "deps: {}", info.deps.len());
        assert!(info.deps.iter().all(|d| !d.rev.is_empty()));
    }

    /// The copyable text carries the FULL revisions (the menu rows are the abbreviated
    /// view) and names the build.
    #[test]
    fn plain_text_carries_full_revisions() {
        let info = build_info();
        let text = info.plain_text();
        assert!(text.starts_with("limina "));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        for dep in &info.deps {
            assert!(text.contains(&dep.rev), "missing {} in:\n{text}", dep.name);
        }
    }

    /// A manifest we cannot parse (or one reshaped out from under us) yields no rows
    /// rather than a panic — About is a diagnostic, not a reason to lose the app.
    #[test]
    fn malformed_manifest_yields_no_rows() {
        assert!(deps("this is not toml {{{").is_empty());
        assert!(deps("[libkrun]\nbranch = \"limina\"\n").is_empty());
    }
}
