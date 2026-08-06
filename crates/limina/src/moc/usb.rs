// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Supervisor side of the impersonated MOC fingerprint reader (M14 wave 3).
//!
//! The worker cold-plugs the elanmoc USB identity (a libkrun `BulkPipe`) and binds a UNIX listener
//! (`--moc-socket`); here we connect to it and run the [`Engine`] — the elanmoc protocol + Touch ID
//! verify + per-VM template store — over the socket. This is the FIDO Stage-C proxy split reused:
//! mechanism (USB bus) in the worker, policy (protocol + Secure Enclave) in the Apple-Development-
//! signed supervisor, where `LAContext` works.
//!
//! **Wire protocol** (see `crates/limina-vmm/src/moc_usb.rs`): `[ep][kind][len LE][payload]`.
//! Guest→host commands arrive as `{ep: 0x01, kind: DATA}`; each maps (via [`Engine::handle`]) to at
//! most one reply — `{ep: 0x83|0x84, kind: DATA}` bytes, `{ep, kind: STALL}` to fail a held read, or
//! nothing (a zero-length command, which posts no read). The worker also pushes
//! `{ep, kind: CANCEL}` when the *guest* abandons an endpoint's read, which is how we learn to
//! take a Touch ID sheet down. The socket path is stable across a worker reboot relaunch, so we
//! simply reconnect.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::store::MocStore;
use super::{AlwaysApprove, Engine, PromptCanceller, Reply, SepVerifier, Verifier, EP_MOC_IN};

const KIND_DATA: u8 = 0;
const KIND_STALL: u8 = 1;
/// Worker→supervisor only: the guest cancelled the endpoint named in the frame's `ep` (xHCI Stop
/// Endpoint — see `crates/limina-vmm/src/moc_usb.rs`). Carries no payload.
const KIND_CANCEL: u8 = 2;

/// Whether the reply to a command the guest issued is still wanted.
///
/// The question looks boolean — "did the guest cancel?" — and was implemented as one: a flag set on
/// cancel and cleared as each new command was forwarded. That is wrong, because a reply belongs to
/// a *particular* command, and the guest's next command would clear the flag out from under the
/// one still being worked on. The sequence that broke it, all of it ordinary use:
///
/// 1. the guest issues a finger-wait read; the executor blocks on the Touch ID sheet;
/// 2. the user puts a wrong finger on the sensor, or waves the sheet away; the guest cancels;
/// 3. the guest immediately retries — and forwarding that retry cleared the flag;
/// 4. the abandoned sheet finally resolves `Cancelled`, sees `false`, and writes its STALL.
///
/// That STALL is then delivered to the retry — or, if the driver has given up and reopened the
/// device, to the *next session's* first read. Which is what the guest reported: a stall ~133 ms
/// after every `libusb_reset_device`, long before a finger could have touched the sensor, looking
/// for all the world like a device that halts its own endpoint on open.
///
/// An epoch answers the real question. Each command carries the epoch it was accepted under; a
/// cancel bumps it; a reply is written only if the epoch has not moved since. A later command
/// cannot vouch for an earlier one, and a cancel that arrives while a command is merely *queued*
/// still invalidates it.
#[derive(Default)]
struct ReplyGate {
    epoch: AtomicU64,
}

impl ReplyGate {
    /// Accept a command; the returned token is what decides its reply's fate.
    fn accept(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// The guest abandoned the read in flight: every command accepted so far is now moot.
    fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn wanted(&self, token: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) == token
    }

