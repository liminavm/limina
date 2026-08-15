// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The SPICE vdagent wire format: chunk framing, message framing, and the clipboard
//! subset we speak. Pure bytes-in/bytes-out — no I/O, no state beyond reassembly — so the
//! whole protocol is unit-testable without a VM.
//!
//! Verified against upstream source (2026-08-15), not memory:
//! - `spice-protocol/spice/vd_agent.h` — `VDIChunkHeader{port,size}` (8 B),
//!   `VDAgentMessage{protocol,type,opaque,size}` (20 B), `VD_AGENT_MAX_DATA_SIZE 2048`,
//!   the message/capability enumerations, and the `#if 0`-documented layout shifts the
//!   `CLIPBOARD_SELECTION` / `CLIPBOARD_GRAB_SERIAL` capabilities impose.
//! - `vd_agent/src/vdagentd/virtio-port.c` — the receiving state machine, which pins down
//!   three rules the header alone does not:
//!   1. `chunk_header.size > VD_AGENT_MAX_DATA_SIZE` is a hard error, and **the chunk
//!      header itself is not counted** in that size (`conn_handle_header`).
//!   2. The 20-byte message header is part of the chunked byte stream, so a chunk may
//!      split it (`vdagent_virtio_port_do_chunk` reassembles the header across chunks).
//!   3. A chunk may not extend past the end of the message it is carrying — the agent
//!      calls that "chunk larger than message, lost sync?" and drops the connection. So
//!      **never pack two messages into one chunk**; each message starts a fresh one.
//!
//! ## Capabilities we announce, and why not more
//!
//! `CLIPBOARD_BY_DEMAND` + `CLIPBOARD_SELECTION`, and deliberately **not** the legacy
//! `CLIPBOARD` (bit 3): by-demand is the grab/request/data handshake we want, the legacy
//! bit is the push-everything-on-copy predecessor, and a stock `spice-vdagent` does not
//! offer bit 3 either (measured in `spikes/m12-spice-port/`).
//!
//! `CLIPBOARD_GRAB_SERIAL` is **not** announced. It would add a `serial` field to grabs in
//! both directions, and the layout of a message the agent sends us is chosen from the
//! capabilities *we* announced — so announcing it changes the wire format, and the
//! host-side staleness ratchet does not need it: exactly one agent speaks on this port, so
//! our own monotonic counter already orders every exchange (unlike the control plane,
//! where one host faces N independently-numbering session peers).
//!
//! [`Selection`] is fixed at `CLIPBOARD` for now; `PRIMARY` (the X11 middle-click
//! selection) has no NSPasteboard counterpart worth bridging.

use anyhow::{bail, Result};

/// `VD_AGENT_PROTOCOL` — the only protocol version that exists.
pub const PROTOCOL: u32 = 1;

/// `VD_AGENT_MAX_DATA_SIZE` — the maximum bytes of *chunk payload*; the 8-byte chunk
/// header is on top of this, not inside it.
pub const MAX_CHUNK_DATA: usize = 2048;

/// `VDP_CLIENT_PORT` — the chunk port a SPICE client uses. (`VDP_END_PORT` is 3, so ports
/// 0..=2 are the legal range the agent will accept.)
pub const VDP_CLIENT_PORT: u32 = 1;

/// One past the last legal chunk port (`VDP_END_PORT`).
const VDP_END_PORT: u32 = 3;

/// `sizeof(VDAgentMessage)` — protocol + type + opaque + size.
const MESSAGE_HEADER_LEN: usize = 4 + 4 + 8 + 4;

/// `sizeof(VDIChunkHeader)`.
const CHUNK_HEADER_LEN: usize = 4 + 4;

/// A ceiling on a single reassembled message. The protocol has none — the guest declares
/// the size and we would allocate it — so this is the bound that keeps a hostile or
/// confused agent from making us allocate without limit. Far above any real clipboard
/// payload; a message past it is a lost-sync error, not a truncation.
const MAX_MESSAGE_DATA: usize = 16 * 1024 * 1024;

