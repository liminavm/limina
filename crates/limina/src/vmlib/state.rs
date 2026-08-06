// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `state.toml` — mutable per-VM machine state, kept OUT of the user-editable
//! `vm.toml` (a window drag must never rewrite config, and a state save must never
//! race a user's editor). Lives at the bundle root so it travels with a
//! Finder-copied bundle, unlike NSUserDefaults frame autosave. Deliberately
//! disposable: missing or corrupt state is `None`, never an error — deleting the
//! file just forgets the window placement.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VmState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
    /// Present iff the VM was suspended (M9.2): the guest was snapshotted to `snapshot` and the
    /// process torn down. STATUS ONLY (what `limina ls` / the control center show): the resume
    /// decision is made from the snapshot file's presence at worker spawn, not from this record
    /// (`supervisor::take_pending_resume`, which also clears this when it consumes the snapshot —
    /// or reconciles a stale record whose snapshot is gone). Absent for a normally-stopped or
    /// running VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended: Option<Suspended>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// NSWindow frame in screen points, Cocoa bottom-left origin: `[x, y, w, h]`.
    ///
    /// Always the last **windowed** frame, even when the VM exits fullscreen-mode: it is what
    /// leaving fullscreen restores to, so a fullscreen exit must not overwrite it with a
    /// screen-sized rectangle.
    pub frame: [f64; 4],
    /// Content size in points — the guest resolution dynamic mode boots back into.
    /// Stored directly so restore needs no style-mask metrics math.
    pub content: (u32, u32),
    /// The VM was **fullscreen** when it last stopped (or was suspended), and should come back
    /// that way. Separate from [`Self::frame`] because the two answer different questions:
    /// where the window goes, and whether it then goes fullscreen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fullscreen: bool,
    /// Stable identity of the display it was fullscreen on — `hostdisplay`'s vendor/model/serial
    /// hash, **not** a `CGDirectDisplayID`. Display ids are reassigned across reboots and
    /// hotplugs, which is the exact situation this record exists to survive.
    ///
    /// Absent (or no longer attached) restores fullscreen on the main display: "was fullscreen"
    /// is the stronger memory and the display is best-effort, so undocking never silently leaves
    /// the VM windowed.
    ///
    /// Persisted through i64 (`u64_bits_as_i64`): TOML integers are i64 and the identity key is
    /// a full-width hash, so a key with bit 63 set would otherwise make the whole state save
    /// fail — fullscreen placement silently never persisted on affected displays.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "u64_bits_as_i64"
    )]
    pub fullscreen_display: Option<u64>,
}

/// Bit-preserving u64 ↔ i64 bridge for TOML, which has no unsigned integers. Keys already
/// saved by older builds were necessarily ≤ `i64::MAX` and load unchanged.
mod u64_bits_as_i64 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        v.map(|k| k as i64).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Ok(Option::<i64>::deserialize(d)?.map(|k| k as u64))
    }
}

/// A durable "this VM is suspended" record (M9.2). See [`VmState::suspended`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suspended {
    /// The snapshot file the worker wrote (the bundle's `run/snapshot.bin`).
    pub snapshot: PathBuf,
}

