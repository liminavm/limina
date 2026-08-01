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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::store::MocStore;
use super::{AlwaysApprove, Engine, PromptCanceller, Reply, SepVerifier, Verifier, EP_MOC_IN};

const KIND_DATA: u8 = 0;
const KIND_STALL: u8 = 1;
/// Worker→supervisor only: the guest cancelled the endpoint named in the frame's `ep` (xHCI Stop
/// Endpoint — see `crates/limina-vmm/src/moc_usb.rs`). Carries no payload.
const KIND_CANCEL: u8 = 2;

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
            serve_conn(stream, &engine, verifier.as_ref(), &canceller);
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
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    // Set when the guest cancels, cleared as each new command is forwarded: the executor reads it
    // to decide whether its reply is still wanted (see `run_commands`).
    let cancelled = Arc::new(AtomicBool::new(false));

    std::thread::scope(|scope| {
        let exec_cancelled = cancelled.clone();
        scope.spawn(move || run_commands(rx, engine, verifier, writer, &exec_cancelled));
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
                    cancelled.store(true, Ordering::SeqCst);
                    canceller.cancel();
                }
                continue;
            }
            cancelled.store(false, Ordering::SeqCst);
            if tx.send(cmd).is_err() {
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
    rx: mpsc::Receiver<Vec<u8>>,
    engine: &Engine,
    verifier: &dyn Verifier,
    mut writer: UnixStream,
    cancelled: &AtomicBool,
) {
    for cmd in rx {
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
        if cancelled.load(Ordering::SeqCst) {
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

        std::thread::scope(|scope| {
            scope.spawn(|| serve_conn(supervisor, &engine, &verifier, &canceller));

            // Guest asks to verify; the prompt goes up and the engine thread parks in it.
            write_frame(&mut worker, 0x01, KIND_DATA, &[0x40, 0xff, 0x73]).unwrap();
            up_rx.recv().unwrap();

            // Guest walks away (its read was cancelled). This must be read *now*, not after the
            // prompt resolves — otherwise nothing could ever take the sheet down.
            write_frame(&mut worker, EP_MOC_IN, KIND_CANCEL, &[]).unwrap();
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

    /// The ordinary path is untouched: a command still gets its reply back on the right endpoint.
    #[test]
    fn a_command_still_gets_its_reply() {
        let (mut worker, supervisor) = UnixStream::pair().unwrap();
        let store = Arc::new(MocStore::in_memory());
        let engine = Engine::new(store, "test-vm".into());
        let verifier = AlwaysApprove;
        let canceller = PromptCanceller::new(Arc::new(AtomicU64::new(0)));

        std::thread::scope(|scope| {
            scope.spawn(|| serve_conn(supervisor, &engine, &verifier, &canceller));
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