/// `VD_AGENT_*` message types (`vd_agent.h`; the enum starts at 1).
pub mod msg_type {
    pub const CLIPBOARD: u32 = 4;
    pub const ANNOUNCE_CAPABILITIES: u32 = 6;
    pub const CLIPBOARD_GRAB: u32 = 7;
    pub const CLIPBOARD_REQUEST: u32 = 8;
    pub const CLIPBOARD_RELEASE: u32 = 9;
    pub const CLIENT_DISCONNECTED: u32 = 13;
}

/// `VD_AGENT_CAP_*` bit indices (`vd_agent.h`; the enum starts at 0).
pub mod cap {
    /// The legacy push-on-copy clipboard. We never announce it — see the module docs.
    pub const CLIPBOARD: u32 = 3;
    pub const CLIPBOARD_BY_DEMAND: u32 = 5;
    pub const CLIPBOARD_SELECTION: u32 = 6;
    pub const CLIPBOARD_NO_RELEASE_ON_REGRAB: u32 = 16;
    pub const CLIPBOARD_GRAB_SERIAL: u32 = 17;
}

/// `VD_AGENT_CLIPBOARD_*` data formats. Text is the only one M12 carries.
pub const CLIPBOARD_UTF8_TEXT: u32 = 1;

/// `VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD` — the ordinary Ctrl-C/Ctrl-V selection.
pub const SELECTION_CLIPBOARD: u8 = 0;

/// The capability word we announce. One 32-bit word is enough: the agent derives the word
/// count from the message size (`VD_AGENT_CAPS_SIZE_FROM_MSG_SIZE`) and
/// `VD_AGENT_HAS_CAPABILITY` bounds-checks against it, so a short array is legal and every
/// bit we care about is below 32.
pub fn our_caps() -> u32 {
    (1 << cap::CLIPBOARD_BY_DEMAND) | (1 << cap::CLIPBOARD_SELECTION)
}

/// Whether `caps` (as announced by the agent) has bit `index` set.
pub fn has_cap(caps: &[u32], index: u32) -> bool {
    let word = (index / 32) as usize;
    word < caps.len() && caps[word] & (1 << (index % 32)) != 0
}

/// A decoded vdagent message — the clipboard subset, plus a catch-all so an unknown type
/// is ignored rather than treated as a framing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMessage {
    /// The agent's greeting (or its reply to ours). `request` means "announce back".
    AnnounceCapabilities { request: bool, caps: Vec<u32> },
    /// "I own the clipboard now, and here are the formats I can produce."
    ClipboardGrab { selection: u8, types: Vec<u32> },
    /// "Send me the clipboard in this format."
    ClipboardRequest { selection: u8, format: u32 },
    /// The answer to a request. An empty `data` with `format == NONE` is the agent's way
    /// of saying it could not produce the content.
    Clipboard {
        selection: u8,
        format: u32,
        data: Vec<u8>,
    },
    /// "I no longer own the clipboard."
    ClipboardRelease { selection: u8 },
    /// A type we do not handle (mouse state, monitors config, file transfer, …).
    Other { msg_type: u32 },
}

/// Encode `data` as a complete vdagent message, chunked for the wire.
///
/// One message per chunk run: the byte stream `VDAgentMessage || data` is split into
/// pieces of at most [`MAX_CHUNK_DATA`], each prefixed with its own chunk header, and the
/// last piece ends exactly at the message end (rule 3 in the module docs).
pub fn encode(msg_type: u32, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(MESSAGE_HEADER_LEN + data.len());
    body.extend_from_slice(&PROTOCOL.to_le_bytes());
    body.extend_from_slice(&msg_type.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes()); // opaque: unused in this direction
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);

    let mut out =
        Vec::with_capacity(body.len() + CHUNK_HEADER_LEN * body.len().div_ceil(MAX_CHUNK_DATA));
    for piece in body.chunks(MAX_CHUNK_DATA) {
        out.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        out.extend_from_slice(&(piece.len() as u32).to_le_bytes());
        out.extend_from_slice(piece);
    }
    out
}

/// `VD_AGENT_ANNOUNCE_CAPABILITIES` from us. `request` asks the agent to announce back —
/// set it when *we* open the dialogue, clear it when we are answering the agent's own
/// request (otherwise two polite peers ping-pong forever).
pub fn encode_announce(request: bool) -> Vec<u8> {
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&u32::from(request).to_le_bytes());
    data.extend_from_slice(&our_caps().to_le_bytes());
    encode(msg_type::ANNOUNCE_CAPABILITIES, &data)
}

