// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! CTAP2 request parsing for the virtual FIDO authenticator: turn the bytes a guest sends into a
//! typed request, or into the CTAP status that refuses it.
//!
//! Everything here is a pure function of the request bytes. Nothing reads the credential store,
//! mints a key or puts up a Touch ID sheet; [`super::ctap2`] does all of that with what this
//! returns. The split is what lets the parser be fuzzed (`fuzz/fuzz_targets/ctap2_request.rs`):
//! the guest controls every byte of a request, and the host authenticator is the one piece of
//! limina that holds a secret. So this module names nothing else in the crate, and must keep it
//! that way to stay reachable from the fuzz workspace (see `docs/design/in-crate-checkers.md`).
//!
//! Every refusal is decided here, in the order the commands have always checked them, so a
//! request is refused before the authenticator touches the store or the enclave.

use ciborium::value::Value;

// CTAP2 command bytes.
pub const CMD_MAKE_CREDENTIAL: u8 = 0x01;
pub const CMD_GET_ASSERTION: u8 = 0x02;
pub const CMD_GET_INFO: u8 = 0x04;

// CTAP2 status codes this module refuses with.
pub const CTAP1_ERR_INVALID_COMMAND: u8 = 0x01;
pub const CTAP1_ERR_INVALID_LENGTH: u8 = 0x03;
pub const CTAP2_ERR_INVALID_CBOR: u8 = 0x12;
pub const CTAP2_ERR_MISSING_PARAMETER: u8 = 0x14;
pub const CTAP2_ERR_UNSUPPORTED_ALGORITHM: u8 = 0x26;
pub const CTAP2_ERR_INVALID_OPTION: u8 = 0x2C;

/// ES256 COSE algorithm id.
pub const ALG_ES256: i64 = -7;

/// One CTAP2 request the authenticator serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    GetInfo,
    MakeCredential(MakeCredential),
    GetAssertion(GetAssertion),
}

/// `authenticatorMakeCredential`, as far as the authenticator acts on it. ES256 was offered and
/// `up` was not refused, or parsing would have said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakeCredential {
    /// Key 1.
    pub client_data_hash: Vec<u8>,
    /// Key 2, `rp.id`.
    pub rp_id: String,
    /// Key 3, `user.id`.
    pub user_handle: Vec<u8>,
    /// Key 3, `user.name`; empty when absent.
    pub user_name: String,
    /// Key 5, the excludeList: the ids of its descriptors that carry one. Empty when absent.
    pub exclude: Vec<Vec<u8>>,
}

/// `authenticatorGetAssertion`, as far as the authenticator acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetAssertion {
    /// Key 1.
    pub rp_id: String,
    /// Key 2.
    pub client_data_hash: Vec<u8>,
    /// Key 3, the allowList, when it is a non-empty array: the ids of its descriptors that carry
    /// one. `None` means pick the newest resident credential for the RP. A non-empty list whose
    /// descriptors carry no id is `Some(vec![])`, which matches nothing, and is not the same as
    /// no list at all.
    pub allow: Option<Vec<Vec<u8>>>,
    /// Key 5, `options.up`, if the platform sent one. `Some(false)` is a silent pre-flight.
    pub up: Option<bool>,
}

/// Parse one CTAP2 message (`payload[0]` = command, rest = CBOR) into the request it asks for,
/// or the status that refuses it.
pub fn parse(payload: &[u8]) -> Result<Request, u8> {
    let Some((&cmd, cbor)) = payload.split_first() else {
        return Err(CTAP1_ERR_INVALID_LENGTH);
    };
    match cmd {
        CMD_GET_INFO => Ok(Request::GetInfo),
        CMD_MAKE_CREDENTIAL => parse_make_credential(cbor).map(Request::MakeCredential),
        CMD_GET_ASSERTION => parse_get_assertion(cbor).map(Request::GetAssertion),
        _ => Err(CTAP1_ERR_INVALID_COMMAND),
    }
}

pub fn parse_make_credential(cbor: &[u8]) -> Result<MakeCredential, u8> {
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
    let user_name = text_key_text(user, "name").unwrap_or("");

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

    let exclude = map_get(&root, 5)
        .and_then(as_array)
        .map(descriptor_ids)
        .unwrap_or_default();

    Ok(MakeCredential {
        client_data_hash: client_data_hash.to_vec(),
        rp_id: rp_id.to_string(),
        user_handle: user_handle.to_vec(),
        user_name: user_name.to_string(),
        exclude,
    })
}

pub fn parse_get_assertion(cbor: &[u8]) -> Result<GetAssertion, u8> {
    let root = parse_map(cbor)?;
    let rp_id = map_text(&root, 1).ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let client_data_hash = map_bytes(&root, 2).ok_or(CTAP2_ERR_MISSING_PARAMETER)?;
    let allow = match map_get(&root, 3).and_then(as_array) {
        Some(list) if !list.is_empty() => Some(descriptor_ids(list)),
        _ => None,
    };
    Ok(GetAssertion {
        rp_id: rp_id.to_string(),
        client_data_hash: client_data_hash.to_vec(),
        allow,
        up: requested_up(&root, 5),
    })
}

