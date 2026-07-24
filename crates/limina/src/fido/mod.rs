// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host side of the virtual FIDO authenticator (M14).
//!
//! Speaks CTAPHID over 64-byte reports carried by `Message::FidoReport` frames on
//! `CHANNEL_FIDO`. This module owns the transport layer completely (channel
//! allocation, packet reassembly, chunked responses); the CTAP2 command semantics
//! (`getInfo` / `makeCredential` / `getAssertion`, backed by Secure-Enclave ES256
//! keys behind a host Touch ID prompt) live in [`ctap2`], and the credential
//! records in [`store`]. `CMD_CBOR` frames are handed to `ctap2::handle`; the
//! framing below is shared by both the uhid transport and the future emulated-USB
//! one (see roadmap M14).
//!
//! One instance per control-plane peer: CTAPHID channel ids are per-device state and
//! the agent creates one uhid device per connection. All peers of one VM share the
//! same credential [`store`].

use std::sync::Arc;

pub mod ctap2;
pub mod store;

use store::FidoStore;

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
    /// Shared per-VM credential store (CTAP2 commands read/write it).
    store: Arc<FidoStore>,
}

impl FidoAuthenticator {
    pub fn new(store: Arc<FidoStore>) -> FidoAuthenticator {
        FidoAuthenticator {
            next_cid: 1,
            rx: None,
            store,
        }
    }

    /// Process one 64-byte report from the guest. Short reports are padded (the uhid
    /// path always delivers full frames; be tolerant anyway).
    pub fn on_report(&mut self, report: &[u8]) -> Outcome {
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
                return Outcome::now(vec![error_report(cid, ERR_INVALID_CMD)]);
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
            Outcome::now(Vec::new())
        } else {
            // Continuation packet.
            let Some(rx) = self.rx.as_mut() else {
                return Outcome::now(vec![error_report(cid, ERR_INVALID_CHANNEL)]);
            };
            if rx.cid != cid {
                return Outcome::now(vec![error_report(cid, ERR_INVALID_CHANNEL)]);
            }
            if b4 != rx.next_seq {
                self.rx = None;
                return Outcome::now(vec![error_report(cid, ERR_INVALID_SEQ)]);
            }
            rx.next_seq = rx.next_seq.wrapping_add(1);
            let take = (rx.total - rx.buf.len()).min(CONT_DATA);
            rx.buf.extend_from_slice(&frame[5..5 + take]);
            if rx.buf.len() == rx.total {
                let rx = self.rx.take().unwrap();
                return self.dispatch(rx.cid, rx.cmd, &rx.buf);
            }
            Outcome::now(Vec::new())
        }
    }

    /// The shared per-VM credential store (the caller runs the CTAP2 command with it).
    pub fn store(&self) -> Arc<FidoStore> {
        self.store.clone()
    }

    /// Handle one complete CTAPHID message.
    fn dispatch(&mut self, cid: u32, cmd: u8, payload: &[u8]) -> Outcome {
        match cmd {
            CMD_INIT => {
                if payload.len() < 8 {
                    return Outcome::now(vec![error_report(cid, ERR_INVALID_CMD)]);
                }
                // Broadcast → allocate a channel; existing cid → re-sync (echo it back).
                let assigned = if cid == BROADCAST_CID {
                    let new_cid = self.next_cid;
                    self.next_cid = self.next_cid.wrapping_add(1).max(1);
                    new_cid
                } else {
                    cid
                };
                let mut resp = Vec::with_capacity(17);
                resp.extend_from_slice(&payload[..8]); // nonce echo
                resp.extend_from_slice(&assigned.to_be_bytes());
                resp.push(2); // CTAPHID protocol version
                resp.extend_from_slice(&[0, 1, 0]); // device version major/minor/build
                resp.push(CAP_CBOR | CAP_NMSG);
                Outcome::now(packetize(cid, CMD_INIT, &resp))
            }
            CMD_PING => Outcome::now(packetize(cid, CMD_PING, payload)),
            // CTAP2 commands can block on a host Touch ID prompt for seconds. The
            // caller must run them off the serve thread and emit CTAPHID_KEEPALIVE
            // meanwhile, or libfido2/browsers time out with FIDO_ERR_RX. Hand the work
            // out rather than blocking here.
            CMD_CBOR => Outcome::Cbor {
                cid,
                payload: payload.to_vec(),
            },
            _ => Outcome::now(vec![error_report(cid, ERR_INVALID_CMD)]),
        }
    }
}

/// What [`FidoAuthenticator::on_report`] wants the caller to do next.
pub enum Outcome {
    /// Send these reports to the guest now.
    Reports(Vec<[u8; REPORT_SIZE]>),
    /// A CTAP2 CBOR command that may block on Touch ID. Run
    /// `ctap2::handle(&auth.store(), &payload)` off the serve thread, sending
    /// [`keepalive_report`]`(cid)` every ~100 ms until it returns, then emit
    /// [`cbor_report_frames`]`(cid, &response)`.
    Cbor { cid: u32, payload: Vec<u8> },
}

impl Outcome {
    fn now(reports: Vec<[u8; REPORT_SIZE]>) -> Outcome {
        Outcome::Reports(reports)
    }
}

/// A CTAPHID_KEEPALIVE report (status = processing) for `cid` — keeps libfido2 and
/// browsers waiting while a command blocks on the host Touch ID prompt.
pub fn keepalive_report(cid: u32) -> [u8; REPORT_SIZE] {
    const CMD_KEEPALIVE: u8 = 0xBB;
    const STATUS_PROCESSING: u8 = 0x01;
    let mut f = [0u8; REPORT_SIZE];
    f[..4].copy_from_slice(&cid.to_be_bytes());
    f[4] = CMD_KEEPALIVE;
    f[6] = 1; // BCNT
    f[7] = STATUS_PROCESSING;
    f
}

