// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Per-VM template store for the impersonated MOC fingerprint reader (M14 wave 3).
//!
//! A real match-on-chip reader stores fingerprint templates on the device; ours stores only the
//! libfprint-authored **`user_id` string** (`FP1-<date>-<finger>-<user>`) that stands in for a
//! template — the guest's `elanmoc` driver hands it to us at enroll and expects it back at verify,
//! comparing bytes (see `docs/design/usb-moc-fingerprint.md` §1.2). We never see or store a real
//! fingerprint; a "match" is a host Touch ID prompt.
//!
//! **Single logical finger** (the design decision): the enclave only ever reports *that* a trusted
//! finger matched, never which, so one credential is the honest model. The store therefore holds at
//! most one `user_id` (occupying device slot 0); a second distinct enrollment is refused at the
//! protocol layer by reporting the reader "full" (see `moc::Engine`). Persisted to a JSON file so
//! the enrolled finger survives a VM restart — the same bytes fprintd keeps in its own on-disk
//! print, so the two stay consistent across reboots.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The persisted form: the one enrolled `user_id`, or none.
#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    /// The single enrolled slot's `user_id` bytes (device slot 0), if any.
    user_id: Option<Vec<u8>>,
}

/// The single-finger `user_id` store. Shared (`Arc`) across the MOC serve thread and any future
/// peer; internally synchronized.
pub struct MocStore {
    inner: Mutex<Persisted>,
    path: Option<PathBuf>,
}

impl MocStore {
    /// An in-memory store (no persistence) — used by tests and by CI without a bundle dir.
    pub fn in_memory() -> MocStore {
        MocStore {
            inner: Mutex::new(Persisted::default()),
            path: None,
        }
    }

    /// Load from `path` (missing/corrupt → empty, non-fatal) and persist to it.
    pub fn with_path(path: PathBuf) -> MocStore {
        let inner = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
            .unwrap_or_default();
        MocStore {
            inner: Mutex::new(inner),
            path: Some(path),
        }
    }

    /// Is a finger enrolled (slot 0 occupied)?
    pub fn is_enrolled(&self) -> bool {
        self.inner.lock().unwrap().user_id.is_some()
    }

    /// The enrolled `user_id`, if any (returned verbatim to the guest at verify/list).
    pub fn user_id(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().user_id.clone()
    }

    /// Store the enrolled finger's `user_id` (enroll commit). Replaces any prior value — the
    /// protocol layer refuses a second *distinct* enroll before commit, so this only lands for a
    /// first enroll or a delete-then-re-enroll.
    pub fn set(&self, user_id: Vec<u8>) {
        let mut g = self.inner.lock().unwrap();
        g.user_id = Some(user_id);
        self.persist(&g);
    }

    /// Delete the enrolled finger if its `user_id` matches `user_id` (the guest's delete carries
    /// the print's stored id). Returns whether a finger was removed.
    pub fn delete_matching(&self, user_id: &[u8]) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.user_id.as_deref() == Some(user_id) {
            g.user_id = None;
            self.persist(&g);
            true
        } else {
            false
        }
    }

    fn persist(&self, p: &Persisted) {
        if let Some(path) = &self.path {
            if let Ok(json) = serde_json::to_vec_pretty(p) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enroll_read_delete_roundtrip() {
        let s = MocStore::in_memory();
        assert!(!s.is_enrolled());
        assert_eq!(s.user_id(), None);

        let uid = b"FP1-20260724-5-abcd1234-alice".to_vec();
        s.set(uid.clone());
        assert!(s.is_enrolled());
        assert_eq!(s.user_id(), Some(uid.clone()));

        // A delete with a non-matching id leaves the finger in place.
        assert!(!s.delete_matching(b"FP1-other"));
        assert!(s.is_enrolled());
        // A matching delete clears it.
        assert!(s.delete_matching(&uid));
        assert!(!s.is_enrolled());
    }

    #[test]
    fn persists_across_reload() {
        let dir = std::env::temp_dir().join(format!("moc-store-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("moc-templates.json");
        let uid = b"FP1-20260724-1-deadbeef-bob".to_vec();
        {
            let s = MocStore::with_path(path.clone());
            s.set(uid.clone());
        }
        let reloaded = MocStore::with_path(path.clone());
        assert_eq!(reloaded.user_id(), Some(uid));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
