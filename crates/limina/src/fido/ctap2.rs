// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! CTAP2 commands for the virtual FIDO authenticator (M14): `authenticatorGetInfo`,
//! `authenticatorMakeCredential`, `authenticatorGetAssertion`. Backed by
//! Secure-Enclave ES256 keys (`crate::sep`) behind a host Touch ID prompt — the
//! guest browser / libfido2 gets a real WebAuthn platform authenticator whose
//! private keys never leave this Mac's enclave.
//!
//! Requests are CTAP canonical CBOR (parsed with `ciborium`); responses are
//! hand-encoded so we control canonical key ordering exactly. Attestation is
//! **packed self-attestation** (fmt "packed", `alg -7`, `sig` over the credential's
//! own key) — no X.509, which every consumer treats as self-attestation, and the
//! registration signature is what triggers Touch ID at credential creation time.

use ciborium::value::Value;
use sha2::{Digest, Sha256};

use super::store::{Credential, FidoStore};
use crate::sep::SepKey;

// CTAP2 command bytes.
const CMD_MAKE_CREDENTIAL: u8 = 0x01;
const CMD_GET_ASSERTION: u8 = 0x02;
const CMD_GET_INFO: u8 = 0x04;

// CTAP2 status codes (subset).
const CTAP2_OK: u8 = 0x00;
const CTAP1_ERR_INVALID_COMMAND: u8 = 0x01;
const CTAP1_ERR_INVALID_LENGTH: u8 = 0x03;
const CTAP2_ERR_OPERATION_DENIED: u8 = 0x27;
const CTAP2_ERR_INVALID_CBOR: u8 = 0x12;
const CTAP2_ERR_MISSING_PARAMETER: u8 = 0x14;
const CTAP2_ERR_UNSUPPORTED_ALGORITHM: u8 = 0x26;
const CTAP2_ERR_NO_CREDENTIALS: u8 = 0x2E;
const CTAP2_ERR_CREDENTIAL_EXCLUDED: u8 = 0x19;
const CTAP2_ERR_INVALID_OPTION: u8 = 0x2C;

/// ES256 COSE algorithm id.
const ALG_ES256: i64 = -7;

/// Our AAGUID: `c2852a39-665e-4017-aef0-21d709f98b1d`, this authenticator model's public identity.
///
/// A random v4 UUID rather than the ASCII string it used to be, because an AAGUID is a value other
/// people's software looks up, not a label we read: relying parties resolve it against the FIDO
/// metadata service and the community passkey-AAGUID list to show "which passkey provider is this",
/// and a value shaped like a UUID is the price of ever appearing in either. (Until an entry lands
/// there, sites show the AAGUID with no name — which is what a lookup miss looks like, not a bug.)
///
/// **Stable forever.** Changing it makes every credential already registered against it look like
/// it came from a different authenticator model to any RP that recorded it.
///
/// The zero AAGUID is NOT available to us as an alternative: 16 zero bytes is how WebAuthn spells
/// "self-attested, identify me by nothing", and choosing it would pass our `packed` attestation
/// through the client untouched — but permanently forecloses being named.
const AAGUID: [u8; 16] = [
    0xc2, 0x85, 0x2a, 0x39, 0x66, 0x5e, 0x40, 0x17, 0xae, 0xf0, 0x21, 0xd7, 0x09, 0xf9, 0x8b, 0x1d,
];

// authenticatorData flag bits.
const FLAG_UP: u8 = 0x01; // user present
const FLAG_UV: u8 = 0x04; // user verified
const FLAG_AT: u8 = 0x40; // attested credential data included

/// Handle one CTAP2 message (`payload[0]` = command, rest = CBOR). Returns the full
/// response body: a status byte, followed by response CBOR on success.
pub fn handle(store: &FidoStore, payload: &[u8]) -> Vec<u8> {
    let Some((&cmd, cbor)) = payload.split_first() else {
        return vec![CTAP1_ERR_INVALID_LENGTH];
    };
    let result = match cmd {
        CMD_GET_INFO => Ok(get_info()),
        CMD_MAKE_CREDENTIAL => make_credential(store, cbor),
        CMD_GET_ASSERTION => get_assertion(store, cbor),
        _ => Err(CTAP1_ERR_INVALID_COMMAND),
    };
    match result {
        Ok(body) => {
            let mut out = Vec::with_capacity(body.len() + 1);
            out.push(CTAP2_OK);
            out.extend_from_slice(&body);
            out
        }
        Err(status) => vec![status],
    }
}