/// `VD_AGENT_CLIPBOARD_GRAB` — the host took a copy; these are the formats we can serve.
pub fn encode_grab(selection: u8, types: &[u32]) -> Vec<u8> {
    let mut data = selection_prefix(selection);
    for t in types {
        data.extend_from_slice(&t.to_le_bytes());
    }
    encode(msg_type::CLIPBOARD_GRAB, &data)
}

/// `VD_AGENT_CLIPBOARD_REQUEST` — send us the guest's clipboard in `format`.
pub fn encode_request(selection: u8, format: u32) -> Vec<u8> {
    let mut data = selection_prefix(selection);
    data.extend_from_slice(&format.to_le_bytes());
    encode(msg_type::CLIPBOARD_REQUEST, &data)
}

/// `VD_AGENT_CLIPBOARD` — the content behind a grab the guest asked us to make good on.
pub fn encode_clipboard(selection: u8, format: u32, data: &[u8]) -> Vec<u8> {
    let mut body = selection_prefix(selection);
    body.extend_from_slice(&format.to_le_bytes());
    body.extend_from_slice(data);
    encode(msg_type::CLIPBOARD, &body)
}

/// `VD_AGENT_CLIPBOARD_RELEASE` — we no longer own the clipboard.
pub fn encode_release(selection: u8) -> Vec<u8> {
    encode(msg_type::CLIPBOARD_RELEASE, &selection_prefix(selection))
}

/// The 4-byte selection prefix the `CLIPBOARD_SELECTION` capability adds to every
/// clipboard message: `uint8_t selection` followed by 3 reserved bytes.
fn selection_prefix(selection: u8) -> Vec<u8> {
    vec![selection, 0, 0, 0]
}

/// Decode one reassembled message body.
///
/// `selection` says whether the `CLIPBOARD_SELECTION` capability is in force, because it
/// shifts the layout of every clipboard message by 4 bytes. It is a parameter rather than
/// a constant so a guest that never announced the capability still decodes correctly.
pub fn decode(msg_type: u32, data: &[u8], selection: bool) -> Result<AgentMessage> {
    let u32at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    match msg_type {
        msg_type::ANNOUNCE_CAPABILITIES => {
            if data.len() < 4 {
                bail!("ANNOUNCE_CAPABILITIES with {} bytes (need ≥4)", data.len());
            }
            let request = u32at(data, 0) != 0;
            let caps = data[4..]
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect();
            Ok(AgentMessage::AnnounceCapabilities { request, caps })
        }
        msg_type::CLIPBOARD_GRAB => {
            let (sel, rest) = split_selection(data, selection)?;
            let types = rest
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect();
            Ok(AgentMessage::ClipboardGrab {
                selection: sel,
                types,
            })
        }
        msg_type::CLIPBOARD_REQUEST => {
            let (sel, rest) = split_selection(data, selection)?;
            if rest.len() < 4 {
                bail!("CLIPBOARD_REQUEST with no format");
            }
            Ok(AgentMessage::ClipboardRequest {
                selection: sel,
                format: u32at(rest, 0),
            })
        }
        msg_type::CLIPBOARD => {
            let (sel, rest) = split_selection(data, selection)?;
            if rest.len() < 4 {
                bail!("CLIPBOARD with no format");
            }
            Ok(AgentMessage::Clipboard {
                selection: sel,
                format: u32at(rest, 0),
                data: rest[4..].to_vec(),
            })
        }
        msg_type::CLIPBOARD_RELEASE => {
            let (sel, _) = split_selection(data, selection)?;
            Ok(AgentMessage::ClipboardRelease { selection: sel })
        }
        other => Ok(AgentMessage::Other { msg_type: other }),
    }
}

/// Peel the optional selection prefix off a clipboard message body.
fn split_selection(data: &[u8], selection: bool) -> Result<(u8, &[u8])> {
    if !selection {
        return Ok((SELECTION_CLIPBOARD, data));
    }
    if data.len() < 4 {
        bail!(
            "clipboard message too short for a selection prefix ({} bytes)",
            data.len()
        );
    }
    Ok((data[0], &data[4..]))
}

