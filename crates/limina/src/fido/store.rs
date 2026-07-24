// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Per-VM credential store for the virtual FIDO authenticator (M14).
//!
//! Holds one record per registered passkey: the CTAP credential id, the relying
//! party it belongs to, the user handle/name (for resident-key assertions), the
//! Secure-Enclave key blob (the private key stays in the enclave — this is only
//! the enclave-encrypted handle, Spike A), and the signature counter.
//!
//! Shared across all control-plane peers of one VM (an `Arc` in the supervisor's
//! `Inner`), so a guest that reconnects — or a second agent connection — sees the
//! same passkeys. Optionally persisted to a JSON file (`LIMINA_FIDO_STORE`, later
//! the per-VM state dir) so credentials survive a VM restart; without a path it is
//! in-memory only.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One registered passkey.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credential {
    /// CTAP credential id (the RP hands this back in an allow-list assertion).
    pub cred_id: Vec<u8>,
    pub rp_id: String,
    /// WebAuthn user handle (opaque; returned in resident-key assertions).
    pub user_handle: Vec<u8>,
    pub user_name: String,
    /// The Secure-Enclave key blob (`sep::SepKey::blob`).
    pub blob: Vec<u8>,
    /// Monotonic per-credential signature counter (WebAuthn clone-detection).
    pub sign_count: u32,
}

pub struct FidoStore {
    inner: Mutex<Vec<Credential>>,
    path: Option<PathBuf>,
}

impl FidoStore {
    /// A store that persists to `LIMINA_FIDO_STORE` if set, else in-memory only.
    pub fn from_env() -> FidoStore {
        match std::env::var_os("LIMINA_FIDO_STORE") {
            Some(p) => Self::with_path(PathBuf::from(p)),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> FidoStore {
        FidoStore {
            inner: Mutex::new(Vec::new()),
            path: None,
        }
    }

    /// Load from `path` (missing/corrupt file → empty, non-fatal) and persist to it.
    pub fn with_path(path: PathBuf) -> FidoStore {
        let creds = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Credential>>(&b).ok())
            .unwrap_or_default();
        FidoStore {
            inner: Mutex::new(creds),
            path: Some(path),
        }
    }

    /// Register a new credential.
    pub fn add(&self, cred: Credential) {
        let mut creds = self.inner.lock().unwrap();
        creds.push(cred);
        self.persist(&creds);
    }

    /// All credentials for `rp_id`, newest first (resident-key assertion order).
    pub fn for_rp(&self, rp_id: &str) -> Vec<Credential> {
        let creds = self.inner.lock().unwrap();
        creds
            .iter()
            .filter(|c| c.rp_id == rp_id)
            .rev()
            .cloned()
            .collect()
    }

    /// The credential matching `cred_id` under `rp_id`, if any (allow-list path).
    pub fn find(&self, rp_id: &str, cred_id: &[u8]) -> Option<Credential> {
        let creds = self.inner.lock().unwrap();
        creds
            .iter()
            .find(|c| c.rp_id == rp_id && c.cred_id == cred_id)
            .cloned()
    }

    /// Increment and return the signature counter for `cred_id`. Returns `None` if
    /// the credential vanished between selection and use.
    pub fn bump_count(&self, cred_id: &[u8]) -> Option<u32> {
        let mut creds = self.inner.lock().unwrap();
        let c = creds.iter_mut().find(|c| c.cred_id == cred_id)?;
        c.sign_count = c.sign_count.wrapping_add(1);
        let n = c.sign_count;
        self.persist(&creds);
        Some(n)
    }

    fn persist(&self, creds: &[Credential]) {
        if let Some(path) = &self.path {
            if let Ok(json) = serde_json::to_vec_pretty(creds) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_find_and_counter() {
        let store = FidoStore::in_memory();
        store.add(Credential {
            cred_id: vec![1, 2, 3],
            rp_id: "example.com".into(),
            user_handle: vec![9],
            user_name: "alice".into(),
            blob: vec![0xaa],
            sign_count: 0,
        });
        assert!(store.find("example.com", &[1, 2, 3]).is_some());
        assert!(store.find("example.com", &[4]).is_none());
        assert!(store.find("other.com", &[1, 2, 3]).is_none());
        assert_eq!(store.for_rp("example.com").len(), 1);
        assert_eq!(store.bump_count(&[1, 2, 3]), Some(1));
        assert_eq!(store.bump_count(&[1, 2, 3]), Some(2));
        assert_eq!(store.bump_count(&[7]), None);
    }
}
