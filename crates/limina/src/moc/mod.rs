// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The impersonated Elan match-on-chip (MOC) fingerprint reader (M14 wave 3).
//!
//! Stock Fedora's `libfprint` `elanmoc` driver binds our emulated USB device (VID:PID
//! `04f3:0c7d`) and drives it as a real reader; we answer its byte-exact protocol and back every
//! "match" with a host Touch ID prompt. No real fingerprint data ever exists — the device is a
//! key-value store of one libfprint-authored `user_id` string (see [`store`]), and a match is
//! `LAContext` biometrics. Design + full wire reference: `docs/design/usb-moc-fingerprint.md`;
//! the driver source (`libfprint/drivers/elanmoc/elanmoc.{c,h}`) is the spec.
//!
//! This module is the transport-agnostic protocol **engine** ([`Engine`]): it maps one command
//! (the bytes the guest wrote to bulk-OUT `0x01`) to at most one reply frame, tagged with the IN
//! endpoint it belongs on (`0x83` immediate, `0x84` finger-wait) or a stall. Touch ID is behind
//! the [`Verifier`] trait so the engine is a pure function over bytes for the pcapng-replay oracle.
//! The worker's `moc_usb.rs` presents the USB identity (a libkrun `BulkPipe`); [`usb::serve`]
//! shuttles frames over a UNIX socket into this engine — the FIDO Stage-C proxy split reused.

use std::path::PathBuf;
use std::sync::Arc;

pub mod store;
pub mod usb;

use store::MocStore;

/// Build the per-VM template store iff this host can back the reader — a usable Touch ID sensor
/// (`sep::can_verify`), or the `LIMINA_FP_TEST_APPROVE` knob. `None` on a Mac that can never satisfy
/// a biometric prompt (no sensor / no enrolled finger), so the reader is simply not advertised
/// (graceful degrade). Persists to `path` (the managed VM's bundle dir) so the enrolled finger
/// survives restarts, matching fprintd's own on-disk print.
pub fn store_if_capable(path: Option<PathBuf>) -> Option<Arc<MocStore>> {
    if crate::sep::can_verify() || usb::test_approve() {
        let store = match path {
            Some(p) => MocStore::with_path(p),
            None => MocStore::in_memory(),
        };
        Some(Arc::new(store))
    } else {
        log::info!("moc: no usable Touch ID sensor; fingerprint reader disabled");
        None
    }
}

/// IN endpoint addresses the engine replies on (`elanmoc.h`): immediate replies on `0x83`,
/// cancelable finger-wait replies (enroll capture / verify) on `0x84`. Commands arrive on OUT `0x01`.
pub const EP_CMD_IN: u8 = 0x83;
pub const EP_MOC_IN: u8 = 0x84;

/// Elan protocol constants (`elanmoc.h`).
const ELAN_MSG_OK: u8 = 0x00;
const ELAN_MSG_VERIFY_ERR: u8 = 0xfd; // no-match status in a verify reply
const ELAN_MSG_EMPTY_SLOT: u8 = 0xfe; // get-userid reply for an unoccupied slot
const ELAN_MAX_USER_ID_LEN: usize = 92;
/// The real elan unit returns a fixed 71-byte get-userid reply (`[family, status]` + a 69-byte
/// body); libfprint's 97-byte `resp_len` is just its read buffer, and it uses the actual short-read
/// length. We match 71 for short user_ids (the common case) and grow toward 97 only if a user_id
/// exceeds the 66-byte body (5 header/meta bytes + 66 = 71).
const GET_USERID_RESP_LEN: usize = 71;
/// Enrolled-count value that arms the driver's DATA_FULL rejection (`curr_enrolled ==
/// ELAN_MAX_ENROLL_NUM + 1`, elanmoc.c:346). We report this when the single slot is occupied so a
/// second *distinct* enrollment is refused cleanly (GNOME shows "no space") instead of silently
/// overwriting the one finger. Empty → 0.
const FULL_COUNT: u8 = 10;

/// One outgoing action the engine asks the transport to perform.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Deliver `bytes` on IN endpoint `ep`.
    Data { ep: u8, bytes: Vec<u8> },
    /// Fail (stall) the held read on IN endpoint `ep` — an enroll decline / protocol error.
    Stall { ep: u8 },
}

/// The Touch ID primitive, abstracted so the engine stays a pure function for tests/the oracle.
pub trait Verifier: Send + Sync {
    /// Prompt for a fingerprint and return whether a trusted finger matched.
    fn verify(&self, reason: &str) -> bool;
}

