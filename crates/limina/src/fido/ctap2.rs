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

/// ES256 COSE algorithm id.
const ALG_ES256: i64 = -7;

/// Our AAGUID (16 bytes). Self-attested authenticators may pick any stable value.
const AAGUID: [u8; 16] = *b"limina-touchid!!";

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

    // Response: {1: credential, 2: authData, 3: signature, 4: user}.
    let mut w = CborWriter::new();
    w.map_header(4);
    w.uint(1);
    w.map_header(2);
    w.text("id");
    w.bytes(&cred.cred_id);
    w.text("type");
    w.text("public-key");
    w.uint(2);
    w.bytes(&auth_data);
    w.uint(3);
    w.bytes(&sig);
    w.uint(4);
    w.map_header(1);
    w.text("id");
    w.bytes(&cred.user_handle);
    Ok(w.finish())
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