/// The receive side: bytes off the port → whole messages.
///
/// Mirrors the agent's own state machine, including the parts that are easy to get wrong:
/// a chunk may split the message header, several chunks may carry one message, and a chunk
/// that would run past the end of its message is a lost-sync error rather than the start
/// of the next one.
#[derive(Default)]
pub struct Reassembler {
    /// Bytes received but not yet consumed as whole chunks.
    wire: Vec<u8>,
    /// Per-port partial message (the agent keeps one of these per port too).
    ports: [Partial; VDP_END_PORT as usize],
}

#[derive(Default)]
struct Partial {
    /// `VDAgentMessage` header bytes plus payload, as far as they have arrived.
    body: Vec<u8>,
    /// Declared payload size, once the header is complete.
    declared: Option<usize>,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    /// Feed `bytes` (any split — a single byte or a full buffer) and return every message
    /// that completed. An `Err` means the stream desynchronized and the port is no longer
    /// trustworthy; the caller should stop reading rather than try to resync.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<(u32, Vec<u8>)>> {
        self.wire.extend_from_slice(bytes);
        let mut done = Vec::new();
        let mut consumed = 0usize;

        while self.wire.len() - consumed >= CHUNK_HEADER_LEN {
            let h = &self.wire[consumed..consumed + CHUNK_HEADER_LEN];
            let port = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
            if size > MAX_CHUNK_DATA {
                bail!("chunk size {size} exceeds VD_AGENT_MAX_DATA_SIZE ({MAX_CHUNK_DATA})");
            }
            if port >= VDP_END_PORT {
                bail!("chunk port {port} out of range");
            }
            if self.wire.len() - consumed - CHUNK_HEADER_LEN < size {
                break; // the chunk body hasn't fully arrived yet
            }
            let start = consumed + CHUNK_HEADER_LEN;
            let chunk = self.wire[start..start + size].to_vec();
            consumed = start + size;

            if let Some(msg) = self.ports[port as usize].feed(&chunk)? {
                done.push(msg);
            }
        }

        self.wire.drain(..consumed);
        Ok(done)
    }
}

impl Partial {
    /// Absorb one chunk's payload; returns the message if it completed it.
    fn feed(&mut self, chunk: &[u8]) -> Result<Option<(u32, Vec<u8>)>> {
        self.body.extend_from_slice(chunk);

        if self.declared.is_none() && self.body.len() >= MESSAGE_HEADER_LEN {
            let b = &self.body;
            let protocol = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if protocol != PROTOCOL {
                bail!("message protocol {protocol} (expected {PROTOCOL})");
            }
            let size = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) as usize;
            if size > MAX_MESSAGE_DATA {
                bail!("message declares {size} bytes (max {MAX_MESSAGE_DATA})");
            }
            self.declared = Some(size);
        }

        let Some(declared) = self.declared else {
            return Ok(None); // still assembling the header
        };
        let total = MESSAGE_HEADER_LEN + declared;
        // The agent's "chunk larger than message, lost sync?" check: a chunk must end at
        // or before the message end, never spill into whatever follows.
        if self.body.len() > total {
            bail!(
                "chunk larger than message, lost sync? ({} bytes for a {total}-byte message)",
                self.body.len()
            );
        }
        if self.body.len() < total {
            return Ok(None);
        }