/// Production verifier: a biometrics-only `LAContext` prompt via the Secure-Enclave shim.
pub struct SepVerifier;
impl Verifier for SepVerifier {
    fn verify(&self, reason: &str) -> bool {
        crate::sep::sep_verify(reason)
    }
}

/// CI verifier: approve without a sheet (the `LIMINA_FP_TEST_APPROVE` knob, mirroring
/// `LIMINA_FIDO_TEST_APPROVE`). Lets an L2 exercise enroll/verify without a live finger.
pub struct AlwaysApprove;
impl Verifier for AlwaysApprove {
    fn verify(&self, _reason: &str) -> bool {
        true
    }
}

/// The elanmoc protocol engine over a single-finger [`MocStore`].
pub struct Engine {
    store: Arc<MocStore>,
    /// Shown in the Touch ID sheet (the VM's name), so a multi-VM user knows which asked.
    vm_label: String,
}

impl Engine {
    pub fn new(store: Arc<MocStore>, vm_label: String) -> Engine {
        Engine { store, vm_label }
    }

    fn reason(&self, action: &str) -> String {
        format!("{action} for {}", self.vm_label)
    }

    /// Handle one command (the bytes the guest wrote to bulk-OUT). Returns the reply to emit, or
    /// `None` for a `resp_len == 0` command (finger-lift), which posts no IN read — emitting a
    /// stray frame would desync the next reply (see the design doc §4).
    pub fn handle(&self, cmd: &[u8], verifier: &dyn Verifier) -> Option<Reply> {
        let b = |i: usize| cmd.get(i).copied().unwrap_or(0);
        match (b(0), b(1), b(2)) {
            // Init / status — all immediate on 0x83, no Touch ID.
            (0x40, 0x19, _) => data(EP_CMD_IN, vec![0xff, 0xff]), // FW version → 0xFFFF (fwupd bluff)
            (0x00, 0x0c, _) => data(EP_CMD_IN, vec![0x57, 0x00, 0x6b, 0x00]), // sensor dims (real 87x107; x=[0], y=[2])
            (0x40, 0xff, 0x00) => data(EP_CMD_IN, vec![0x40, 0x03]), // cal status → 0x03 = ready
            (0x40, 0xff, 0x04) => {
                // Enrolled count: 0 when empty, FULL_COUNT when occupied (arms DATA_FULL so a
                // second distinct enroll is refused — the single-finger policy).
                let count = if self.store.is_enrolled() {
                    FULL_COUNT
                } else {
                    0
                };
                data(EP_CMD_IN, vec![0x40, count])
            }
            (0x40, 0xff, 0x14) => data(EP_CMD_IN, vec![0x40, ELAN_MSG_OK]), // set mode → ACK
            (0x40, 0xff, 0x02) => None, // finger-lift → NO reply
            (0x40, 0xff, 0x22) => data(EP_CMD_IN, vec![0x40, ELAN_MSG_OK]), // reenroll check → "new finger"

            // Enroll capture (finger-wait, 0x84): prompt Touch ID once on the first stage.
            (0x40, 0xff, 0x01) => self.enroll_capture(cmd, verifier),
            // Enroll commit: store the host-authored user_id, ACK on 0x83.
            (0x40, 0xff, 0x11) => self.commit(cmd),
            // Verify/identify (finger-wait, 0x84): Touch ID → matched slot or no-match.
            (0x40, 0xff, 0x73) => self.verify(verifier),
            // Delete by user_id.
            (0x40, 0xff, 0x13) => self.delete(cmd),
            // Get a slot's user_id (list walk + verify follow-up). Slot index is cmd[2].
            (0x43, 0x21, _) => self.get_userid(cmd),

            _ => {
                log::warn!("moc: unhandled command {:02x?}", &cmd[..cmd.len().min(4)]);
                Some(Reply::Stall { ep: EP_CMD_IN })
            }
        }
    }

    /// `40 ff 01`: one enroll capture stage. The frame index is `cmd[5]`; we prompt Touch ID once
    /// on stage 0 and auto-complete the rest (GNOME's enroll bar races to done). A declined finger
    /// stalls the held finger-wait read → the driver fails enrollment cleanly (no retry loop).
    fn enroll_capture(&self, cmd: &[u8], verifier: &dyn Verifier) -> Option<Reply> {
        let frame_idx = cmd.get(5).copied().unwrap_or(0);
        if frame_idx == 0 && !verifier.verify(&self.reason("Enroll your fingerprint")) {
            return Some(Reply::Stall { ep: EP_MOC_IN });
        }
        data(EP_MOC_IN, vec![0x40, ELAN_MSG_OK])
    }

