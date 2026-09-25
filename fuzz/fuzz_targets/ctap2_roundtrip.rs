// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The virtual FIDO authenticator's request parser against a model of the rules it enforces.
//!
//! Random bytes rarely form a CBOR map with the right keys, so this builds well-formed requests
//! from fields the fuzzer picks, encodes them, and checks that `parse` gives back exactly the
//! request those fields describe, or exactly the status the rules call for: no ES256 on offer
//! is `UNSUPPORTED_ALGORITHM`, `up: false` on a registration is `INVALID_OPTION`, descriptors
//! without an id are skipped, and a non-empty allowList is never read as an absent one.

#![no_main]

use arbitrary::Arbitrary;
use ciborium::value::Value;
use libfuzzer_sys::fuzz_target;
use limina_fuzz::ctap2_request::*;

#[derive(Arbitrary, Debug)]
enum Input {
    Make {
        client_data_hash: Vec<u8>,
        rp_id: String,
        user_handle: Vec<u8>,
        user_name: Option<String>,
        algs: Vec<i64>,
        exclude: Option<Vec<Option<Vec<u8>>>>,
        up: Option<bool>,
        reversed: bool,
    },
    Get {
        rp_id: String,
        client_data_hash: Vec<u8>,
        allow: Option<Vec<Option<Vec<u8>>>>,
        up: Option<bool>,
        reversed: bool,
    },
}

fn int(i: i64) -> Value {
    Value::Integer(i.into())
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn descriptors(list: &[Option<Vec<u8>>]) -> Value {
    Value::Array(
        list.iter()
            .map(|id| {
                let mut m = vec![(text("type"), text("public-key"))];
                if let Some(id) = id {
                    m.push((text("id"), Value::Bytes(id.clone())));
                }
                Value::Map(m)
            })
            .collect(),
    )
}

fn options(up: Option<bool>) -> Option<Value> {
    up.map(|up| Value::Map(vec![(text("up"), Value::Bool(up))]))
}

fn ids(list: &[Option<Vec<u8>>]) -> Vec<Vec<u8>> {
    list.iter().flatten().cloned().collect()
}

/// Encode the map, in canonical key order or reversed: the parser must not care which.
fn request(cmd: u8, mut m: Vec<(Value, Value)>, reversed: bool) -> Vec<u8> {
    if reversed {
        m.reverse();
    }
    let mut out = vec![cmd];
    ciborium::into_writer(&Value::Map(m), &mut out).expect("a map encodes");
    out
}

fuzz_target!(|input: Input| {
    match input {
        Input::Make {
            client_data_hash,
            rp_id,
            user_handle,
            user_name,
            algs,
            exclude,
            up,
            reversed,
        } => {
            let mut user = vec![(text("id"), Value::Bytes(user_handle.clone()))];
            if let Some(name) = &user_name {
                user.push((text("name"), text(name)));
            }
            let params = algs
                .iter()
                .map(|&alg| {
                    Value::Map(vec![
                        (text("alg"), int(alg)),
                        (text("type"), text("public-key")),
                    ])
                })
                .collect();
            let mut m = vec![
                (int(1), Value::Bytes(client_data_hash.clone())),
                (int(2), Value::Map(vec![(text("id"), text(&rp_id))])),
                (int(3), Value::Map(user)),
                (int(4), Value::Array(params)),
            ];
            if let Some(list) = &exclude {
                m.push((int(5), descriptors(list)));
            }
            if let Some(opts) = options(up) {
                m.push((int(7), opts));
            }

            let want = if !algs.contains(&ALG_ES256) {
                Err(CTAP2_ERR_UNSUPPORTED_ALGORITHM)
            } else if up == Some(false) {
                Err(CTAP2_ERR_INVALID_OPTION)
            } else {
                Ok(Request::MakeCredential(MakeCredential {
                    client_data_hash,
                    rp_id,
                    user_handle,
                    user_name: user_name.unwrap_or_default(),
                    exclude: exclude.as_deref().map(ids).unwrap_or_default(),
                }))
            };
            assert_eq!(parse(&request(CMD_MAKE_CREDENTIAL, m, reversed)), want);
        }
        Input::Get {
            rp_id,
            client_data_hash,
            allow,
            up,
            reversed,
        } => {
            let mut m = vec![
                (int(1), text(&rp_id)),
                (int(2), Value::Bytes(client_data_hash.clone())),
            ];
            if let Some(list) = &allow {
                m.push((int(3), descriptors(list)));
            }
            if let Some(opts) = options(up) {
                m.push((int(5), opts));
            }

            let want = Ok(Request::GetAssertion(GetAssertion {
                rp_id,
                client_data_hash,
                allow: allow.filter(|l| !l.is_empty()).as_deref().map(ids),
                up,
            }));
            assert_eq!(parse(&request(CMD_GET_ASSERTION, m, reversed)), want);
        }
    }
});
