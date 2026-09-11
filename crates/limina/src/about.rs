// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Marcelo Jorge Vieira

//! What this build IS — the About menu's contents.
//!
//! limina is not a wrapper over fixed dependencies: the behavior a user sees is as much the
//! libkrun/virglrs/KosmicKrisp revisions it was built from as it is our own source. So a bug
//! report that names only "limina 0.1.0" is nearly useless, and About names the whole host
//! stack: our version and revision, when the binary was built, and the revision of every
//! dependency it carries.
//!
//! **The pin is a claim; the checkout HEAD is the fact.** `third_party/manifest.toml` records
//! what a tree *should* be on, and the two have disagreed repeatedly in practice — the reason
//! `scripts/park-bundle.sh` reads `git rev-parse HEAD` in every tree and never the manifest.
//! About does the same: `build.rs` asks each tree that this build actually consumes, and only
//! a dependency with no tree to ask (edk2, whose firmware is built from its pin inside a
//! container) falls back to the pin — and says so on its row, rather than passing a claim off
//! as an observation.
//!
//! The list is scoped to what this binary and its bundle carry: libkrun, virglrs and imago are
//! compiled in, KosmicKrisp ships as dylibs and edk2 as the firmware. The guest's own kernel
//! and mesa are whatever was last delivered into a guest image — nothing to do with this build
//! — so they are not here; `docs/images.md` is where the guest side is recorded.
//!
//! The presentation is the Limina menu's "About Limina" item, in both the VM window and the
//! control center: a modal dialog showing the whole stamp as fixed-pitch text, with a button
//! that copies it. What the dialog shows IS what the button copies — full hashes either way —
//! so a screenshot and a paste say the same thing.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSAlert, NSAlertSecondButtonReturn, NSFont, NSMenuItem, NSTextField};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

/// Where a dependency's revision came from.
#[derive(Debug, PartialEq)]
pub(crate) enum Source {
    /// Read out of the tree that was compiled in or shipped: an observation.
    Head,
    /// `third_party/manifest.toml`, because there was no tree on the build host to read.
    Pin,
}

/// One dependency's revision, as `build.rs` resolved it at compile time.
pub(crate) struct Dep {
    /// The manifest's table name: `libkrun`, `virglrs`, `kosmickrisp`, …
    pub(crate) name: String,
    /// The full commit the build carries.
    pub(crate) rev: String,
    /// The branch the checkout was on (or the manifest's, for a pinned row).
    pub(crate) branch: Option<String>,
    pub(crate) source: Source,
}

/// Everything About shows.
pub(crate) struct BuildInfo {
    /// The workspace version (`CARGO_PKG_VERSION`).
    pub(crate) version: &'static str,
    /// The source revision this was built from; `unknown` when built outside a git checkout.
    /// No dirty flag — `build.rs` explains why it would be a lie in both directions.
    pub(crate) git_rev: &'static str,
    /// When the binary was built, UTC.
    pub(crate) built: &'static str,
    /// The dependency revisions, in the order `build.rs` lists the host stack.
    pub(crate) deps: Vec<Dep>,
}

/// The dependency stamp `build.rs` baked in (see [`deps`] for the shape).
const DEPS: &str = env!("LIMINA_DEPS");

pub(crate) fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_rev: env!("LIMINA_GIT_REV"),
        built: env!("LIMINA_BUILD_DATE"),
        deps: deps(DEPS),
    }
}

/// Parse the stamp: `;`-separated records of `name|rev|branch|head-or-pin`.
///
/// Written to tolerate a stamp it does not understand rather than panic — About is a
/// diagnostic, and one that takes the app down with it is worse than one that comes up short.
fn deps(stamp: &str) -> Vec<Dep> {
    stamp
        .split(';')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let mut fields = record.split('|');
            let (name, rev, branch, source) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            if name.is_empty() || rev.is_empty() {
                return None;
            }
            Some(Dep {
                name: name.to_string(),
                rev: rev.to_string(),
                branch: (!branch.is_empty()).then(|| branch.to_string()),
                source: match source {
                    "head" => Source::Head,
                    "pin" => Source::Pin,
                    _ => return None,
                },
            })
        })
        .collect()
}