    /// `40 ff 11`: enroll commit. The 95-byte user_id record sits at offset 5:
    /// `[uuid0][uuid1][len][user_id…]`, so the length is `cmd[7]` and the string starts at `cmd[8]`.
    fn commit(&self, cmd: &[u8]) -> Option<Reply> {
        let len = cmd.get(7).copied().unwrap_or(0) as usize;
        let len = len.min(ELAN_MAX_USER_ID_LEN);
        if let Some(uid) = cmd.get(8..8 + len) {
            if !uid.is_empty() {
                self.store.set(uid.to_vec());
            }
        }
        data(EP_CMD_IN, vec![0x40, ELAN_MSG_OK])
    }

    /// `40 ff 73`: verify/identify. Nothing enrolled → immediate no-match (no prompt). Otherwise
    /// Touch ID → matched slot 0 (the single finger) or a clean no-match `0xfd`. The driver then
    /// reads slot 0's user_id via `get_userid` and compares it to its gallery.
    fn verify(&self, verifier: &dyn Verifier) -> Option<Reply> {
        if !self.store.is_enrolled() {
            return data(EP_MOC_IN, vec![0x40, ELAN_MSG_VERIFY_ERR]);
        }
        if verifier.verify(&self.reason("Verify your fingerprint")) {
            data(EP_MOC_IN, vec![0x40, 0x00]) // matched slot index 0
        } else {
            data(EP_MOC_IN, vec![0x40, ELAN_MSG_VERIFY_ERR]) // no match (clean completion, not a stall)
        }
    }

    /// `40 ff 13`: delete. The user_id record is at offset 3 (`[uuid0][uuid1][len][user_id…]`), so
    /// the string is at `cmd[6]`, length `cmd[5]`. Clears the slot if it matches.
    fn delete(&self, cmd: &[u8]) -> Option<Reply> {
        let len = (cmd.get(5).copied().unwrap_or(0) as usize).min(ELAN_MAX_USER_ID_LEN);
        if let Some(uid) = cmd.get(6..6 + len) {
            self.store.delete_matching(uid);
        }
        data(EP_CMD_IN, vec![0x40, ELAN_MSG_OK])
    }

    /// `43 21 <slot>`: read a slot's user_id record. Slot 0 (when occupied) returns the enrolled
    /// user_id; every other slot — and slot 0 when empty — returns the `0xfe` empty marker the list
    /// walk skips.
    fn get_userid(&self, cmd: &[u8]) -> Option<Reply> {
        let slot = cmd.get(2).copied().unwrap_or(0);
        let resp = match (slot, self.store.user_id()) {
            (0, Some(uid)) => userid_response(&uid),
            _ => empty_userid_response(),
        };
        data(EP_CMD_IN, resp)
    }
}

/// Build the get-userid reply for an occupied slot:
/// `[0x43, OK, uuid0=0, uuid1=0, len, user_id…]`, 71 bytes for a short user_id (matching the real
/// unit), growing only if the user_id needs more than the 66-byte body.
fn userid_response(user_id: &[u8]) -> Vec<u8> {
    let len = user_id.len().min(ELAN_MAX_USER_ID_LEN);
    let total = (5 + len).max(GET_USERID_RESP_LEN);
    let mut r = vec![0u8; total];
    r[0] = 0x43;
    r[1] = ELAN_MSG_OK;
    // r[2], r[3] = UUID bytes, always 0 for libfprint prints.
    r[4] = len as u8;
    r[5..5 + len].copy_from_slice(&user_id[..len]);
    r
}

/// The 71-byte get-userid reply for an empty slot: `[0x43, 0xfe, 0xff…]` (the driver skips it on
/// `resp[1] == 0xfe` without reading the body; `0xff` filler matches the real unit).
fn empty_userid_response() -> Vec<u8> {
    let mut r = vec![0xffu8; GET_USERID_RESP_LEN];
    r[0] = 0x43;
    r[1] = ELAN_MSG_EMPTY_SLOT;
    r
}