/// authenticatorGetInfo: versions, aaguid, options (resident keys + user
/// verification supported; platform authenticator).
fn get_info() -> Vec<u8> {
    let mut w = CborWriter::new();
    w.map_header(3);
    // 1: versions
    w.uint(1);
    w.array_header(1);
    w.text("FIDO_2_0");
    // 3: aaguid
    w.uint(3);
    w.bytes(&AAGUID);
    // 4: options — CTAP2 canonical CBOR order (shorter keys first, then bytewise),
    // i.e. rk, up, uv, plat. libfido2 rejects any other order as invalid cbor and
    // falls back to U2F, which we don't support (→ FIDO_ERR_RX).
    w.uint(4);
    w.map_header(4);
    w.text("rk");
    w.bool(true); // discoverable (resident) credentials
    w.text("up");
    w.bool(true); // user presence
    w.text("uv");
    w.bool(true); // user verification (Touch ID)
    w.text("plat");
    w.bool(true); // platform (bound to this device)
    w.finish()
}

/// The `options` map (`makeCredential` key 7, `getAssertion` key 5) as far as we care about it:
/// the `up` value, if the platform sent one. `None` means "not specified", which CTAP2 defines as
/// `up: true` — the ordinary consent-required ceremony.
fn requested_up(root: &[(Value, Value)], options_key: i128) -> Option<bool> {
    map_get(root, options_key)
        .and_then(as_map)
        .and_then(|opts| text_key(opts, "up"))
        .and_then(|v| match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        })
}

/// Does `list` (a CTAP credential-descriptor array) name a credential we hold for `rp_id`?
/// Used for `makeCredential`'s excludeList — the RP saying "this account already has a passkey
/// on some authenticator; if it is you, refuse". Without this a repeat registration mints a
/// second credential for the same account, and the site only ever learns about the newest.
fn holds_any(store: &FidoStore, rp_id: &str, list: &[Value]) -> bool {
    list.iter()
        .filter_map(|d| as_map(d).and_then(|m| text_key_bytes(m, "id")))
        .any(|id| store.find(rp_id, id).is_some())
}

/// Put a Touch ID sheet up purely for consent — no signature, no enclave key involved. This is
/// CTAP2's "wait for user presence" step, which excludeList needs: the spec makes the authenticator
/// take consent *before* admitting a credential exists, so a site cannot silently probe which
/// accounts live on this Mac. `LIMINA_FIDO_TEST_APPROVE` skips the sheet, as everywhere else.
fn user_presence(reason: &str) -> bool {
    if super::test_approve() {
        return true;
    }
    crate::sep::sep_verify(crate::sep::next_token(), reason) == crate::sep::VerifyOutcome::Matched
}