impl BuildInfo {
    /// The whole stamp as text: what the dialog shows, and what its Copy button puts on the
    /// pasteboard — the two are the same string, so a screenshot and a paste cannot disagree.
    pub(crate) fn plain_text(&self) -> String {
        let mut out = format!(
            "limina {} ({})\nBuilt {}\n\nDependency revisions:\n",
            self.version, self.git_rev, self.built
        );
        let width = self.deps.iter().map(|d| d.name.len()).max().unwrap_or(0);
        for dep in &self.deps {
            let branch = dep
                .branch
                .as_deref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            // A pinned row is a claim about a tree this host never saw. Say which rows those
            // are on the row itself: a bug report is read by someone who was not here.
            let pinned = match dep.source {
                Source::Head => "",
                Source::Pin => "  [manifest pin, not built from a checkout here]",
            };
            out.push_str(&format!(
                "  {:width$}  {}{}{}\n",
                dep.name,
                dep.rev,
                branch,
                pinned,
                width = width
            ));
        }
        out
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; `showAbout:` matches
    // AppKit's target/action convention.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaAboutMenuTarget"]
    struct AboutMenuTarget;

    unsafe impl NSObjectProtocol for AboutMenuTarget {}

    impl AboutMenuTarget {
        #[unsafe(method(showAbout:))]
        fn show_about(&self, _sender: &NSMenuItem) {
            show(self.mtm());
        }
    }
);

/// The "About Limina" menu item, wired to its own target.
///
/// Both menu bars — the VM window's and the control center's — take theirs from here. About
/// is a global verb (a build stamp is the same wherever it is read from), so neither window
/// has any business borrowing the other's action class to route it.
///
/// NSMenuItem holds its target weakly, so the target is leaked deliberately: there is one per
/// menu bar, both live as long as the process, and an item whose target has been freed is a
/// dead item.
pub(crate) fn menu_item(mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    let target: Retained<AboutMenuTarget> = unsafe { msg_send![AboutMenuTarget::alloc(mtm), init] };
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("About Limina"),
            Some(sel!(showAbout:)),
            &NSString::from_str(""),
        )
    };
    unsafe { item.setTarget(Some(&*target)) };
    std::mem::forget(target);
    item
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
        // Fixed pitch: the revisions are a table of hashes, and a proportional font turns the
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
        crate::clipboard::copy_to_pasteboard(&text);
        copied = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host stack this build carries is all there, with whole hashes — a truncated or
    /// missing row is exactly the thing that makes a bug report unanswerable.
    #[test]
    fn stamps_the_whole_host_stack() {
        let info = build_info();
        let names: Vec<&str> = info.deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["libkrun", "virglrs", "imago", "kosmickrisp", "edk2"],
            "the host stack, in build.rs's order"
        );
        for dep in &info.deps {
            assert_eq!(dep.rev.len(), 40, "{}: a full commit hash", dep.name);
            assert!(
                dep.rev.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: a hex revision, got {}",
                dep.name,
                dep.rev
            );
        }
    }

    /// The guest's own components are NOT part of this build — the guest runs whatever was
    /// last delivered into it — so naming them here would describe a stack nobody is running.
    #[test]
    fn leaves_the_guest_side_out() {
        let info = build_info();
        assert!(
            !info
                .deps
                .iter()
                .any(|d| d.name == "mesa-guest" || d.name == "linux"),
            "guest components in the host build stamp"
        );
    }

    /// The copied text carries the full revisions and names the build.
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

    /// A pinned row says so: the reader of a bug report cannot otherwise tell an observed
    /// revision from a manifest claim.
    #[test]
    fn a_pinned_row_is_marked_as_one() {
        let deps = deps("libkrun|aaaa|limina|head;edk2|bbbb|limina|pin");
        assert_eq!(deps[0].source, Source::Head);
        assert_eq!(deps[1].source, Source::Pin);
        let text = BuildInfo {
            version: "0.0.0",
            git_rev: "0000",
            built: "never",
            deps,
        }
        .plain_text();
        assert!(
            !text.contains("aaaa (limina)  [manifest"),
            "head row marked:\n{text}"
        );
        assert!(
            text.contains("bbbb (limina)  [manifest pin"),
            "pin row unmarked:\n{text}"
        );
    }

    /// A stamp we cannot parse yields no rows rather than a panic — About is a diagnostic,
    /// not a reason to lose the app.
    #[test]
    fn malformed_stamp_yields_no_rows() {
        assert!(deps("").is_empty());
        assert!(deps("libkrun").is_empty());
        assert!(deps("libkrun|deadbeef|limina").is_empty());
        assert!(deps("libkrun|deadbeef|limina|somewhere-else").is_empty());
        assert!(deps("libkrun||limina|head").is_empty());
    }
}
