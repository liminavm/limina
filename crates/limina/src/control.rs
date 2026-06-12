// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host side of the limina-proto control plane (M5/D8).
//!
//! The supervisor owns this channel: it binds a unix socket the worker bridges to the
//! guest's vsock (`CID_HOST:CONTROL_PORT`), accepts the agent when (if!) one connects,
//! answers HELLO with WELCOME, tracks liveness, and — the first real payoff — turns
//! window-close / SIGTERM into an **orderly guest power-off** by sending SHUTDOWN and
//! letting the agent run the guest's own shutdown path, instead of going straight to the
//! GPIO power button (which stock EFI guests ignore) and SIGKILL.
//!
//! Everything here is opportunistic: a guest without an agent simply never connects and
//! every caller falls back to the pre-existing teardown ladder. The agent may also
//! reconnect (guest reboot), so the accept loop runs for the supervisor's lifetime.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use limina_proto::{read_message, write_message, Message, Shutdown, Welcome, CHANNEL_CONTROL};

/// How long the orderly path gets before the caller escalates (power button / SIGKILL).
pub const AGENT_GRACE: Duration = Duration::from_secs(5);

/// Socket path to remove on exit (the windowed path leaves via `process::exit`, which
/// skips destructors — same pattern as `gateway::cleanup`).
static CLEANUP_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Remove the control socket file (idempotent; safe from any exit path).
pub fn cleanup() {
    if let Some(path) = CLEANUP_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(path);
    }
}

/// The supervisor's handle to the control plane. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct ControlPlane {
    inner: Arc<Inner>,
}

struct Inner {
    /// Write half (a `try_clone`) of the connected agent's stream, present between a
    /// completed HELLO/WELCOME handshake and disconnect.
    agent: Mutex<Option<UnixStream>>,
}

impl ControlPlane {
    /// Bind `socket_path` and start the accept/serve thread. The returned handle is what
    /// shutdown paths use; the thread runs for the process's lifetime.
    pub fn start(socket_path: &Path) -> Result<ControlPlane> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding control socket {socket_path:?}"))?;
        *CLEANUP_PATH.lock().unwrap() = Some(socket_path.to_path_buf());

        let inner = Arc::new(Inner {
            agent: Mutex::new(None),
        });
        let serve_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-control".into())
            .spawn(move || accept_loop(listener, serve_inner))
            .context("spawning the control-plane thread")?;
        Ok(ControlPlane { inner })
    }

    /// Ask the connected agent to power the guest off. Returns `true` if the request was
    /// sent (the caller should give it [`AGENT_GRACE`] before escalating); `false` if no
    /// agent is connected or the send failed (escalate immediately).
    pub fn request_shutdown(&self, grace: Duration) -> bool {
        let mut slot = self.inner.agent.lock().unwrap();
        let Some(stream) = slot.as_mut() else {
            return false;
        };
        let msg = Message::Shutdown(Shutdown {
            grace_ms: grace.as_millis() as u64,
        });
        match write_message(stream, CHANNEL_CONTROL, &msg) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("control: sending SHUTDOWN failed ({e}); falling back");
                *slot = None;
                false
            }
        }
    }
}

/// Accept agents forever; one at a time (the channel is a singleton by design — a second
/// connect replaces a dead predecessor after its serve loop ends).
fn accept_loop(listener: UnixListener, inner: Arc<Inner>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                set_nosigpipe(&stream);
                if let Err(e) = serve_agent(stream, &inner) {
                    log::warn!("control: agent connection ended with error: {e}");
                }
                *inner.agent.lock().unwrap() = None;
                log::info!("control: guest agent disconnected");
            }
            Err(e) => {
                // Listener broken (e.g. socket unlinked early) — nothing left to serve.
                log::warn!("control: accept failed: {e}");
                return;
            }
        }
    }
}

/// One agent session: HELLO → WELCOME, then serve until EOF. Heartbeats keep liveness;
/// unknown types get ERROR(UNSUPPORTED) — never fatal, per the protocol's ground rule.
fn serve_agent(mut stream: UnixStream, inner: &Inner) -> std::io::Result<()> {
    let (_, first) = read_message(&mut stream)?;
    let hello = match first {
        Message::Hello(h) => h,
        other => {
            log::warn!("control: peer's first message was not HELLO ({other:?}); dropping");
            return Ok(());
        }
    };
    log::info!(
        "control: guest agent connected: {} caps={:?} pagesize={}",
        hello.agent,
        hello.caps,
        hello.pagesize
    );
    write_message(
        &mut stream,
        CHANNEL_CONTROL,
        &Message::Welcome(Welcome {
            caps: vec!["shutdown".to_string()],
        }),
    )?;
    *inner.agent.lock().unwrap() = Some(stream.try_clone()?);

    loop {
        match read_message(&mut stream) {
            Ok((_, Message::Heartbeat(_))) => {} // liveness; nothing to track yet
            Ok((_, Message::ShutdownAck)) => {
                log::info!("control: agent acknowledged shutdown");
            }
            Ok((_, Message::Error(e))) => {
                log::warn!("control: agent reported error: {e:?}");
            }
            Ok((_, Message::Unknown { msg_type, .. })) => {
                write_message(
                    &mut stream,
                    CHANNEL_CONTROL,
                    &Message::unsupported(msg_type),
                )?;
            }
            // HELLO twice / host-only messages from a guest: ignore rather than die.
            Ok((_, Message::Hello(_)))
            | Ok((_, Message::Welcome(_)))
            | Ok((_, Message::Shutdown(_))) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// Writing to a dead peer must fail with EPIPE, not raise SIGPIPE and kill the
/// supervisor (macOS has no MSG_NOSIGNAL).
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