fn make_credential(store: &FidoStore, cbor: &[u8]) -> Result<Vec<u8>, u8> {
    let root = parse_map(cbor)?;
    let client_data_hash = map_bytes(&root, 1).ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let rp = map_get(&root, 2)
        .and_then(as_map)
        .ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let rp_id = text_key_text(rp, "id").ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let user = map_get(&root, 3)
        .and_then(as_map)
        .ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let user_handle = text_key_bytes(user, "id").ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let user_name = text_key_text(user, "name").unwrap_or("").to_string();

    // pubKeyCredParams (key 4): require ES256 among the offered algorithms.
    let params = map_get(&root, 4)
        .and_then(as_array)
        .ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let wants_es256 = params.iter().any(|p| {
        as_map(p)
            .and_then(|m| text_key_int(m, "alg"))
            .map(|alg| alg == ALG_ES256)
            .unwrap_or(false)
    });
    if !wants_es256 {
        return Err(CTAP2_ERR_UNSUPPORTED_ALGORITHM);
    }

    // `up: false` is meaningless for registration — a credential nobody consented to is not a
    // credential — and CTAP2.1 spells the answer out: end the operation with CTAP2_ERR_INVALID_OPTION.
    if requested_up(&root, 7) == Some(false) {
        return Err(CTAP2_ERR_INVALID_OPTION);
    }

    // excludeList (key 5): consent first, then admit we hold one. Doing it in this order is the
    // spec's, and the reason is privacy — the error itself reveals that this account has a passkey
    // here, so the human authorizes the disclosure. Checked before minting anything, so a refused
    // registration leaves no key behind.
    if let Some(list) = map_get(&root, 5).and_then(as_array) {
        if holds_any(store, rp_id, list) {
            if !user_presence(&format!("{rp_id} already has a passkey on this Mac")) {
                return Err(CTAP2_ERR_OPERATION_DENIED);
            }
            return Err(CTAP2_ERR_CREDENTIAL_EXCLUDED);
        }
    }

    // Mint the enclave key and a random credential id.
    let key = SepKey::create().map_err(|_| CTAP2_ERR_OPERATION_DENIED)?;
    let pubkey = key
        .public_key_x963()
        .map_err(|_| CTAP2_ERR_OPERATION_DENIED)?;
    let cred_id = random_bytes(16).map_err(|_| CTAP2_ERR_OPERATION_DENIED)?;

    // authenticatorData with attested credential data.
    let auth_data = build_auth_data(
        rp_id,
        FLAG_UP | FLAG_UV | FLAG_AT,
        0,
        Some((&cred_id, &cose_es256_key(&pubkey))),
    );

    // Packed self-attestation: sign authData || clientDataHash with the new key.
    // This IS the Touch ID moment at registration.
    let mut signed = auth_data.clone();
    signed.extend_from_slice(client_data_hash);
    let reason = format!("Register a passkey for {rp_id}");
    let sig = key
        .sign(&signed, &reason)
        .map_err(|_| CTAP2_ERR_OPERATION_DENIED)?;

    store.add(Credential {
        cred_id: cred_id.clone(),
        rp_id: rp_id.to_string(),
        user_handle: user_handle.to_vec(),
        user_name,
        blob: key.blob().to_vec(),
        sign_count: 0,
    });

    // Response: {1: fmt, 2: authData, 3: attStmt}.
    let mut w = CborWriter::new();
    w.map_header(3);
    w.uint(1);
    w.text("packed");
    w.uint(2);
    w.bytes(&auth_data);
    w.uint(3);
    w.map_header(2);
    w.text("alg");
    w.nint(ALG_ES256);
    w.text("sig");
    w.bytes(&sig);
    Ok(w.finish())
}

fn get_assertion(store: &FidoStore, cbor: &[u8]) -> Result<Vec<u8>, u8> {
    let root = parse_map(cbor)?;
    let rp_id = map_text(&root, 1).ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let client_data_hash = map_bytes(&root, 2).ok_or(CTAP2_ERR_MISSING_PARAMETER)?;

    // Select a credential: allow-list (key 3) first entry we hold, else the newest
    // resident credential for this RP.
    let allow = map_get(&root, 3).and_then(as_array);
    let cred = match allow {
        Some(list) if !list.is_empty() => list
            .iter()
            .filter_map(|d| as_map(d).and_then(|m| text_key_bytes(m, "id")))
            .find_map(|id| store.find(rp_id, id)),
        _ => store.for_rp(rp_id).into_iter().next(),
    }
    .ok_or(CTAP2_ERR_NO_CREDENTIALS)?;

    // A CTAP2 "pre-flight" carries `up: false`: the client asks *silently* which of an allow-list's
    // credentials this authenticator holds, then re-runs the ceremony for real against the one that
    // matched. Answering it with a full signature is what made signing in cost an extra Touch ID.
    if requested_up(&root, 5) == Some(false) {
        return Ok(silent_assertion(rp_id, &cred));
    }

    let count = store
        .bump_count(&cred.cred_id)
        .ok_or(CTAP2_ERR_NO_CREDENTIALS)?;

    // authenticatorData (no attested credential data on assertions).
    let auth_data = build_auth_data(rp_id, FLAG_UP | FLAG_UV, count, None);
    let mut signed = auth_data.clone();
    signed.extend_from_slice(client_data_hash);
    let reason = format!("Sign in to {rp_id}");
    let key = SepKey::from_blob(cred.blob.clone());
    let sig = key
        .sign(&signed, &reason)
        .map_err(|_| CTAP2_ERR_OPERATION_DENIED)?;

    Ok(assertion_response(&cred, &auth_data, &sig))
}

