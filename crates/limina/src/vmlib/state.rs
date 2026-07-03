// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `state.toml` — mutable per-VM machine state, kept OUT of the user-editable
//! `vm.toml` (a window drag must never rewrite config, and a state save must never
//! race a user's editor). Lives at the bundle root so it travels with a
//! Finder-copied bundle, unlike NSUserDefaults frame autosave. Deliberately
//! disposable: missing or corrupt state is `None`, never an error — deleting the
//! file just forgets the window placement.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct VmState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// NSWindow frame in screen points, Cocoa bottom-left origin: `[x, y, w, h]`.
    pub frame: [f64; 4],
    /// Content size in points — the guest resolution dynamic mode boots back into.
    /// Stored directly so restore needs no style-mask metrics math.
    pub content: (u32, u32),
}

/// Load the state file. Missing, unreadable, or corrupt → `None` (state is
/// disposable; the caller falls back to first-boot behavior).
pub fn load(path: &Path) -> Option<VmState> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Atomic save (tmp + rename, the `VmBundle::save` pattern). Best-effort at the
/// call sites — losing a window-placement save is not worth failing anything.
pub fn save(path: &Path, state: &VmState) -> std::io::Result<()> {
    let text = format!(
        "# limina per-VM machine state — safe to delete.\n{}",
        toml::to_string_pretty(state).map_err(std::io::Error::other)?
    );
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "limina-state-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_and_cleans_the_tmp_file() {
        let dir = scratch("roundtrip");
        let path = dir.join("state.toml");
        let state = VmState {
            window: Some(WindowState {
                frame: [100.0, 200.0, 1280.0, 828.0],
                content: (1280, 800),
            }),
        };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), Some(state));
        assert!(!dir.join("state.toml.tmp").exists(), "tmp file cleaned");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_corrupt_is_none() {
        let dir = scratch("corrupt");
        assert_eq!(load(&dir.join("state.toml")), None, "missing file");
        let path = dir.join("state.toml");
        std::fs::write(&path, "not [valid toml").unwrap();
        assert_eq!(load(&path), None, "corrupt file");
        // An empty-but-valid file is a state with no window section.
        std::fs::write(&path, "").unwrap();
        assert_eq!(load(&path), Some(VmState::default()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