/// Load the state file. Missing, unreadable, or corrupt → `None` (state is
/// disposable; the caller falls back to first-boot behavior).
pub fn load(path: &Path) -> Option<VmState> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Load-modify-save the `[suspended]` record while preserving every other field (window
/// placement). Passing `Some(..)` marks the VM suspended (worker exited 126); `None` clears it
/// (consumed on restore, or a normal stop). A missing/corrupt file starts from `VmState::default`.
pub fn set_suspended(path: &Path, suspended: Option<Suspended>) -> std::io::Result<()> {
    let mut state = load(path).unwrap_or_default();
    state.suspended = suspended;
    save(path, &state)
}

/// Load-modify-save the `[window]` record while preserving every other field. The window
/// savers MUST use this, never a whole-`VmState` [`save`]: a windowed suspend writes
/// `[suspended]` from the session monitor thread moments before the window's final state
/// save runs, and a full save with `suspended: None` would silently consume the record —
/// the VM would cold-boot instead of resuming (M9.4).
pub fn set_window(path: &Path, window: Option<WindowState>) -> std::io::Result<()> {
    let mut state = load(path).unwrap_or_default();
    state.window = window;
    save(path, &state)
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
                fullscreen: true,
                fullscreen_display: Some(0x1234_5678_9abc_def0),
            }),
            suspended: None,
        };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), Some(state));
        assert!(!dir.join("state.toml.tmp").exists(), "tmp file cleaned");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trips_a_top_bit_identity_key() {
        // The display identity_key is an FNV-1a-derived u64 — bit 63 is a coin flip. TOML
        // integers are i64, so a key > i64::MAX must still save (bit-preserved through i64),
        // or fullscreen placement silently never persists on affected displays.
        let dir = scratch("topbit");
        let path = dir.join("state.toml");
        let state = VmState {
            window: Some(WindowState {
                frame: [0.0, 0.0, 1512.0, 982.0],
                content: (1512, 982),
                fullscreen: true,
                fullscreen_display: Some(0x9abc_def0_1234_5678),
            }),
            suspended: None,
        };
        save(&path, &state).expect("a top-bit identity key must be persistable");
        assert_eq!(load(&path), Some(state));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_dogfood_record_parses_with_its_identity_key_intact() {
        // Verbatim from a dogfood state.toml, from a session where a fullscreen VM kept
        // restoring on the internal display. The suspicion was another silent type
        // mis-serialization (the frame/content class); it was not — the key survives byte for
        // byte and decomposes into a sane panel (serial 0x3704f790, product 0xf13b, 60 Hz), so
        // the fault was downstream, in which SCREEN the window was then placed on. Keep this
        // pinned: it is cheap, and it stops the next reader from re-litigating the file format.
        let dir = scratch("dogfood");
        let path = dir.join("state.toml");
        std::fs::write(
            &path,
            "# limina per-VM machine state — safe to delete.\n\
             [window]\n\
             frame = [\n    3840.0,\n    919.0,\n    2048.0,\n    1286.0,\n]\n\
             content = [\n    2048,\n    1286,\n]\n\
             fullscreen = true\n\
             fullscreen_display = 3964565773887406140\n",
        )
        .unwrap();
        let win = load(&path).expect("the record parses").window.unwrap();
        assert_eq!(win.frame, [3840.0, 919.0, 2048.0, 1286.0]);
        assert_eq!(win.content, (2048, 1286));
        assert!(win.fullscreen);
        let key = win.fullscreen_display.expect("identity key preserved");
        assert_eq!(key, 3_964_565_773_887_406_140);
        // The identity_key layout: serial << 32 | product << 16 | refresh.
        assert_eq!(key & 0xffff, 60, "60 Hz panel");
        assert_eq!((key >> 16) & 0xffff, 0xf13b, "product code");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_suspended_preserves_window_and_round_trips() {
        let dir = scratch("suspended");
        let path = dir.join("state.toml");
        let win = WindowState {
            frame: [10.0, 20.0, 640.0, 480.0],
            content: (640, 480),
            fullscreen: false,
            fullscreen_display: None,
        };
        save(
            &path,
            &VmState {
                window: Some(win),
                suspended: None,
            },
        )
        .unwrap();
        // Mark suspended — the window placement must survive.
        let snap = std::path::PathBuf::from("/tmp/snap.bin");
        set_suspended(
            &path,
            Some(Suspended {
                snapshot: snap.clone(),
            }),
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.window, Some(win), "window preserved across suspend");
        assert_eq!(loaded.suspended, Some(Suspended { snapshot: snap }));
        // Clear it — window still preserved.
        set_suspended(&path, None).unwrap();
        let cleared = load(&path).unwrap();
        assert_eq!(cleared.window, Some(win));
        assert_eq!(cleared.suspended, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_window_preserves_suspended() {
        // The windowed-suspend ordering: the monitor thread persists [suspended], then the
        // window's final state save runs. It must MERGE, not clobber (the M9.4-1a bug class).
        let dir = scratch("setwindow");
        let path = dir.join("state.toml");
        let snap = std::path::PathBuf::from("/tmp/snap.bin");
        set_suspended(
            &path,
            Some(Suspended {
                snapshot: snap.clone(),
            }),
        )
        .unwrap();
        let win = WindowState {
            frame: [1.0, 2.0, 800.0, 600.0],
            content: (800, 600),
            fullscreen: true,
            fullscreen_display: None,
        };
        set_window(&path, Some(win)).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.suspended,
            Some(Suspended { snapshot: snap }),
            "suspend record survives a window save"
        );
        assert_eq!(loaded.window, Some(win));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_state_file_written_before_fullscreen_memory_still_loads() {
        // Every existing managed VM has one of these on disk. The new fields must default rather
        // than make the whole record unreadable — losing it would silently reset the window
        // placement of every VM on the machine at the first launch after an upgrade.
        let dir = scratch("compat");
        let path = dir.join("state.toml");
        std::fs::write(
            &path,
            "[window]\nframe = [10.0, 20.0, 1280.0, 828.0]\ncontent = [1280, 800]\n",
        )
        .unwrap();
        let win = load(&path).unwrap().window.unwrap();
        assert_eq!(win.frame, [10.0, 20.0, 1280.0, 828.0]);
        assert_eq!(win.content, (1280, 800));
        assert!(!win.fullscreen, "absent means windowed");
        assert_eq!(win.fullscreen_display, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_windowed_vm_writes_no_fullscreen_keys_at_all() {
        // `skip_serializing_if` keeps the common record identical to what shipped before, so a
        // downgrade reads it too and nobody is puzzled by `fullscreen = false` in every file.
        let dir = scratch("skip");
        let path = dir.join("state.toml");
        set_window(
            &path,
            Some(WindowState {
                frame: [0.0, 0.0, 800.0, 600.0],
                content: (800, 600),
                fullscreen: false,
                fullscreen_display: None,
            }),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("fullscreen"), "got: {text}");
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