/// A getAssertion response: {1: credential, 2: authData, 3: signature, 4: user}.
fn assertion_response(cred: &Credential, auth_data: &[u8], sig: &[u8]) -> Vec<u8> {
    let mut w = CborWriter::new();
    w.map_header(4);
    w.uint(1);
    w.map_header(2);
    w.text("id");
    w.bytes(&cred.cred_id);
    w.text("type");
    w.text("public-key");
    w.uint(2);
    w.bytes(auth_data);
    w.uint(3);
    w.bytes(sig);
    w.uint(4);
    w.map_header(1);
    w.text("id");
    w.bytes(&cred.user_handle);
    w.finish()
}

/// The answer to a silent pre-flight: "yes, this credential is mine" and nothing else.
///
/// The client is not asking us to authenticate anybody. It is choosing which credential to run the
/// real ceremony against — Chromium re-issues `getAssertion` for exactly the credential this
/// response names, with user presence enforced, and libfido2-family clients likewise use it only to
/// pick a descriptor. Neither ever verifies the signature, and neither *can*: the public key lives
/// with the relying party, and the probe's clientDataHash is a block of zeros the client made up.
///
/// So the signature here is a placeholder, and that is a deliberate, bounded lie with no way to
/// become an authentication. Everything a relying party checks fails on it independently: `flags`
/// carries neither UP nor UV, which WebAuthn requires; the challenge is the client's dummy, not the
/// RP's; and the bytes do not verify against the credential's public key. What we cannot do is
/// produce a *real* signature — every key here is enclave-held and presence-gated, so signing means
/// a Touch ID sheet, which is precisely the prompt the pre-flight exists to avoid.
///
/// The alternative — refusing the probe with an error — is what we shipped first, and it broke
/// signing in outright: to Chromium any error means "credential not recognised", so it moves to the
/// next batch and, finding none, fails the ceremony as NotAllowedError.
///
/// The counter is reported as-is and never advanced: a probe is not an authentication, and a
/// counter that moves without one is what an RP reads as a cloned authenticator.
fn silent_assertion(rp_id: &str, cred: &Credential) -> Vec<u8> {
    const PLACEHOLDER_SIG: &[u8] = &[0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01];
    let auth_data = build_auth_data(rp_id, 0, cred.sign_count, None);
    assertion_response(cred, &auth_data, PLACEHOLDER_SIG)
}

/// authenticatorData = rpIdHash(32) || flags(1) || signCount(4 BE) ||
/// [attestedCredentialData].
fn build_auth_data(
    rp_id: &str,
    flags: u8,
    sign_count: u32,
    attested: Option<(&[u8], &[u8])>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(37);
    out.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
    out.push(flags);
    out.extend_from_slice(&sign_count.to_be_bytes());
    if let Some((cred_id, cose_key)) = attested {
        out.extend_from_slice(&AAGUID);
        out.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        out.extend_from_slice(cred_id);
        out.extend_from_slice(cose_key);
    }
    out
}

/// COSE_Key for an ES256 public key from its X9.63 form (0x04 || X(32) || Y(32)).
/// Canonical CBOR map {1: 2, 3: -7, -1: 1, -2: X, -3: Y} — keys already ascending
/// by encoded byte (0x01, 0x03, 0x20, 0x21, 0x22).
fn cose_es256_key(x963: &[u8; 65]) -> Vec<u8> {
    let x = &x963[1..33];
    let y = &x963[33..65];
    let mut w = CborWriter::new();
    w.map_header(5);
    w.uint(1); // label 1: kty
    w.uint(2); // EC2
    w.uint(3); // label 3: alg
    w.nint(ALG_ES256);
    w.nint(-1); // label -1: crv
    w.uint(1); // P-256
    w.nint(-2); // label -2: x
    w.bytes(x);
    w.nint(-3); // label -3: y
    w.bytes(y);
    w.finish()
}

fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

// --- CBOR request parsing (ciborium Value helpers) -------------------------

fn parse_map(cbor: &[u8]) -> Result<Vec<(Value, Value)>, u8> {
    let v: Value = ciborium::from_reader(cbor).map_err(|_| CTAP2_ERR_INVALID_CBOR)?;
    match v {
        Value::Map(m) => Ok(m),
        _ => Err(CTAP2_ERR_INVALID_CBOR),
    }
}