/// Frame a completed CTAP2 response (status byte + CBOR) into CTAPHID CBOR reports.
pub fn cbor_report_frames(cid: u32, response: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
    packetize(cid, CMD_CBOR, response)
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

    /// A fresh authenticator with an empty in-memory store. These transport tests
    /// never reach the SEP-backed CTAP2 commands, so no store contents are needed.
    fn auth() -> FidoAuthenticator {
        FidoAuthenticator::new(Arc::new(FidoStore::in_memory()))
    }

    fn init_frame(cid: u32, cmd: u8, payload: &[u8]) -> [u8; REPORT_SIZE] {
        assert!(payload.len() <= INIT_DATA);
        let mut f = [0u8; REPORT_SIZE];
        f[..4].copy_from_slice(&cid.to_be_bytes());
        f[4] = cmd;
        f[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        f[7..7 + payload.len()].copy_from_slice(payload);
        f
    }

    /// Feed a report and require an immediate (non-CBOR) `Reports` outcome.
    fn reports(auth: &mut FidoAuthenticator, frame: &[u8]) -> Vec<[u8; REPORT_SIZE]> {
        match auth.on_report(frame) {
            Outcome::Reports(r) => r,
            Outcome::Cbor { .. } => panic!("unexpected CBOR outcome"),
        }
    }

    /// Broadcast INIT allocates a channel and echoes the nonce.
    #[test]
    fn init_allocates_channel() {
        let mut auth = auth();
        let nonce = [9u8, 8, 7, 6, 5, 4, 3, 2];
        let out = reports(&mut auth, &init_frame(BROADCAST_CID, CMD_INIT, &nonce));
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
        let out = reports(auth, &init_frame(BROADCAST_CID, CMD_INIT, &[0u8; 8]));
        let f = out[0];
        u32::from_be_bytes([f[15], f[16], f[17], f[18]])
    }

    /// PING echoes on the allocated channel.
    #[test]
    fn ping_echoes() {
        let mut auth = auth();
        let cid = open_channel(&mut auth);
        let out = reports(&mut auth, &init_frame(cid, CMD_PING, b"hello"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_PING);
        assert_eq!(u16::from_be_bytes([out[0][5], out[0][6]]), 5);
        assert_eq!(&out[0][7..12], b"hello");
    }

    /// A multi-packet PING reassembles and the echo chunks back out.
    #[test]
    fn multipacket_ping_roundtrips() {
        let mut auth = auth();
        let cid = open_channel(&mut auth);
        let payload: Vec<u8> = (0..200u16).map(|i| i as u8).collect();

        // Send: init packet + continuations, hand-framed.
        let mut sent = INIT_DATA;
        let mut f = [0u8; REPORT_SIZE];
        f[..4].copy_from_slice(&cid.to_be_bytes());
        f[4] = CMD_PING;
        f[5..7].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        f[7..].copy_from_slice(&payload[..INIT_DATA]);
        assert!(reports(&mut auth, &f).is_empty());
        let mut seq = 0u8;
        let mut responses = Vec::new();
        while sent < payload.len() {
            let take = (payload.len() - sent).min(CONT_DATA);
            let mut c = [0u8; REPORT_SIZE];
            c[..4].copy_from_slice(&cid.to_be_bytes());
            c[4] = seq;
            c[5..5 + take].copy_from_slice(&payload[sent..sent + take]);
            responses = reports(&mut auth, &c);
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

    /// getInfo (CTAP2 cmd 0x04) routes through the Cbor outcome → `ctap2::handle` →
    /// `cbor_report_frames`, answering OK with FIDO_2_0 + our AAGUID.
    #[test]
    fn get_info_speaks_fido2() {
        let mut auth = auth();
        let cid = open_channel(&mut auth);
        let out = match auth.on_report(&init_frame(cid, CMD_CBOR, &[0x04])) {
            Outcome::Cbor { cid, payload } => {
                let resp = ctap2::handle(&auth.store(), &payload);
                cbor_report_frames(cid, &resp)
            }
            Outcome::Reports(_) => panic!("getInfo should be a CBOR command"),
        };
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_CBOR);
        assert_eq!(out[0][7], 0x00); // CTAP2_OK
        let body = &out[0][8..8 + u16::from_be_bytes([out[0][5], out[0][6]]) as usize - 1];
        assert!(body.windows(8).any(|w| w == b"FIDO_2_0"));
        assert!(body.windows(16).any(|w| w == b"limina-touchid!!"));
    }

    /// Unknown CTAPHID commands answer CTAPHID_ERROR, never silence.
    #[test]
    fn unknown_cmd_errors() {
        let mut auth = auth();
        let cid = open_channel(&mut auth);
        let out = reports(&mut auth, &init_frame(cid, 0x88 /* WINK */, &[]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][4], CMD_ERROR);
        assert_eq!(out[0][7], ERR_INVALID_CMD);
    }

    /// A stray continuation with no reassembly in flight errors instead of panicking.
    #[test]
    fn stray_continuation_errors() {
        let mut auth = auth();
        let cid = open_channel(&mut auth);
        let mut c = [0u8; REPORT_SIZE];
        c[..4].copy_from_slice(&cid.to_be_bytes());
        c[4] = 0; // seq 0, no init in flight
        let out = reports(&mut auth, &c);
        assert_eq!(out[0][4], CMD_ERROR);
        assert_eq!(out[0][7], ERR_INVALID_CHANNEL);
    }
}