        let msg_type = u32::from_le_bytes([self.body[4], self.body[5], self.body[6], self.body[7]]);
        let data = self.body[MESSAGE_HEADER_LEN..].to_vec();
        self.body.clear();
        self.declared = None;
        Ok(Some((msg_type, data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a wire buffer the way the broker will: reassemble, then interpret.
    fn roundtrip(wire: &[u8]) -> Vec<AgentMessage> {
        let mut r = Reassembler::new();
        r.push(wire)
            .unwrap()
            .into_iter()
            .map(|(t, d)| decode(t, &d, true).unwrap())
            .collect()
    }

    #[test]
    fn a_small_message_is_one_chunk_and_survives_the_round_trip() {
        let wire = encode_request(SELECTION_CLIPBOARD, CLIPBOARD_UTF8_TEXT);
        // 8 chunk header + 20 message header + 4 selection + 4 format
        assert_eq!(wire.len(), 8 + 20 + 4 + 4);
        assert_eq!(
            roundtrip(&wire),
            vec![AgentMessage::ClipboardRequest {
                selection: SELECTION_CLIPBOARD,
                format: CLIPBOARD_UTF8_TEXT
            }]
        );
    }

    #[test]
    fn a_large_clipboard_splits_into_chunks_that_each_respect_the_2048_limit() {
        // Big enough to need several chunks, and deliberately not a multiple of 2048 so
        // the tail chunk is short.
        let text = "x".repeat(5000);
        let wire = encode_clipboard(SELECTION_CLIPBOARD, CLIPBOARD_UTF8_TEXT, text.as_bytes());

        // Walk the chunk headers and check each one against the agent's hard limit.
        let mut off = 0;
        let mut chunks = 0;
        while off < wire.len() {
            let size =
                u32::from_le_bytes([wire[off + 4], wire[off + 5], wire[off + 6], wire[off + 7]])
                    as usize;
            assert!(
                size <= MAX_CHUNK_DATA,
                "chunk of {size} bytes exceeds the limit"
            );
            assert_eq!(
                u32::from_le_bytes([wire[off], wire[off + 1], wire[off + 2], wire[off + 3]]),
                VDP_CLIENT_PORT
            );
            off += CHUNK_HEADER_LEN + size;
            chunks += 1;
        }
        assert_eq!(
            off,
            wire.len(),
            "chunk walk did not land exactly at the end"
        );
        assert!(chunks > 1, "a 5000-byte payload must span multiple chunks");

        assert_eq!(
            roundtrip(&wire),
            vec![AgentMessage::Clipboard {
                selection: SELECTION_CLIPBOARD,
                format: CLIPBOARD_UTF8_TEXT,
                data: text.into_bytes(),
            }]
        );
    }

    #[test]
    fn reassembly_survives_the_stream_arriving_one_byte_at_a_time() {
        // The nastiest split there is: it lands inside the chunk header, inside the
        // message header, and inside the payload.
        let text = "hello, clipboard".repeat(300);
        let wire = encode_clipboard(SELECTION_CLIPBOARD, CLIPBOARD_UTF8_TEXT, text.as_bytes());

        let mut r = Reassembler::new();
        let mut got = Vec::new();
        for b in &wire {
            got.extend(r.push(&[*b]).unwrap());
        }
        assert_eq!(got.len(), 1);
        assert_eq!(
            decode(got[0].0, &got[0].1, true).unwrap(),
            AgentMessage::Clipboard {
                selection: SELECTION_CLIPBOARD,
                format: CLIPBOARD_UTF8_TEXT,
                data: text.into_bytes(),
            }
        );
    }

    #[test]
    fn several_messages_in_one_read_all_come_out() {
        let mut wire = encode_announce(true);
        wire.extend(encode_grab(SELECTION_CLIPBOARD, &[CLIPBOARD_UTF8_TEXT]));
        wire.extend(encode_release(SELECTION_CLIPBOARD));

        assert_eq!(
            roundtrip(&wire),
            vec![
                AgentMessage::AnnounceCapabilities {
                    request: true,
                    caps: vec![our_caps()]
                },
                AgentMessage::ClipboardGrab {
                    selection: SELECTION_CLIPBOARD,
                    types: vec![CLIPBOARD_UTF8_TEXT]
                },
                AgentMessage::ClipboardRelease {
                    selection: SELECTION_CLIPBOARD
                },
            ]
        );
    }

    #[test]
    fn we_never_announce_the_legacy_clipboard_capability() {
        // Bit 3 is the push-on-copy predecessor of by-demand; announcing both is how you
        // get a guest that pushes every copy at you unasked.
        let caps = [our_caps()];
        assert!(has_cap(&caps, cap::CLIPBOARD_BY_DEMAND));
        assert!(has_cap(&caps, cap::CLIPBOARD_SELECTION));
        assert!(!has_cap(&caps, cap::CLIPBOARD));
        // Not announced: it would change the on-wire layout of grabs in both directions.
        assert!(!has_cap(&caps, cap::CLIPBOARD_GRAB_SERIAL));
    }

    #[test]
    fn has_cap_does_not_read_past_a_short_capability_array() {
        // The agent may send fewer words than the bit index implies; that must read as
        // "absent", not panic (VD_AGENT_HAS_CAPABILITY bounds-checks the same way).
        assert!(!has_cap(&[], cap::CLIPBOARD_BY_DEMAND));
        assert!(!has_cap(&[0, 0], 99));
    }

    #[test]
    fn an_oversized_chunk_is_a_lost_sync_error() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        wire.extend_from_slice(&((MAX_CHUNK_DATA + 1) as u32).to_le_bytes());
        wire.extend_from_slice(&vec![0u8; MAX_CHUNK_DATA + 1]);

        let err = Reassembler::new().push(&wire).unwrap_err().to_string();
        assert!(err.contains("exceeds VD_AGENT_MAX_DATA_SIZE"), "{err}");
    }

    #[test]
    fn a_chunk_running_past_its_message_is_a_lost_sync_error() {
        // A well-formed 0-byte message, then a chunk claiming more payload than the
        // message declares — the exact condition the agent calls "lost sync?".
        let mut wire = Vec::new();
        wire.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        wire.extend_from_slice(&(MESSAGE_HEADER_LEN as u32 + 4).to_le_bytes());
        wire.extend_from_slice(&PROTOCOL.to_le_bytes());
        wire.extend_from_slice(&msg_type::CLIPBOARD_RELEASE.to_le_bytes());
        wire.extend_from_slice(&0u64.to_le_bytes());
        wire.extend_from_slice(&0u32.to_le_bytes()); // declares zero payload…
        wire.extend_from_slice(&[0u8; 4]); // …but the chunk carries four bytes

        let err = Reassembler::new().push(&wire).unwrap_err().to_string();
        assert!(err.contains("lost sync"), "{err}");
    }

    #[test]
    fn a_wrong_protocol_version_is_rejected_rather_than_misparsed() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        wire.extend_from_slice(&(MESSAGE_HEADER_LEN as u32).to_le_bytes());
        wire.extend_from_slice(&7u32.to_le_bytes()); // not VD_AGENT_PROTOCOL
        wire.extend_from_slice(&msg_type::CLIPBOARD_RELEASE.to_le_bytes());
        wire.extend_from_slice(&0u64.to_le_bytes());
        wire.extend_from_slice(&0u32.to_le_bytes());

        let err = Reassembler::new().push(&wire).unwrap_err().to_string();
        assert!(err.contains("protocol 7"), "{err}");
    }

    #[test]
    fn a_declared_size_past_the_ceiling_is_refused_before_we_allocate_for_it() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
        wire.extend_from_slice(&(MESSAGE_HEADER_LEN as u32).to_le_bytes());
        wire.extend_from_slice(&PROTOCOL.to_le_bytes());
        wire.extend_from_slice(&msg_type::CLIPBOARD.to_le_bytes());
        wire.extend_from_slice(&0u64.to_le_bytes());
        wire.extend_from_slice(&u32::MAX.to_le_bytes());

        let err = Reassembler::new().push(&wire).unwrap_err().to_string();
        assert!(err.contains("max "), "{err}");
    }