    /// How many cancels have been processed. Only the tests read it, to wait until the reader has
    /// actually *seen* a cancel before letting the prompt resolve — writing the frame and
    /// releasing the sheet in the next statement is a race, and it is the one that made
    /// `a_cancel_is_serviced_mid_prompt_and_the_stale_reply_is_dropped` flaky under load long
    /// before this gate existed.
    #[cfg(test)]
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// Spawn the fingerprint serve thread: connect to the worker's gadget socket and run the elanmoc
/// engine backed by `store`, prompting Touch ID (labelled with `vm_label`) on verify/enroll.
/// Reconnects across worker relaunches; runs for the supervisor's lifetime. Call only when the
/// `fingerprint` capability is available (a usable Touch ID sensor, or the test-approve knob).
pub fn serve(socket_path: PathBuf, store: Arc<MocStore>, vm_label: String) {
    std::thread::Builder::new()
        .name("limina-moc-usb-sup".into())
        .spawn(move || serve_loop(&socket_path, store, vm_label))
        .ok();
}

fn serve_loop(socket_path: &Path, store: Arc<MocStore>, vm_label: String) {
    // Pick the verifier once: the real biometric prompt, or CI's unconditional approve. The
    // canceller shares the verifier's in-flight cell, so it can dismiss a live sheet from the
    // reader thread; with `AlwaysApprove` nothing is ever in flight and it is a no-op.
    let inflight = Arc::new(AtomicU64::new(0));
    let verifier: Box<dyn Verifier> = if test_approve() {
        log::info!("moc: LIMINA_FP_TEST_APPROVE set — verifying without a Touch ID prompt");
        Box::new(AlwaysApprove)
    } else {
        Box::new(SepVerifier::new(inflight.clone()))
    };
    let canceller = PromptCanceller::new(inflight);
    let engine = Engine::new(store, vm_label);
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path) {
            log::info!("moc: connected to the worker gadget at {socket_path:?}");
            serve_conn(
                stream,
                &engine,
                verifier.as_ref(),
                &canceller,
                &ReplyGate::default(),
            );
            log::info!("moc: worker gadget connection ended; retrying");
        }
        // Worker not up yet, or relaunching across a guest reboot — retry.
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Serve one connection: read guest→host frames, run each command through the engine, write back
/// the reply (data / stall / nothing).
///
/// **Two threads, on purpose.** A command may block for as long as the human takes at the Touch ID
/// sheet (the driver's finger-wait reads have an infinite timeout, so that is safe), but the
/// *cancel* notification — the guest abandoning its request — arrives on the same socket and must
/// be serviced while that prompt is up. So the reader stays on this thread and commands execute on
/// a second one, in order (the guest is strictly lockstep, so ordering is all the sequencing the
/// protocol needs).
fn serve_conn(
    stream: UnixStream,
    engine: &Engine,
    verifier: &dyn Verifier,
    canceller: &PromptCanceller,
    gate: &ReplyGate,
) {
    set_nosigpipe(&stream);
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("moc: cloning the gadget socket failed: {e}");
            return;
        }
    };
    let writer = stream;
    let (tx, rx) = mpsc::channel::<(u64, Vec<u8>)>();

    std::thread::scope(|scope| {
        scope.spawn(move || run_commands(rx, engine, verifier, writer, gate));
        // Reads until EOF/error (the worker is gone).
        while let Ok((ep, kind, cmd)) = read_frame(&mut reader) {
            if kind == KIND_CANCEL {
                // Only the finger-wait endpoint matters: it is the one whose read we may be
                // holding open across a Touch ID prompt. A cancel on the immediate-reply
                // endpoint (a close, say) must not discard a reply that is already on its way.
                log::debug!("moc: guest cancelled ep {ep:#x}");
                if ep == EP_MOC_IN {
                    // The guest dropped the read this command was answering. Take the sheet down
                    // and mark the in-flight reply unwanted: delivering it would answer a
                    // transaction the guest has forgotten.
                    gate.cancel();
                    canceller.cancel();
                }
                continue;
            }
            // Stamped at accept time, not at dequeue: a cancel arriving while this sits in the
            // queue has to invalidate it too.
            if tx.send((gate.accept(), cmd)).is_err() {
                break; // executor gone
            }
        }
        drop(tx);
        // The worker went away mid-prompt (guest reboot / relaunch): nobody is waiting for that
        // sheet either, so take it down rather than leaving it on the user's screen.
        canceller.cancel();
    });
}

/// Run commands from `rx` through the engine in order, writing each reply back. Exits when the
/// reader hangs up.
fn run_commands(
    rx: mpsc::Receiver<(u64, Vec<u8>)>,
    engine: &Engine,
    verifier: &dyn Verifier,
    mut writer: UnixStream,
    gate: &ReplyGate,
) {
    for (token, cmd) in rx {
        let Some(reply) = engine.handle(&cmd, verifier) else {
            log::debug!("moc: cmd {:02x?} -> (no reply)", &cmd[..cmd.len().min(4)]);
            continue; // zero-length command (finger-lift): no reply frame
        };
        // Per-command trace (RUST_LOG=limina=debug) — the field oracle for the elanmoc flow; it was
        // what pinned the IDT transport bug (see docs/design/usb-moc-fingerprint.md §2.1).
        match &reply {
            Reply::Data { ep, bytes } => log::debug!(
                "moc: cmd {:02x?} -> DATA ep {:#x} len {}",
                &cmd[..cmd.len().min(4)],
                ep,
                bytes.len()
            ),
            Reply::Stall { ep } => {
                log::debug!(
                    "moc: cmd {:02x?} -> STALL ep {:#x}",
                    &cmd[..cmd.len().min(4)],
                    ep
                )
            }
        }
        if !gate.wanted(token) {
            // The guest cancelled while we were working. Its read is gone, so this reply has no
            // recipient — and a stall left queued would fail the guest's *next*, unrelated read.
            log::debug!(
                "moc: dropping the reply to {:02x?} — the guest cancelled",
                &cmd[..cmd.len().min(4)]
            );
            continue;
        }
        let res = match reply {
            Reply::Data { ep, bytes } => write_frame(&mut writer, ep, KIND_DATA, &bytes),
            Reply::Stall { ep } => write_frame(&mut writer, ep, KIND_STALL, &[]),
        };
        if let Err(e) = res {
            log::warn!("moc: write failed: {e}");
            return;
        }
    }
}