fn as_int(v: &Value) -> Option<i128> {
    match v {
        Value::Integer(i) => Some((*i).into()),
        _ => None,
    }
}
fn as_bytes(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}
fn as_text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        _ => None,
    }
}
fn as_map(v: &Value) -> Option<&[(Value, Value)]> {
    match v {
        Value::Map(m) => Some(m),
        _ => None,
    }
}
fn as_array(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

/// Look up an integer-keyed entry (CTAP2 request top level).
fn map_get(m: &[(Value, Value)], key: i128) -> Option<&Value> {
    m.iter()
        .find(|(k, _)| as_int(k) == Some(key))
        .map(|(_, v)| v)
}
fn map_bytes(m: &[(Value, Value)], key: i128) -> Option<&[u8]> {
    map_get(m, key).and_then(as_bytes)
}
fn map_text(m: &[(Value, Value)], key: i128) -> Option<&str> {
    map_get(m, key).and_then(as_text)
}

/// Look up a text-keyed entry (nested maps: rp, user, pubKeyCredParams items).
fn text_key<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    m.iter()
        .find(|(k, _)| as_text(k) == Some(key))
        .map(|(_, v)| v)
}
fn text_key_bytes<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a [u8]> {
    text_key(m, key).and_then(as_bytes)
}
fn text_key_text<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a str> {
    text_key(m, key).and_then(as_text)
}
fn text_key_int(m: &[(Value, Value)], key: &str) -> Option<i64> {
    text_key(m, key)
        .and_then(as_int)
        .and_then(|i| i64::try_from(i).ok())
}

// --- minimal canonical CBOR writer -----------------------------------------

struct CborWriter {
    buf: Vec<u8>,
}

impl CborWriter {
    fn new() -> CborWriter {
        CborWriter { buf: Vec::new() }
    }

    fn head(&mut self, major: u8, val: u64) {
        let mt = major << 5;
        if val < 24 {
            self.buf.push(mt | val as u8);
        } else if val < 0x100 {
            self.buf.push(mt | 24);
            self.buf.push(val as u8);
        } else if val < 0x1_0000 {
            self.buf.push(mt | 25);
            self.buf.extend_from_slice(&(val as u16).to_be_bytes());
        } else if val < 0x1_0000_0000 {
            self.buf.push(mt | 26);
            self.buf.extend_from_slice(&(val as u32).to_be_bytes());
        } else {
            self.buf.push(mt | 27);
            self.buf.extend_from_slice(&val.to_be_bytes());
        }
    }