/// The ids of a credential-descriptor array's entries, skipping any that carry none.
fn descriptor_ids(list: &[Value]) -> Vec<Vec<u8>> {
    list.iter()
        .filter_map(|d| as_map(d).and_then(|m| text_key_bytes(m, "id")))
        .map(<[u8]>::to_vec)
        .collect()
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

// --- CBOR request parsing (ciborium Value helpers) -------------------------

pub fn parse_map(cbor: &[u8]) -> Result<Vec<(Value, Value)>, u8> {
    let v: Value = ciborium::from_reader(cbor).map_err(|_| CTAP2_ERR_INVALID_CBOR)?;
    match v {
        Value::Map(m) => Ok(m),
        _ => Err(CTAP2_ERR_INVALID_CBOR),
    }
}

pub fn as_int(v: &Value) -> Option<i128> {
    match v {
        Value::Integer(i) => Some((*i).into()),
        _ => None,
    }
}
pub fn as_bytes(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Bytes(b) => Some(b),
        _ => None,
    }
}
pub fn as_text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        _ => None,
    }
}
pub fn as_map(v: &Value) -> Option<&[(Value, Value)]> {
    match v {
        Value::Map(m) => Some(m),
        _ => None,
    }
}
pub fn as_array(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

/// Look up an integer-keyed entry (CTAP2 request top level).
pub fn map_get(m: &[(Value, Value)], key: i128) -> Option<&Value> {
    m.iter()
        .find(|(k, _)| as_int(k) == Some(key))
        .map(|(_, v)| v)
}
pub fn map_bytes(m: &[(Value, Value)], key: i128) -> Option<&[u8]> {
    map_get(m, key).and_then(as_bytes)
}
pub fn map_text(m: &[(Value, Value)], key: i128) -> Option<&str> {
    map_get(m, key).and_then(as_text)
}

/// Look up a text-keyed entry (nested maps: rp, user, pubKeyCredParams items).
pub fn text_key<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    m.iter()
        .find(|(k, _)| as_text(k) == Some(key))
        .map(|(_, v)| v)
}
pub fn text_key_bytes<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a [u8]> {
    text_key(m, key).and_then(as_bytes)
}
pub fn text_key_text<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a str> {
    text_key(m, key).and_then(as_text)
}
pub fn text_key_int(m: &[(Value, Value)], key: &str) -> Option<i64> {
    text_key(m, key)
        .and_then(as_int)
        .and_then(|i| i64::try_from(i).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cbor(v: Value) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::into_writer(&v, &mut out).unwrap();
        out
    }

    fn int(i: i64) -> Value {
        Value::Integer(i.into())
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn descriptor(id: Option<&[u8]>) -> Value {
        let mut m = vec![(text("type"), text("public-key"))];
        if let Some(id) = id {
            m.push((text("id"), Value::Bytes(id.to_vec())));
        }
        Value::Map(m)
    }

    fn assertion(allow: Option<Vec<Value>>) -> Vec<u8> {
        let mut m = vec![
            (int(1), text("example.com")),
            (int(2), Value::Bytes(vec![0; 32])),
        ];
        if let Some(list) = allow {
            m.push((int(3), Value::Array(list)));
        }
        cbor(Value::Map(m))
    }

    /// A non-empty allowList whose descriptors carry no id names no credential, and must not
    /// fall back to the RP's resident credentials the way an absent or empty list does.
    #[test]
    fn an_allow_list_without_ids_is_not_an_absent_one() {
        let allow = |list| parse_get_assertion(&assertion(list)).unwrap().allow;
        assert_eq!(allow(None), None);
        assert_eq!(allow(Some(vec![])), None);
        assert_eq!(allow(Some(vec![descriptor(None)])), Some(vec![]));
        assert_eq!(
            allow(Some(vec![descriptor(None), descriptor(Some(b"c1"))])),
            Some(vec![b"c1".to_vec()])
        );
    }

    #[test]
    fn a_request_refuses_before_it_is_parsed_further() {
        assert_eq!(parse(&[]), Err(CTAP1_ERR_INVALID_LENGTH));
        assert_eq!(parse(&[0x7f]), Err(CTAP1_ERR_INVALID_COMMAND));
        assert_eq!(parse(&[CMD_GET_INFO, 0xff]), Ok(Request::GetInfo));
        assert_eq!(
            parse(&[CMD_GET_ASSERTION, 0xff]),
            Err(CTAP2_ERR_INVALID_CBOR)
        );
        assert_eq!(
            parse(&[CMD_MAKE_CREDENTIAL, 0x01]),
            Err(CTAP2_ERR_INVALID_CBOR)
        );
    }
}
