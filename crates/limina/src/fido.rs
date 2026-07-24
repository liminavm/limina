// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host side of the virtual FIDO authenticator (M14, Spike B tier).
//!
//! Speaks CTAPHID over 64-byte reports carried by `Message::FidoReport` frames on
//! `CHANNEL_FIDO`. This spike-level implementation handles the transport layer
//! completely (channel allocation, packet reassembly, chunked responses) plus the
//! minimum CTAP2 surface for tooling to recognize a live FIDO2 authenticator:
//! `CTAPHID_INIT`, `CTAPHID_PING`, and `authenticatorGetInfo`. Everything else
//! answers a proper CTAPHID/CTAP2 error instead of wedging the channel.
//!
//! The real authenticator (makeCredential/getAssertion on Secure-Enclave keys behind
//! a Touch ID prompt — Spike A proved the primitive) plugs in at [`dispatch`]'s
//! `CMD_CBOR` arm; the framing below stays as-is for both the uhid transport and the
//! future emulated-USB one (see roadmap M14).
//!
//! One instance per control-plane peer: CTAPHID channel ids are per-device state and
//! the agent creates one uhid device per connection.

/// CTAPHID reports are always this size (no report ids).
pub const REPORT_SIZE: usize = 64;

const BROADCAST_CID: u32 = 0xFFFF_FFFF;
/// Max payload of an initialization packet / continuation packet.
const INIT_DATA: usize = REPORT_SIZE - 7;
const CONT_DATA: usize = REPORT_SIZE - 5;
/// CTAPHID limit: one init packet + 128 continuation packets.
const MAX_MSG: usize = INIT_DATA + 128 * CONT_DATA;

const CMD_PING: u8 = 0x81;
const CMD_INIT: u8 = 0x86;
const CMD_CBOR: u8 = 0x90;
const CMD_ERROR: u8 = 0xBF;

const ERR_INVALID_CMD: u8 = 0x01;
const ERR_INVALID_SEQ: u8 = 0x04;
const ERR_INVALID_CHANNEL: u8 = 0x0B;

/// Capability flags in the INIT response: CBOR (CTAP2) supported, CTAPHID_MSG
/// (U2F/CTAP1) NOT supported.
const CAP_CBOR: u8 = 0x04;
const CAP_NMSG: u8 = 0x08;

/// CTAP2 command byte for authenticatorGetInfo.
const CTAP2_GET_INFO: u8 = 0x04;
/// CTAP2 status: success / invalid command.
const CTAP2_OK: u8 = 0x00;
const CTAP2_ERR_INVALID_COMMAND: u8 = 0x01;

/// Our AAGUID (16 bytes, fixed). Self-attested authenticators may use any stable id.
const AAGUID: &[u8; 16] = b"limina-touchid!!";

/// An in-progress multi-packet CTAPHID message.
struct Reassembly {
    cid: u32,
    cmd: u8,
    total: usize,
    buf: Vec<u8>,
    next_seq: u8,
}

/// Per-peer CTAPHID state machine. Feed guest→host reports into [`Self::on_report`];
/// it returns the host→guest reports to send back (possibly none, e.g. mid-reassembly).
pub struct FidoAuthenticator {
    next_cid: u32,
    rx: Option<Reassembly>,
}

impl Default for FidoAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

impl FidoAuthenticator {
    pub fn new() -> FidoAuthenticator {
        FidoAuthenticator {
            next_cid: 1,
            rx: None,
        }
    }

    /// Process one 64-byte report from the guest. Short reports are padded (the uhid
    /// path always delivers full frames; be tolerant anyway).
    pub fn on_report(&mut self, report: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
        let mut frame = [0u8; REPORT_SIZE];
        let n = report.len().min(REPORT_SIZE);
        frame[..n].copy_from_slice(&report[..n]);

        let cid = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let b4 = frame[4];
        if b4 & 0x80 != 0 {
            // Initialization packet: cmd, 2-byte length, payload.
            let cmd = b4;
            let total = u16::from_be_bytes([frame[5], frame[6]]) as usize;
            if total > MAX_MSG {
                return vec![error_report(cid, ERR_INVALID_CMD)];
            }
            let take = total.min(INIT_DATA);
            let buf = frame[7..7 + take].to_vec();
            if buf.len() == total {
                self.rx = None;
                return self.dispatch(cid, cmd, &buf);
            }
            // A new init packet aborts any unfinished reassembly (CTAPHID re-sync).
            self.rx = Some(Reassembly {
                cid,
                cmd,
                total,
                buf,
                next_seq: 0,
            });
            Vec::new()
        } else {
            // Continuation packet.
            let Some(rx) = self.rx.as_mut() else {
                return vec![error_report(cid, ERR_INVALID_CHANNEL)];
            };
            if rx.cid != cid {
                return vec![error_report(cid, ERR_INVALID_CHANNEL)];
            }
            if b4 != rx.next_seq {
                self.rx = None;
                return vec![error_report(cid, ERR_INVALID_SEQ)];
            }
            rx.next_seq = rx.next_seq.wrapping_add(1);
            let take = (rx.total - rx.buf.len()).min(CONT_DATA);
            rx.buf.extend_from_slice(&frame[5..5 + take]);
            if rx.buf.len() == rx.total {
                let rx = self.rx.take().unwrap();
                return self.dispatch(rx.cid, rx.cmd, &rx.buf);
            }
            Vec::new()
        }
    }