    fn uint(&mut self, n: u64) {
        self.head(0, n);
    }
    /// Encode a negative integer given as its actual value (e.g. -7).
    fn nint(&mut self, n: i64) {
        debug_assert!(n < 0);
        self.head(1, (-1 - n) as u64);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.head(2, b.len() as u64);
        self.buf.extend_from_slice(b);
    }
    fn text(&mut self, s: &str) {
        self.head(3, s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn array_header(&mut self, len: u64) {
        self.head(4, len);
    }
    fn map_header(&mut self, len: u64) {
        self.head(5, len);
    }
    fn bool(&mut self, b: bool) {
        self.buf.push(0xf4 | u8::from(b)); // false=0xf4, true=0xf5
    }
    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbor_writer_canonical_shapes() {
        let mut w = CborWriter::new();
        w.map_header(2);
        w.uint(1);
        w.text("FIDO_2_0");
        w.nint(-7);
        w.bool(true);
        let out = w.finish();
        // map(2), key 1, text(8)"FIDO_2_0", negint -7 (0x26), true (0xf5).
        assert_eq!(out[0], 0xa2);
        assert_eq!(out[1], 0x01);
        assert_eq!(out[2], 0x68);
        assert_eq!(&out[3..11], b"FIDO_2_0");
        assert_eq!(out[11], 0x26);
        assert_eq!(out[12], 0xf5);
    }

    #[test]
    fn cose_key_layout() {
        let mut x963 = [0u8; 65];
        x963[0] = 0x04;
        for (i, b) in x963.iter_mut().enumerate() {
            *b = i as u8;
        }
        x963[0] = 0x04;
        let cose = cose_es256_key(&x963);
        // map(5); 1:2; 3:-7(0x26); -1(0x20):1; -2(0x21):bytes(32); -3(0x22):bytes(32).
        assert_eq!(cose[0], 0xa5);
        assert_eq!(&cose[1..4], &[0x01, 0x02, 0x03]);
        assert_eq!(cose[4], 0x26); // -7
        assert_eq!(cose[5], 0x20); // key -1
        assert_eq!(cose[6], 0x01); // P-256
        assert_eq!(cose[7], 0x21); // key -2
        assert_eq!(cose[8], 0x58); // bytes, 1-byte len follows
        assert_eq!(cose[9], 32);
        assert_eq!(&cose[10..42], &x963[1..33]);
    }

    #[test]
    fn auth_data_assertion_layout() {
        let ad = build_auth_data("example.com", FLAG_UP | FLAG_UV, 0x01020304, None);
        assert_eq!(ad.len(), 37);
        assert_eq!(ad[32], 0x05); // UP|UV
        assert_eq!(&ad[33..37], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&ad[0..32], &Sha256::digest(b"example.com")[..]);
    }

    #[test]
    fn get_info_parses_and_advertises_fido2() {
        let body = get_info();
        let v: Value = ciborium::from_reader(&body[..]).unwrap();
        let m = as_map(&v).unwrap();
        let versions = map_get(m, 1).and_then(as_array).unwrap();
        assert!(versions.iter().any(|x| as_text(x) == Some("FIDO_2_0")));
        assert_eq!(map_bytes(m, 3).unwrap(), &AAGUID);
        let opts = map_get(m, 4).and_then(as_map).unwrap();
        assert_eq!(text_key(opts, "rk").and_then(as_bool_v), Some(true));
        assert_eq!(text_key(opts, "uv").and_then(as_bool_v), Some(true));
        // CTAP2 canonical order: shorter keys first, then bytewise. libfido2 rejects
        // any other order (that was the FIDO_ERR_RX bug). rk,up,uv (len 2) then plat.
        let keys: Vec<&str> = opts.iter().filter_map(|(k, _)| as_text(k)).collect();
        assert_eq!(keys, vec!["rk", "up", "uv", "plat"]);
    }

    #[test]
    fn missing_params_error_not_panic() {
        let store = FidoStore::in_memory();
        // Empty map → missing clientDataHash.
        let empty = {
            let mut w = CborWriter::new();
            w.map_header(0);
            w.finish()
        };
        assert_eq!(
            make_credential(&store, &empty),
            Err(CTAP2_ERR_MISSING_PARAMETER)
        );
        assert_eq!(
            get_assertion(&store, &empty),
            Err(CTAP2_ERR_MISSING_PARAMETER)
        );
    }

    #[test]
    fn assertion_without_credential_errors() {
        let store = FidoStore::in_memory();
        let mut w = CborWriter::new();
        w.map_header(2);
        w.uint(1);
        w.text("nobody.example");
        w.uint(2);
        w.bytes(&[0u8; 32]);
        let req = w.finish();
        assert_eq!(get_assertion(&store, &req), Err(CTAP2_ERR_NO_CREDENTIALS));
    }

    /// A stored credential whose blob is never touched (these tests never reach the enclave).
    fn stored(store: &FidoStore, rp: &str, cred_id: &[u8]) {
        store.add(Credential {
            cred_id: cred_id.to_vec(),
            rp_id: rp.to_string(),
            user_handle: vec![1, 2, 3],
            user_name: "alice".into(),
            blob: vec![0xAB; 8],
            sign_count: 0,
        });
    }

    /// A getAssertion with an allow-list and `options: {up: <up>}`.
    fn assertion_request_up(rp: &str, cred_id: &[u8], up: bool) -> Vec<u8> {
        let mut w = CborWriter::new();
        w.map_header(4);
        w.uint(1);
        w.text(rp);
        w.uint(2);
        w.bytes(&[0x22u8; 32]);
        w.uint(3);
        w.array_header(1);
        w.map_header(2);
        w.text("id");
        w.bytes(cred_id);
        w.text("type");
        w.text("public-key");
        w.uint(5);
        w.map_header(1);
        w.text("up");
        w.bool(up);
        w.finish()
    }

    /// The client's silent pre-flight (`up: false`) must answer "this one is mine" without a Touch
    /// ID — it is choosing a credential, not authenticating anyone. It must also *succeed*: to
    /// Chromium any error means "credential not recognised", so it moves to the next batch and,
    /// finding none, fails the sign-in as NotAllowedError. Refusing the probe is what we shipped
    /// first, and that is exactly what a real browser did with it.
    #[test]
    fn a_silent_preflight_answers_without_a_touch_id() {
        let store = FidoStore::in_memory();
        stored(&store, "webauthn.io", b"cred-1");
        // No enclave is reachable from a plain test binary, so a response at all proves the probe
        // short-circuits before any signing.
        let resp = get_assertion(
            &store,
            &assertion_request_up("webauthn.io", b"cred-1", false),
        )
        .unwrap();
        let v: Value = ciborium::from_reader(&resp[..]).unwrap();
        let m = as_map(&v).unwrap();
        // It names the credential the client asked about — that is the whole payload of the answer.
        let cred = map_get(m, 1).and_then(as_map).unwrap();
        assert_eq!(text_key_bytes(cred, "id"), Some(&b"cred-1"[..]));
        // Neither presence nor verification is claimed: nobody touched anything.
        let auth_data = map_bytes(m, 2).unwrap();
        assert_eq!(
            auth_data[32] & (FLAG_UP | FLAG_UV),
            0,
            "silent means silent"
        );
        // And the counter does not move — one that advances without an authentication is what an
        // RP reads as a cloned authenticator.
        assert_eq!(&auth_data[33..37], &[0, 0, 0, 0]);
        assert_eq!(store.find("webauthn.io", b"cred-1").unwrap().sign_count, 0);
    }

    /// ...but a pre-flight for a credential we do NOT hold still gets the honest answer, which is
    /// the whole information the probe came for. (Refusing the option first would tell every
    /// probe the same thing and hide it.)
    #[test]
    fn a_silent_preflight_for_a_stranger_still_says_no_credentials() {
        let store = FidoStore::in_memory();
        stored(&store, "webauthn.io", b"cred-1");
        assert_eq!(
            get_assertion(
                &store,
                &assertion_request_up("webauthn.io", b"other", false)
            ),
            Err(CTAP2_ERR_NO_CREDENTIALS)
        );
    }

    /// `up: false` on registration is nonsense (nobody consented to the credential), and CTAP2.1
    /// names the error. It must be refused before any enclave key is minted.
    #[test]
    fn registration_refuses_up_false() {
        let store = FidoStore::in_memory();
        let mut req = make_credential_request("example.com", &[7, 7, 7]);
        // Re-encode with an options map (key 7) carrying up:false.
        let body = {
            let mut w = CborWriter::new();
            w.map_header(5);
            w.uint(1);
            w.bytes(&[0x11u8; 32]);
            w.uint(2);
            w.map_header(1);
            w.text("id");
            w.text("example.com");
            w.uint(3);
            w.map_header(2);
            w.text("id");
            w.bytes(&[7, 7, 7]);
            w.text("name");
            w.text("alice");
            w.uint(4);
            w.array_header(1);
            w.map_header(2);
            w.text("alg");
            w.nint(ALG_ES256);
            w.text("type");
            w.text("public-key");
            w.uint(7);
            w.map_header(1);
            w.text("up");
            w.bool(false);
            w.finish()
        };
        req.truncate(1);
        req.extend(body);
        assert_eq!(
            make_credential(&store, &req[1..]),
            Err(CTAP2_ERR_INVALID_OPTION)
        );
        assert!(store.for_rp("example.com").is_empty(), "no key was minted");
    }

    /// excludeList is how an RP says "this account already has a passkey; if it is on you, refuse".
    /// Without it a repeat registration mints a second credential for the same account and the
    /// site only ever hears about the newest — which is how one store ended up with three
    /// credentials for one webauthn.io account.
    #[test]
    fn exclude_list_recognises_our_own_credentials() {
        let store = FidoStore::in_memory();
        stored(&store, "webauthn.io", b"cred-1");
        let descriptor = |id: &[u8]| {
            let mut w = CborWriter::new();
            w.map_header(2);
            w.text("id");
            w.bytes(id);
            w.text("type");
            w.text("public-key");
            ciborium::from_reader(&w.finish()[..]).unwrap()
        };
        let ours: Vec<Value> = vec![descriptor(b"cred-1")];
        let theirs: Vec<Value> = vec![descriptor(b"cred-9")];
        assert!(holds_any(&store, "webauthn.io", &ours));
        assert!(!holds_any(&store, "webauthn.io", &theirs));
        // Same credential id, different RP: not ours to exclude.
        assert!(!holds_any(&store, "example.com", &ours));
    }

    fn as_bool_v(v: &Value) -> Option<bool> {
        match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    // --- interactive end-to-end (real Secure Enclave + Touch ID) ---------------
    // Ignored: hits the enclave and prompts Touch ID twice (register, then assert).
    // Run seated on Apple silicon with a codesigned test binary:
    //   cargo test -p limina --no-run   (then codesign the deps binary)
    //   <binary> --ignored fido::ctap2::tests::make_then_assert_roundtrip --nocapture

    fn make_credential_request(rp: &str, user_handle: &[u8]) -> Vec<u8> {
        let mut w = CborWriter::new();
        w.map_header(4);
        w.uint(1);
        w.bytes(&[0x11u8; 32]); // clientDataHash
        w.uint(2);
        w.map_header(1);
        w.text("id");
        w.text(rp);
        w.uint(3);
        w.map_header(2);
        w.text("id");
        w.bytes(user_handle);
        w.text("name");
        w.text("alice");
        w.uint(4);
        w.array_header(1);
        w.map_header(2);
        w.text("alg");
        w.nint(ALG_ES256);
        w.text("type");
        w.text("public-key");
        let mut payload = vec![CMD_MAKE_CREDENTIAL];
        payload.extend(w.finish());
        payload
    }

    fn assertion_request(rp: &str, cred_id: &[u8]) -> Vec<u8> {
        let mut w = CborWriter::new();
        w.map_header(3);
        w.uint(1);
        w.text(rp);
        w.uint(2);
        w.bytes(&[0x22u8; 32]); // clientDataHash
        w.uint(3);
        w.array_header(1);
        w.map_header(2);
        w.text("id");
        w.bytes(cred_id);
        w.text("type");
        w.text("public-key");
        let mut payload = vec![CMD_GET_ASSERTION];
        payload.extend(w.finish());
        payload
    }

    #[test]
    #[ignore]
    fn make_then_assert_roundtrip() {
        let store = FidoStore::in_memory();

        // Register (Touch ID prompt #1). Response {1:"packed", 2:authData, 3:attStmt}.
        let resp = handle(&store, &make_credential_request("example.com", &[7, 7, 7]));
        assert_eq!(resp[0], CTAP2_OK, "makeCredential status");
        let mk: Value = ciborium::from_reader(&resp[1..]).unwrap();
        let mk = as_map(&mk).unwrap();
        assert_eq!(map_text(mk, 1), Some("packed"));
        let auth_data = map_bytes(mk, 2).unwrap();
        // attestedCredentialData: rpIdHash(32)+flags(1)+count(4)+aaguid(16)+len(2)+id...
        assert_eq!(
            auth_data[32] & FLAG_AT,
            FLAG_AT,
            "AT flag set on registration"
        );
        let id_len = u16::from_be_bytes([auth_data[53], auth_data[54]]) as usize;
        let cred_id = auth_data[55..55 + id_len].to_vec();
        assert_eq!(store.for_rp("example.com").len(), 1);

        // Assert with that credential (Touch ID prompt #2).
        let resp = handle(&store, &assertion_request("example.com", &cred_id));
        assert_eq!(resp[0], CTAP2_OK, "getAssertion status");
        let ga: Value = ciborium::from_reader(&resp[1..]).unwrap();
        let ga = as_map(&ga).unwrap();
        let sig = map_bytes(ga, 3).unwrap();
        assert!(
            sig.len() >= 8 && sig[0] == 0x30,
            "DER ECDSA signature present"
        );
        // Assertion authData: counter advanced to 1, no AT flag.
        let a = map_bytes(ga, 2).unwrap();
        assert_eq!(a[32], FLAG_UP | FLAG_UV);
        assert_eq!(&a[33..37], &[0, 0, 0, 1], "counter incremented");
    }
}