/// Test-only escape hatch (`LIMINA_FP_TEST_APPROVE=1`, default off): approve verification without a
/// Touch ID prompt, so an L2 (or a host without a usable sensor) can exercise enroll/verify. Mirrors
/// `LIMINA_FIDO_TEST_APPROVE`.
pub fn test_approve() -> bool {
    std::env::var_os("LIMINA_FP_TEST_APPROVE").is_some_and(|v| v == "1")
}

fn write_frame(w: &mut impl Write, ep: u8, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u16;
    let mut hdr = [ep, kind, 0, 0];
    hdr[2..4].copy_from_slice(&len.to_le_bytes());
    w.write_all(&hdr)?;
    w.write_all(payload)
}

fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, u8, Vec<u8>)> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr)?;
    let len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((hdr[0], hdr[1], payload))
}

/// Writing to a socket whose peer (the worker) has exited must fail with EPIPE, not raise SIGPIPE
/// and kill the supervisor (macOS has no MSG_NOSIGNAL).
fn set_nosigpipe(stream: &UnixStream) {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moc::{VerifyOutcome, EP_CMD_IN};
    use std::sync::Mutex;

    /// Stands in for a Touch ID sheet: parks the serve thread until the test releases it, and
    /// reports when the "prompt" went up.
    struct ParkedPrompt {
        up: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<VerifyOutcome>>,
    }
    impl Verifier for ParkedPrompt {
        fn verify(&self, _reason: &str) -> VerifyOutcome {
            self.up.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap()
        }
    }

    /// The two halves of the fix for "the guest cancelled but the sheet stayed up": the reader
    /// must service a cancel frame *while* a prompt blocks the engine (before this, the same
    /// thread did both and the cancel sat unread until the prompt resolved), and the reply that
    /// prompt eventually produces must be dropped — the guest's read is gone, and a queued stall
    /// would fail its next, unrelated request.
    #[test]
    fn a_cancel_is_serviced_mid_prompt_and_the_stale_reply_is_dropped() {
        let (mut worker, supervisor) = UnixStream::pair().unwrap();
        let store = Arc::new(MocStore::in_memory());
        store.set(b"FP1-alice".to_vec());
        let engine = Engine::new(store, "test-vm".into());
        let (up_tx, up_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let verifier = ParkedPrompt {
            up: up_tx,
            release: Mutex::new(release_rx),
        };
        // Nothing in flight in the shared cell, so cancel() is a no-op here: this test is about
        // the socket/threading behaviour, not the LAContext call.
        let canceller = PromptCanceller::new(Arc::new(AtomicU64::new(0)));
        let gate = ReplyGate::default();

        std::thread::scope(|scope| {
            scope.spawn(|| serve_conn(supervisor, &engine, &verifier, &canceller, &gate));

            // Guest asks to verify; the prompt goes up and the engine thread parks in it.
            write_frame(&mut worker, 0x01, KIND_DATA, &[0x40, 0xff, 0x73]).unwrap();
            up_rx.recv().unwrap();

            // Guest walks away (its read was cancelled). This must be read *now*, not after the
            // prompt resolves — otherwise nothing could ever take the sheet down.
            write_frame(&mut worker, EP_MOC_IN, KIND_CANCEL, &[]).unwrap();
            // Only release once the reader has *processed* it: writing the frame does not mean it
            // has been seen, and releasing into that window is what made this test flaky.
            while gate.epoch() == 0 {
                std::thread::yield_now();
            }
            // The prompt then ends as cancelled, as a dismissed sheet does.
            release_tx.send(VerifyOutcome::Cancelled).unwrap();

            worker
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let stale = read_frame(&mut worker);
            assert!(
                stale.is_err(),
                "a reply to the cancelled command was delivered: {:02x?}",
                stale.ok()
            );

            drop(worker); // EOF ends the serve threads
        });
    }

    /// The bug behind a guest's "endpoint stalled ~133 ms after every device reset"
    /// symptom: a wrong finger or a dismissed sheet
    /// makes the guest cancel and immediately retry, and the retry used to vouch for the
    /// abandoned command — so its STALL was written anyway and landed on the retry, or on the
    /// next session's first read after libfprint reopened the device.
    #[test]
    fn a_later_command_cannot_vouch_for_an_abandoned_one() {
        let gate = ReplyGate::default();
        let abandoned = gate.accept();
        gate.cancel();
        let retry = gate.accept();
        assert!(
            !gate.wanted(abandoned),
            "the abandoned command's reply survived the guest's retry — this is the stall that \
             reaches the next session"
        );
        assert!(
            gate.wanted(retry),
            "the retry's own reply must still be delivered"
        );
    }

    /// A cancel can arrive while the command it kills is still sitting in the queue, never having
    /// reached the engine. Stamping at accept time rather than dequeue time is what covers it.
    #[test]
    fn a_cancel_while_a_command_is_still_queued_invalidates_it() {
        let gate = ReplyGate::default();
        let queued = gate.accept();
        gate.cancel();
        assert!(!gate.wanted(queued));
    }

    /// The same thing end to end, through the real reader/executor threads: after a cancel the
    /// guest retries, and the only frame it may ever see is the answer to the retry. A STALL here
    /// is the wedge — libfprint reports `LIBUSB_ERROR_PIPE` and fails the verify.
    ///
    /// Deterministic only because it waits for the reader to have *seen* the cancel before going
    /// on. Without that wait it passed about half the time with the bug reintroduced — which is
    /// exactly why the guest saw an intermittent stall rather than a reliable one.
    #[test]
    fn a_retry_after_a_cancel_is_never_answered_with_the_abandoned_stall() {
        let (mut worker, supervisor) = UnixStream::pair().unwrap();
        let store = Arc::new(MocStore::in_memory());
        // Must be enrolled, or `verify` short-circuits to "nothing enrolled" and never prompts —
        // and the test then waits forever for a sheet that was never going up.
        store.set(b"FP1-alice".to_vec());
        let engine = Engine::new(store, "test-vm".into());
        let (up_tx, up_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let verifier = ParkedPrompt {
            up: up_tx,
            release: Mutex::new(release_rx),
        };
        let canceller = PromptCanceller::new(Arc::new(AtomicU64::new(0)));
        let gate = ReplyGate::default();

        std::thread::scope(|scope| {
            scope.spawn(|| serve_conn(supervisor, &engine, &verifier, &canceller, &gate));

            // Verify; the sheet goes up and the executor parks in it.
            write_frame(&mut worker, 0x01, KIND_DATA, &[0x40, 0xff, 0x73]).unwrap();
            up_rx.recv().unwrap();
            // Wrong finger / sheet dismissed: the guest gives up on this read and retries at once.
            write_frame(&mut worker, EP_MOC_IN, KIND_CANCEL, &[]).unwrap();
            while gate.epoch() == 0 {
                std::thread::yield_now();
            }
            write_frame(&mut worker, 0x01, KIND_DATA, &[0x40, 0x19]).unwrap(); // FW version
                                                                               // Only now does the abandoned sheet resolve, as a dismissed one does.
            release_tx.send(VerifyOutcome::Cancelled).unwrap();

            worker
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (ep, kind, payload) = read_frame(&mut worker).unwrap();
            assert_eq!(
                (ep, kind),
                (EP_CMD_IN, KIND_DATA),
                "the retry was answered with the abandoned command's reply"
            );
            assert_eq!(payload, vec![0xff, 0xff]);

            // ...and nothing else follows it.
            worker
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let extra = read_frame(&mut worker);
            assert!(
                extra.is_err(),
                "a stale frame trailed the retry's reply: {:02x?}",
                extra.ok()
            );

            drop(worker);
        });
    }

    /// The ordinary path is untouched: a command still gets its reply back on the right endpoint.
    #[test]
    fn a_command_still_gets_its_reply() {
        let (mut worker, supervisor) = UnixStream::pair().unwrap();
        let store = Arc::new(MocStore::in_memory());
        let engine = Engine::new(store, "test-vm".into());
        let verifier = AlwaysApprove;
        let canceller = PromptCanceller::new(Arc::new(AtomicU64::new(0)));
        let gate = ReplyGate::default();

        std::thread::scope(|scope| {
            scope.spawn(|| serve_conn(supervisor, &engine, &verifier, &canceller, &gate));
            write_frame(&mut worker, 0x01, KIND_DATA, &[0x40, 0x19]).unwrap(); // FW version
            worker
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (ep, kind, payload) = read_frame(&mut worker).unwrap();
            assert_eq!((ep, kind), (EP_CMD_IN, KIND_DATA));
            assert_eq!(payload, vec![0xff, 0xff]);
            drop(worker);
        });
    }
}
