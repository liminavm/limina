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
//! nothing (a zero-length command, which posts no read). The socket path is stable across a worker
//! reboot relaunch, so we simply reconnect.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::store::MocStore;
use super::{AlwaysApprove, Engine, Reply, SepVerifier, Verifier};

const KIND_DATA: u8 = 0;
const KIND_STALL: u8 = 1;

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
    // Pick the verifier once: the real biometric prompt, or CI's unconditional approve.
    let verifier: Box<dyn Verifier> = if test_approve() {
        log::info!("moc: LIMINA_FP_TEST_APPROVE set — verifying without a Touch ID prompt");
        Box::new(AlwaysApprove)
    } else {
        Box::new(SepVerifier)
    };
    let engine = Engine::new(store, vm_label);
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path) {
            log::info!("moc: connected to the worker gadget at {socket_path:?}");
            serve_conn(stream, &engine, verifier.as_ref());
            log::info!("moc: worker gadget connection ended; retrying");
        }
        // Worker not up yet, or relaunching across a guest reboot — retry.
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Serve one connection: read guest→host command frames, run each through the engine, write back the
/// reply (data / stall / nothing). Single-threaded — no keepalive is needed (the driver's finger-wait
/// reads have an infinite timeout, so a blocking Touch ID prompt never times the guest out).
fn serve_conn(stream: UnixStream, engine: &Engine, verifier: &dyn Verifier) {
    set_nosigpipe(&stream);
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("moc: cloning the gadget socket failed: {e}");
            return;
        }
    };
    let mut writer = stream;
    loop {
        let (_ep, _kind, cmd) = match read_frame(&mut reader) {
            Ok(f) => f,
            Err(_) => return, // EOF/error: worker gone
        };
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