    /// Handle one complete CTAPHID message.
    fn dispatch(&mut self, cid: u32, cmd: u8, payload: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
        match cmd {
            CMD_INIT => {
                if payload.len() < 8 {
                    return vec![error_report(cid, ERR_INVALID_CMD)];
                }
                if cid == BROADCAST_CID {
                    // Allocate a channel: nonce echo + new CID + version + capabilities.
                    let new_cid = self.next_cid;
                    self.next_cid = self.next_cid.wrapping_add(1).max(1);
                    let mut resp = Vec::with_capacity(17);
                    resp.extend_from_slice(&payload[..8]);
                    resp.extend_from_slice(&new_cid.to_be_bytes());
                    resp.push(2); // CTAPHID protocol version
                    resp.extend_from_slice(&[0, 1, 0]); // device version major/minor/build
                    resp.push(CAP_CBOR | CAP_NMSG);
                    packetize(cid, CMD_INIT, &resp)
                } else {
                    // INIT on an existing channel = re-sync: same CID back.
                    let mut resp = Vec::with_capacity(17);
                    resp.extend_from_slice(&payload[..8]);
                    resp.extend_from_slice(&cid.to_be_bytes());
                    resp.push(2);
                    resp.extend_from_slice(&[0, 1, 0]);
                    resp.push(CAP_CBOR | CAP_NMSG);
                    packetize(cid, CMD_INIT, &resp)
                }
            }
            CMD_PING => packetize(cid, CMD_PING, payload),
            CMD_CBOR => {
                let resp = match payload.first() {
                    Some(&CTAP2_GET_INFO) => get_info_response(),
                    _ => vec![CTAP2_ERR_INVALID_COMMAND],
                };
                packetize(cid, CMD_CBOR, &resp)
            }
            _ => vec![error_report(cid, ERR_INVALID_CMD)],
        }
    }
}

/// authenticatorGetInfo: status OK + a minimal CBOR map — versions=["FIDO_2_0"],
/// aaguid. Hand-encoded (the payload is small and fixed; a CBOR crate arrives with
/// the real CTAP2 core).
fn get_info_response() -> Vec<u8> {
    let mut r = vec![CTAP2_OK];
    r.push(0xA2); // map(2)
    r.push(0x01); // key 1: versions
    r.push(0x81); // array(1)
    r.push(0x68); // text(8)
    r.extend_from_slice(b"FIDO_2_0");
    r.push(0x03); // key 3: aaguid
    r.push(0x50); // bytes(16)
    r.extend_from_slice(AAGUID);
    r
}

/// One CTAPHID_ERROR report.
fn error_report(cid: u32, code: u8) -> [u8; REPORT_SIZE] {
    let mut f = [0u8; REPORT_SIZE];
    f[..4].copy_from_slice(&cid.to_be_bytes());
    f[4] = CMD_ERROR;
    f[6] = 1; // BCNT
    f[7] = code;
    f
}

