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
    fn limina_sep_has_touchid() -> i32;
    fn limina_sep_verify(token: u64, reason: *const std::os::raw::c_char) -> i32;
    fn limina_sep_cancel(token: u64);
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

/// Does this host have a Touch ID sensor? The control plane gates the `fingerprint`
/// capability on sensor PRESENCE — NOT on [`available`] (a SEP-but-no-Touch-ID
/// desktop like a Mac mini/Studio must not advertise a fingerprint reader), and NOT on
/// whether a biometric prompt can succeed *right now*: that transient check
/// (`canEvaluatePolicy`) reports `systemCancel` on a clamshell/locked Mac even though
/// the real prompt works, which wrongly hides the reader. Availability at verify time
/// is the prompt's job — a momentary failure is a clean no-match. See `sep_verify`.
pub fn has_touchid() -> bool {
    unsafe { limina_sep_has_touchid() == 1 }
}

/// How a Touch ID prompt ended. The [`Cancelled`](VerifyOutcome::Cancelled) /
/// [`NoMatch`](VerifyOutcome::NoMatch) split is the whole point: a real reader's "that finger
/// didn't match" invites another try (and the guest's PAM stack duly retries), whereas a human
/// who dismissed the sheet — or a locked-out sensor — must not be asked again, or the prompt
/// simply reappears until the retry budget runs out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// A trusted finger was presented.
    Matched,
    /// The sheet ran and rejected the finger — retryable.
    NoMatch,
    /// Nobody is going to authenticate: the user dismissed the sheet, [`cancel_verify`]
    /// invalidated it, the system pulled it, or biometry is locked out / unavailable.
    Cancelled,
}

/// Prompt a biometrics-only Touch ID sheet showing `reason` and report how it ended. This is
/// the fingerprint reader's "match" primitive — it produces no crypto (the guest verifies
/// none), just "an authorized finger was presented" plus the cancel/no-match distinction.
///
/// `token` names *this* prompt so [`cancel_verify`] can only ever dismiss the prompt it meant
/// to: pass a value no other in-flight or future prompt uses (a monotonic counter). A cancel
/// that arrives before the sheet is up is remembered against its token and returns
/// [`VerifyOutcome::Cancelled`] immediately, and a cancel for a prompt that already finished
/// matches nothing — so a lost race can never eat somebody else's prompt.
pub fn sep_verify(token: u64, reason: &str) -> VerifyOutcome {
    let creason = std::ffi::CString::new(reason).unwrap_or_default();
    // 1 = matched, -2 = cancelled, anything else (0, or an unexpected code) = no match.
    match unsafe { limina_sep_verify(token, creason.as_ptr()) } {
        1 => VerifyOutcome::Matched,
        -2 => VerifyOutcome::Cancelled,
        _ => VerifyOutcome::NoMatch,
    }
}

/// Dismiss the Touch ID sheet [`sep_verify`] put up for `token`, making it return
/// [`VerifyOutcome::Cancelled`]. Call this when whoever asked for the prompt has gone away —
/// the guest cancelling its fingerprint request — so the Mac stops asking on behalf of nobody.
pub fn cancel_verify(token: u64) {
    unsafe { limina_sep_cancel(token) }
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