    #[test]
    fn an_unknown_message_type_decodes_as_other_instead_of_failing() {
        // Mouse state, monitors config, file transfer — all things a stock agent may send
        // that we simply have no use for. They must not look like framing errors.
        let wire = encode(1 /* VD_AGENT_MOUSE_STATE */, &[0u8; 12]);
        assert_eq!(roundtrip(&wire), vec![AgentMessage::Other { msg_type: 1 }]);
    }

    #[test]
    fn a_message_without_the_selection_capability_decodes_unshifted() {
        // Same bytes, two readings: the 4-byte selection prefix exists only if the
        // capability was negotiated, so decoding must be told which world it is in.
        let mut data = Vec::new();
        data.extend_from_slice(&CLIPBOARD_UTF8_TEXT.to_le_bytes());
        data.extend_from_slice(b"hi");

        assert_eq!(
            decode(msg_type::CLIPBOARD, &data, false).unwrap(),
            AgentMessage::Clipboard {
                selection: SELECTION_CLIPBOARD,
                format: CLIPBOARD_UTF8_TEXT,
                data: b"hi".to_vec(),
            }
        );
    }

    #[test]
    fn an_empty_announce_payload_is_refused_rather_than_indexed_into() {
        let err = decode(msg_type::ANNOUNCE_CAPABILITIES, &[], true).unwrap_err();
        assert!(err.to_string().contains("need ≥4"), "{err}");
    }
}