/// Split one message into an init packet + continuation packets.
fn packetize(cid: u32, cmd: u8, payload: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
    let mut out = Vec::new();
    let mut frame = [0u8; REPORT_SIZE];
    frame[..4].copy_from_slice(&cid.to_be_bytes());
    frame[4] = cmd;
    frame[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    let first = payload.len().min(INIT_DATA);
    frame[7..7 + first].copy_from_slice(&payload[..first]);
    out.push(frame);
    let mut sent = first;
    let mut seq = 0u8;
    while sent < payload.len() {
        let mut cont = [0u8; REPORT_SIZE];
        cont[..4].copy_from_slice(&cid.to_be_bytes());
        cont[4] = seq;
        let take = (payload.len() - sent).min(CONT_DATA);
        cont[5..5 + take].copy_from_slice(&payload[sent..sent + take]);
        out.push(cont);
        sent += take;
        seq = seq.wrapping_add(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_frame(cid: u32, cmd: u8, payload: &[u8]) -> [u8; REPORT_SIZE] {
        assert!(payload.len() <= INIT_DATA);
        let mut f = [0u8; REPORT_SIZE];
        f[..4].copy_from_slice(&cid.to_be_bytes());
        f[4] = cmd;
        f[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        f[7..7 + payload.len()].copy_from_slice(payload);
        f
    }

    /// Broadcast INIT allocates a channel and echoes the nonce.
    #[test]
    fn init_allocates_channel() {
        let mut auth = FidoAuthenticator::new();
        let nonce = [9u8, 8, 7, 6, 5, 4, 3, 2];
        let out = auth.on_report(&init_frame(BROADCAST_CID, CMD_INIT, &nonce));
        assert_eq!(out.len(), 1);
        let f = out[0];
        assert_eq!(&f[..4], &BROADCAST_CID.to_be_bytes());
        assert_eq!(f[4], CMD_INIT);
        assert_eq!(u16::from_be_bytes([f[5], f[6]]), 17);
        assert_eq!(&f[7..15], &nonce);
        let cid = u32::from_be_bytes([f[15], f[16], f[17], f[18]]);
        assert_ne!(cid, 0);
        assert_ne!(cid, BROADCAST_CID);
        assert_eq!(f[19], 2); // CTAPHID version
        assert_eq!(f[23], CAP_CBOR | CAP_NMSG);
    }

    fn open_channel(auth: &mut FidoAuthenticator) -> u32 {
        let out = auth.on_report(&init_frame(BROADCAST_CID, CMD_INIT, &[0u8; 8]));
        let f = out[0];
        u32::from_be_bytes([f[15], f[16], f[17], f[18]])
    }

    /// PING echoes on the allocated channel.
    #[test]
    fn ping_echoes() {
        let mut auth = FidoAuthenticator::new();
        let cid = open_channel(&mut auth);
        let out = auth.on_report(&init_frame(cid, CMD_PING, b"hello"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_PING);
        assert_eq!(u16::from_be_bytes([out[0][5], out[0][6]]), 5);
        assert_eq!(&out[0][7..12], b"hello");
    }

    /// A multi-packet PING reassembles and the echo chunks back out.
    #[test]
    fn multipacket_ping_roundtrips() {
        let mut auth = FidoAuthenticator::new();
        let cid = open_channel(&mut auth);
        let payload: Vec<u8> = (0..200u16).map(|i| i as u8).collect();

        // Send: init packet + continuations, hand-framed.
        let mut sent = INIT_DATA;
        let mut f = [0u8; REPORT_SIZE];
        f[..4].copy_from_slice(&cid.to_be_bytes());
        f[4] = CMD_PING;
        f[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        f[7..].copy_from_slice(&payload[..INIT_DATA]);
        assert!(auth.on_report(&f).is_empty());
        let mut seq = 0u8;
        let mut responses = Vec::new();
        while sent < payload.len() {
            let take = (payload.len() - sent).min(CONT_DATA);
            let mut c = [0u8; REPORT_SIZE];
            c[..4].copy_from_slice(&cid.to_be_bytes());
            c[4] = seq;
            c[5..5 + take].copy_from_slice(&payload[sent..sent + take]);
            responses = auth.on_report(&c);
            sent += take;
            seq += 1;
        }

        // Echo comes back as init + continuation packets carrying the same payload.
        assert_eq!(responses.len(), 4); // 57 + 3×59 ≥ 200
        let mut echoed = Vec::new();
        assert_eq!(responses[0][4], CMD_PING);
        assert_eq!(
            u16::from_be_bytes([responses[0][5], responses[0][6]]) as usize,
            payload.len()
        );
        echoed.extend_from_slice(&responses[0][7..]);
        for (i, r) in responses[1..].iter().enumerate() {
            assert_eq!(r[4], i as u8);
            echoed.extend_from_slice(&r[5..]);
        }
        echoed.truncate(payload.len());
        assert_eq!(echoed, payload);
    }

    /// getInfo answers CTAP2 OK with FIDO_2_0 + our AAGUID in the CBOR.
    #[test]
    fn get_info_speaks_fido2() {
        let mut auth = FidoAuthenticator::new();
        let cid = open_channel(&mut auth);
        let out = auth.on_report(&init_frame(cid, CMD_CBOR, &[CTAP2_GET_INFO]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_CBOR);
        assert_eq!(out[0][7], CTAP2_OK);
        let body = &out[0][8..8 + u16::from_be_bytes([out[0][5], out[0][6]]) as usize - 1];
        let needle = b"FIDO_2_0";
        assert!(body.windows(needle.len()).any(|w| w == needle));
        assert!(body.windows(AAGUID.len()).any(|w| w == AAGUID));
    }

    /// Unknown CTAPHID commands answer CTAPHID_ERROR, never silence.
    #[test]
    fn unknown_cmd_errors() {
        let mut auth = FidoAuthenticator::new();
        let cid = open_channel(&mut auth);
        let out = auth.on_report(&init_frame(cid, 0x88 /* WINK */, &[]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_ERROR);
        assert_eq!(out[0][7], ERR_INVALID_CMD);
    }

    /// A stray continuation with no reassembly in flight errors instead of panicking.
    #[test]
    fn stray_continuation_errors() {
        let mut auth = FidoAuthenticator::new();
        let cid = open_channel(&mut auth);
        let mut c = [0u8; REPORT_SIZE];
        c[..4].copy_from_slice(&cid.to_be_bytes());
        c[4] = 0; // seq 0, no init in flight
        let out = auth.on_report(&c);
        assert_eq!(out[0][4], CMD_ERROR);
        assert_eq!(out[0][7], ERR_INVALID_CHANNEL);
    }
}
