// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Safe Rust wrapper over the Secure-Enclave Swift shim (`swift/fido_sep.swift`,
//! compiled by `build.rs`). Backs the M14 virtual FIDO authenticator: create a
//! userPresence-gated P-256 key, read its public key, and sign behind a Touch ID
//! prompt. Persistence is the opaque CryptoKit blob (Spike A — no keychain, no
//! entitlements). Every op is one FFI call into a caller-owned buffer.

use anyhow::{bail, Result};

// C ABI from fido_sep.swift. Each returns a byte count written to `out`, or a
// negative error (-1 access-control, -2 buffer too small, -3 enclave/CryptoKit).
extern "C" {
    fn limina_sep_available() -> i32;
    fn limina_sep_create(out: *mut u8, cap: isize) -> isize;
    fn limina_sep_pubkey(blob: *const u8, blob_len: isize, out: *mut u8, cap: isize) -> isize;
    fn limina_sep_sign(
        blob: *const u8,
        blob_len: isize,
        msg: *const u8,
        msg_len: isize,
        reason: *const std::os::raw::c_char,
        out: *mut u8,
        cap: isize,
    ) -> isize;
}

/// CryptoKit SEP blobs are ~284 B (Spike A) — 512 is comfortable headroom.
const BLOB_CAP: usize = 512;
/// X9.63 uncompressed P-256 public key: 0x04 || X(32) || Y(32).
const PUBKEY_LEN: usize = 65;
/// DER ECDSA-P256 signatures are ≤72 B.
const SIG_CAP: usize = 80;

/// Is a Secure Enclave usable on this host? The control plane gates the `fido`
/// capability on this — no SEP, no authenticator advertised (stock-degrade rule).
pub fn available() -> bool {
    unsafe { limina_sep_available() == 1 }
}

/// A Secure-Enclave P-256 signing key, represented by its persistable blob. The
/// private key never leaves the enclave; the blob is enclave-encrypted and useless
/// off this Mac (Spike A).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SepKey {
    blob: Vec<u8>,
}

impl SepKey {
    /// Create a fresh biometry-gated key.
    pub fn create() -> Result<SepKey> {
        let mut buf = [0u8; BLOB_CAP];
        let n = unsafe { limina_sep_create(buf.as_mut_ptr(), BLOB_CAP as isize) };
        if n < 0 {
            bail!("SEP key create failed (code {n})");
        }
        Ok(SepKey {
            blob: buf[..n as usize].to_vec(),
        })
    }

    /// Reconstruct from a stored blob (no enclave round-trip until used).
    pub fn from_blob(blob: Vec<u8>) -> SepKey {
        SepKey { blob }
    }

    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    /// The X9.63 uncompressed public key (65 bytes). Errors if the blob is foreign
    /// (created on another Mac / another enclave).
    pub fn public_key_x963(&self) -> Result<[u8; PUBKEY_LEN]> {
        let mut buf = [0u8; PUBKEY_LEN];
        let n = unsafe {
            limina_sep_pubkey(
                self.blob.as_ptr(),
                self.blob.len() as isize,
                buf.as_mut_ptr(),
                PUBKEY_LEN as isize,
            )
        };
        if n != PUBKEY_LEN as isize {
            bail!("SEP pubkey failed (code {n})");
        }
        Ok(buf)
    }

    /// Sign `msg` (ECDSA-SHA256, DER) behind a Touch ID prompt showing `reason`.
    /// A user cancel or no-match returns an error.
    pub fn sign(&self, msg: &[u8], reason: &str) -> Result<Vec<u8>> {
        let creason = std::ffi::CString::new(reason).unwrap_or_default();
        let mut buf = [0u8; SIG_CAP];
        let n = unsafe {
            limina_sep_sign(
                self.blob.as_ptr(),
                self.blob.len() as isize,
                msg.as_ptr(),
                msg.len() as isize,
                creason.as_ptr(),
                buf.as_mut_ptr(),
                SIG_CAP as isize,
            )
        };
        if n < 0 {
            bail!("SEP sign failed / declined (code {n})");
        }
        Ok(buf[..n as usize].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These hit the real Secure Enclave and (for sign) prompt Touch ID, so they are
    // ignored by default — run seated with `--ignored` on Apple silicon. create +
    // pubkey are non-interactive; sign needs a finger.
    #[test]
    #[ignore]
    fn create_and_pubkey_roundtrip() {
        assert!(available());
        let key = SepKey::create().unwrap();
        let pk = key.public_key_x963().unwrap();
        assert_eq!(pk[0], 0x04);
        // Reload from blob yields the same public key.
        let reloaded = SepKey::from_blob(key.blob().to_vec());
        assert_eq!(pk, reloaded.public_key_x963().unwrap());
    }
}