fn data(ep: u8, bytes: Vec<u8>) -> Option<Reply> {
    Some(Reply::Data { ep, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(Arc::new(MocStore::in_memory()), "test-vm".into())
    }

    struct Decline;
    impl Verifier for Decline {
        fn verify(&self, _: &str) -> bool {
            false
        }
    }

    fn expect_data(r: Option<Reply>, ep: u8) -> Vec<u8> {
        match r {
            Some(Reply::Data { ep: e, bytes }) => {
                assert_eq!(e, ep, "reply on the wrong endpoint");
                bytes
            }
            other => panic!("expected Data on {ep:#x}, got {other:?}"),
        }
    }

    #[test]
    fn init_sequence_answers_on_0x83() {
        let e = engine();
        let a = &AlwaysApprove;
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x00], a), EP_CMD_IN),
            vec![0x40, 0x03]
        );
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x14, 0x03], a), EP_CMD_IN),
            vec![0x40, 0x00]
        );
        assert_eq!(
            expect_data(e.handle(&[0x40, 0x19], a), EP_CMD_IN),
            vec![0xff, 0xff]
        );
        let dims = expect_data(e.handle(&[0x00, 0x0c], a), EP_CMD_IN);
        assert_eq!(dims.len(), 4);
        assert_ne!(dims[0], 0, "x_trace nonzero");
        assert_ne!(dims[2], 0, "y_trace nonzero");
        // Empty store → count 0.
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x04], a), EP_CMD_IN),
            vec![0x40, 0x00]
        );
    }

    #[test]
    fn finger_lift_emits_no_reply() {
        let e = engine();
        assert_eq!(e.handle(&[0x40, 0xff, 0x02], &AlwaysApprove), None);
    }

    #[test]
    fn fw_version_is_maxed_for_the_fwupd_bluff() {
        let e = engine();
        assert_eq!(
            expect_data(e.handle(&[0x40, 0x19], &AlwaysApprove), EP_CMD_IN),
            vec![0xff, 0xff]
        );
    }

    #[test]
    fn enroll_prompts_once_then_stores_and_reports_full() {
        let e = engine();
        let a = &AlwaysApprove;
        // reenroll check → "new finger"
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x22], a), EP_CMD_IN),
            vec![0x40, 0x00]
        );
        // 9 capture stages (frame index cmd[5] = 0..8), all OK on 0x84
        for frame in 0u8..9 {
            let cmd = [0x40, 0xff, 0x01, 0, 9, frame, 0];
            assert_eq!(expect_data(e.handle(&cmd, a), EP_MOC_IN), vec![0x40, 0x00]);
        }
        // commit with user_id "alice" at offset 8 (record [uuid0,uuid1,len,uid] at offset 5)
        let uid = b"alice";
        let mut commit = vec![0u8; 128];
        commit[0..3].copy_from_slice(&[0x40, 0xff, 0x11]);
        commit[7] = uid.len() as u8; // len at record offset 5+2 = cmd[7]
        commit[8..8 + uid.len()].copy_from_slice(uid);
        assert_eq!(
            expect_data(e.handle(&commit, a), EP_CMD_IN),
            vec![0x40, 0x00]
        );
        assert!(e.store.is_enrolled());
        // Now occupied → enrolled count reports FULL_COUNT (arms DATA_FULL for a 2nd enroll).
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x04], a), EP_CMD_IN),
            vec![0x40, FULL_COUNT]
        );
    }

    #[test]
    fn enroll_decline_stalls_the_finger_wait_read() {
        let e = engine();
        // First stage with a declining verifier → stall on 0x84, nothing stored.
        let cmd = [0x40, 0xff, 0x01, 0, 9, 0, 0];
        assert_eq!(
            e.handle(&cmd, &Decline),
            Some(Reply::Stall { ep: EP_MOC_IN })
        );
        assert!(!e.store.is_enrolled());
    }

    #[test]
    fn verify_matches_after_enroll_and_get_userid_returns_it() {
        let e = engine();
        e.store.set(b"FP1-alice".to_vec());
        // verify → matched slot 0 on 0x84
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x73], &AlwaysApprove), EP_MOC_IN),
            vec![0x40, 0x00]
        );
        // get_userid(slot 0) → the stored id on 0x83
        let r = expect_data(e.handle(&[0x43, 0x21, 0x00], &AlwaysApprove), EP_CMD_IN);
        assert_eq!(r.len(), GET_USERID_RESP_LEN);
        assert_eq!(r[0], 0x43);
        assert_eq!(r[1], ELAN_MSG_OK);
        assert_eq!(r[4] as usize, b"FP1-alice".len());
        assert_eq!(&r[5..5 + b"FP1-alice".len()], b"FP1-alice");
    }

    #[test]
    fn verify_declined_is_clean_no_match_not_a_stall() {
        let e = engine();
        e.store.set(b"FP1-alice".to_vec());
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x73], &Decline), EP_MOC_IN),
            vec![0x40, ELAN_MSG_VERIFY_ERR]
        );
    }

    #[test]
    fn verify_with_nothing_enrolled_is_no_match_without_a_prompt() {
        let e = engine();
        // A verifier that would panic if called — proves no prompt happens on an empty store.
        struct Never;
        impl Verifier for Never {
            fn verify(&self, _: &str) -> bool {
                panic!("must not prompt when nothing is enrolled");
            }
        }
        assert_eq!(
            expect_data(e.handle(&[0x40, 0xff, 0x73], &Never), EP_MOC_IN),
            vec![0x40, ELAN_MSG_VERIFY_ERR]
        );
    }

    #[test]
    fn list_returns_slot0_occupied_and_others_empty() {
        let e = engine();
        e.store.set(b"bob".to_vec());
        // slot 0 occupied
        let r0 = expect_data(e.handle(&[0x43, 0x21, 0x00], &AlwaysApprove), EP_CMD_IN);
        assert_eq!(r0[1], ELAN_MSG_OK);
        assert_eq!(&r0[5..5 + 3], b"bob");
        // slots 1..9 empty (0xfe), skipped by the list walk
        for slot in 1u8..=9 {
            let r = expect_data(e.handle(&[0x43, 0x21, slot], &AlwaysApprove), EP_CMD_IN);
            assert_eq!(r[0], 0x43);
            assert_eq!(r[1], ELAN_MSG_EMPTY_SLOT, "slot {slot} must read empty");
        }
    }

    /// Golden oracle: replay the real elanmoc USB traffic captured by libfprint
    /// (`tests/elanmoc/custom.pcapng`, extracted to `testdata/elanmoc_capture.json`) — the full
    /// open → enroll → list → verify → identify → delete flow, 37 command/response pairs — through
    /// our engine. State-dependent VALUES differ by design (we return `0xffff` for the firmware
    /// version, `FULL_COUNT` for an occupied device, our own stored user_id), so we assert the
    /// *shape* the driver actually depends on: every real command is handled (never the unknown →
    /// stall fallback), the reply lands on the correct endpoint (`0x83` immediate vs `0x84`
    /// finger-wait), and carries the right family byte and response length. This pins wire
    /// faithfulness against real hardware, independent of any guest.
    #[test]
    fn golden_pcapng_replay() {
        #[derive(serde::Deserialize)]
        struct Pair {
            cmd: String,
            in_ep: u8,
            resp: String,
        }
        fn unhex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        }
        let corpus: Vec<Pair> =
            serde_json::from_str(include_str!("testdata/elanmoc_capture.json")).unwrap();
        assert_eq!(corpus.len(), 37, "the full captured flow");
        let e = engine();
        for (i, p) in corpus.iter().enumerate() {
            let cmd = unhex(&p.cmd);
            let cap = unhex(&p.resp);
            // fw-version (40 19) and sensor-dims (00 0c) replies are raw values, not
            // `[family, status]`-framed, so they carry no family byte (and our version is the
            // intentional 0xffff bluff, ≠ the captured 0x8004). Every other reply is framed.
            let raw_value = cmd[..2] == [0x40, 0x19] || cmd[..2] == [0x00, 0x0c];
            match e.handle(&cmd, &AlwaysApprove) {
                Some(Reply::Data { ep, bytes }) => {
                    assert_eq!(ep, p.in_ep, "pair {i} cmd {}: endpoint", p.cmd);
                    assert_eq!(
                        bytes.len(),
                        cap.len(),
                        "pair {i} cmd {}: response length",
                        p.cmd
                    );
                    if !raw_value {
                        assert_eq!(bytes[0], cap[0], "pair {i} cmd {}: family byte", p.cmd);
                    }
                }
                other => panic!(
                    "pair {i} cmd {}: expected a data reply, got {other:?}",
                    p.cmd
                ),
            }
        }
    }

    #[test]
    fn delete_matching_userid_clears_the_slot() {
        let e = engine();
        let uid = b"carol";
        e.store.set(uid.to_vec());
        let mut del = vec![0u8; 128];
        del[0..3].copy_from_slice(&[0x40, 0xff, 0x13]);
        del[5] = uid.len() as u8; // len at record offset 3+2 = cmd[5]
        del[6..6 + uid.len()].copy_from_slice(uid);
        assert_eq!(
            expect_data(e.handle(&del, &AlwaysApprove), EP_CMD_IN),
            vec![0x40, 0x00]
        );
        assert!(!e.store.is_enrolled());
    }
}
