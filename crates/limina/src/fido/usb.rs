// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Supervisor side of the stock-tier FIDO **USB** transport (M14 Stage C).
//!
//! The worker cold-plugs a HID report-pipe gadget with the FIDO identity and binds a UNIX
//! listener (`--fido-socket`); here we connect to it and shuttle raw 64-byte CTAPHID frames
//! through a [`FidoAuthenticator`] — the *same* authenticator, store, and keepalive engine
//! the uhid/agent path uses ([`crate::fido::pump`]). This is the design-doc-consistent proxy
//! split: mechanism (USB bus + report pipe) in the worker, policy (CTAP2 + Secure Enclave +
//! passkey store) in the supervisor, where the Apple-Development-signed binary can reach the
//! enclave. A guest with the agent gets uhid; a stock guest gets USB; a guest with both gets
//! both (two authenticators sharing one concurrency-safe store).
//!
//! The socket path is stable across a worker reboot relaunch, so we simply reconnect: one
//! [`FidoAuthenticator`] per connection (per worker life), which repeated hidraw opens
//! re-INIT and allocate channels on — exactly as one uhid device serves repeated opens.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::store::FidoStore;
use super::{pump, FidoAuthenticator, REPORT_SIZE};

/// Spawn the FIDO USB serve thread: connect to the worker's gadget socket and bridge CTAPHID
/// to an authenticator backed by `store`. Reconnects across worker relaunches; runs for the
/// supervisor's lifetime. Call only when the `fido` capability is available (a store exists).
pub fn serve(socket_path: PathBuf, store: Arc<FidoStore>) {
    std::thread::Builder::new()
        .name("limina-fido-usb-sup".into())
        .spawn(move || serve_loop(&socket_path, store))
        .ok();
}

fn serve_loop(socket_path: &Path, store: Arc<FidoStore>) {
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path) {
            log::info!("fido-usb: connected to the worker gadget at {socket_path:?}");
            serve_conn(stream, &store);
            log::info!("fido-usb: worker gadget connection ended; retrying");
        }
        // Worker not up yet, or relaunching across a guest reboot — retry.
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Serve one connection: a fresh [`FidoAuthenticator`] sharing the per-VM `store`, pumping
/// each inbound 64-byte report through the shared keepalive engine and writing responses back.
fn serve_conn(stream: UnixStream, store: &Arc<FidoStore>) {
    set_nosigpipe(&stream);
    let writer = match stream.try_clone() {
        Ok(w) => Mutex::new(w),
        Err(e) => {
            log::warn!("fido-usb: cloning the gadget socket failed: {e}");
            return;
        }
    };
    let mut reader = stream;
    let mut auth = FidoAuthenticator::new(store.clone());
    let mut buf = [0u8; REPORT_SIZE];
    loop {
        if reader.read_exact(&mut buf).is_err() {
            return; // EOF/error: worker gone
        }
        let send =
            |f: [u8; REPORT_SIZE]| -> std::io::Result<()> { writer.lock().unwrap().write_all(&f) };
        if let Err(e) = pump(&mut auth, &buf, send) {
            log::warn!("fido-usb: send failed: {e}");
            return;
        }
    }
}

/// Writing to a socket whose peer (the worker) has exited must fail with EPIPE, not raise
/// SIGPIPE and kill the supervisor (macOS has no MSG_NOSIGNAL).
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
