// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Bytes in, replies out. No I/O and no state beyond the line accumulator, so the whole
//! wire format is unit-testable without a VM.
//!
//! The protocol is QMP-shaped: one JSON object per line each way. Two details are not
//! obvious from the shape and both are handled here — the `0xFF` sentinel that
//! `guest-sync-delimited` prepends to its reply (everything before it is stale output from
//! a previous client and must be dropped), and the fact that an error is a *reply*
//! (`{"error": {...}}`), not a transport failure.

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// The byte `qemu-ga` prepends to a `guest-sync-delimited` reply (`qga/main.c:672-675`),
/// and the byte we send ahead of that request to break its JSON parser out of any partial
/// message a previous client left behind.
pub const SENTINEL: u8 = 0xFF;

/// Longest reply line we will accumulate. Nothing the host asks for in anger is close to
/// this; the bound exists so a wedged or hostile guest cannot grow our buffer without end.
/// (`guest-file-read` caps at 48 MB, so a future milestone reading files will want its own
/// larger, per-call bound rather than raising this one.)
pub const MAX_LINE: usize = 1 << 20;

/// An error the *agent* returned — a well-formed reply saying the command failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestError {
    pub class: String,
    pub desc: String,
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class, self.desc)
    }
}

impl std::error::Error for GuestError {}

/// Encode one request. `args` is the `arguments` object, or `None` for a bare command.
pub fn request(cmd: &str, args: Option<Value>) -> Vec<u8> {
    let mut v = match args {
        Some(args) => json!({ "execute": cmd, "arguments": args }),
        None => json!({ "execute": cmd }),
    }
    .to_string()
    .into_bytes();
    v.push(b'\n');
    v
}

/// Encode the resync request: a leading sentinel to reset the agent's parser, then
/// `guest-sync-delimited` carrying `id`, whose reply comes back sentinel-prefixed.
pub fn sync_request(id: i64) -> Vec<u8> {
    let mut v = vec![SENTINEL];
    v.extend_from_slice(&request("guest-sync-delimited", Some(json!({ "id": id }))));
    v
}

/// Parse one reply line into the agent's answer.
///
/// The outer `Result` is *our* framing/JSON problem; the inner one is the agent saying the
/// command failed, which is a normal outcome (a blocked RPC, an unfreezable filesystem).
pub fn parse_reply(line: &[u8]) -> Result<std::result::Result<Value, GuestError>> {
    let v: Value = serde_json::from_slice(line).map_err(|e| {
        anyhow::anyhow!("unparseable reply ({e}): {}", String::from_utf8_lossy(line))
    })?;
    if let Some(err) = v.get("error") {
        return Ok(Err(GuestError {
            class: err
                .get("class")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_string(),
            desc: err
                .get("desc")
                .and_then(Value::as_str)
                .unwrap_or("(no description)")
                .to_string(),
        }));
    }
    match v.get("return") {
        Some(ret) => Ok(Ok(ret.clone())),
        None => bail!(
            "reply has neither `return` nor `error`: {}",
            String::from_utf8_lossy(line)
        ),
    }
}

/// Splits the inbound byte stream into reply lines, honoring the sentinel.
#[derive(Default)]
pub struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    pub fn new() -> Lines {
        Lines::default()
    }

    /// Feed freshly read bytes; returns every complete line they completed.
    ///
    /// A sentinel byte discards everything before it: that is precisely its purpose, and it
    /// is why a stale half-line from a previous supervisor cannot desynchronize us the way
    /// it would a plain line-splitter. `0xFF` is not valid UTF-8, so it can never appear
    /// inside a legitimate reply.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(bytes);
        if let Some(pos) = self.buf.iter().rposition(|&b| b == SENTINEL) {
            self.buf.drain(..=pos);
        }

        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).take(nl).collect();
            if !line.iter().all(u8::is_ascii_whitespace) {
                out.push(line);
            }
        }
        if self.buf.len() > MAX_LINE {
            let n = self.buf.len();
            self.buf.clear();
            bail!("a reply line exceeded {MAX_LINE} bytes ({n} buffered); dropping the stream");
        }
        Ok(out)
    }

    /// Forget any partial line — used when a call times out, because the bytes still in
    /// flight belong to a question nobody is waiting for any more.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_one_json_line() {
        let bytes = request("guest-ping", None);
        assert_eq!(bytes.last(), Some(&b'\n'));
        let v: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(v["execute"], "guest-ping");
        assert!(v.get("arguments").is_none());

        let bytes = request("guest-set-time", Some(json!({ "time": 42 })));
        let v: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(v["arguments"]["time"], 42);
    }

    #[test]
    fn the_sync_request_leads_with_the_sentinel() {
        let bytes = sync_request(7);
        assert_eq!(bytes[0], SENTINEL);
        let v: Value = serde_json::from_slice(&bytes[1..bytes.len() - 1]).unwrap();
        assert_eq!(v["execute"], "guest-sync-delimited");
        assert_eq!(v["arguments"]["id"], 7);
    }

    #[test]
    fn a_reply_carries_either_return_or_error() {
        let ok = parse_reply(br#"{"return": {"time": 5}}"#).unwrap().unwrap();
        assert_eq!(ok["time"], 5);

        let err = parse_reply(br#"{"error": {"class": "CommandNotFound", "desc": "nope"}}"#)
            .unwrap()
            .unwrap_err();
        assert_eq!(err.class, "CommandNotFound");
        assert_eq!(err.desc, "nope");

        assert!(parse_reply(b"{}").is_err());
        assert!(parse_reply(b"not json").is_err());
    }

    #[test]
    fn the_sentinel_discards_everything_before_it() {
        let mut lines = Lines::new();
        // A stale, half-delivered reply from a previous client, then our sync's sentinel.
        let mut bytes = br#"{"return": {"stale":"#.to_vec();
        bytes.push(SENTINEL);
        bytes.extend_from_slice(br#"{"return": 7}"#);
        bytes.push(b'\n');
        let got = lines.push(&bytes).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(parse_reply(&got[0]).unwrap().unwrap(), json!(7));
    }

    #[test]
    fn lines_reassemble_across_reads_and_split_within_one() {
        let mut lines = Lines::new();
        assert!(lines.push(br#"{"return":"#).unwrap().is_empty());
        let got = lines.push(b" 1}\n{\"return\": 2}\n").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(parse_reply(&got[0]).unwrap().unwrap(), json!(1));
        assert_eq!(parse_reply(&got[1]).unwrap().unwrap(), json!(2));
    }

    #[test]
    fn an_unbounded_partial_line_is_refused() {
        let mut lines = Lines::new();
        let flood = vec![b'x'; MAX_LINE + 1];
        assert!(lines.push(&flood).is_err());
        // …and the buffer is dropped, so the next sentinel resyncs from a clean slate.
        let mut bytes = vec![SENTINEL];
        bytes.extend_from_slice(b"{\"return\": 3}\n");
        let got = lines.push(&bytes).unwrap();
        assert_eq!(parse_reply(&got[0]).unwrap().unwrap(), json!(3));
    }
}
