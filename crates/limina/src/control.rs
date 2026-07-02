// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host side of the limina-proto control plane (M5/D8).
//!
//! The supervisor owns this channel: it binds a unix socket the worker bridges to the
//! guest's vsock (`CID_HOST:CONTROL_PORT`), accepts agents as they connect, answers
//! HELLO with WELCOME, tracks the connected peers, and — the first real payoff — turns
//! window-close / SIGTERM into an **orderly guest power-off** by sending SHUTDOWN and
//! letting the agent run the guest's own shutdown path, instead of going straight to the
//! GPIO power button (which stock EFI guests ignore) and SIGKILL.
//!
//! The plane serves **multiple concurrent connections** (the clipboard spike settled the
//! guest topology: a root `limina-agent` plus per-session user helpers, each with its own
//! vsock connection and capability set — vsock connect needs no root). Each peer gets its
//! own serve thread and registry entry; requests are routed by capability (SHUTDOWN goes
//! to every `shutdown`-capable peer; power-off is idempotent, first one wins).
//!
//! Everything here is opportunistic: a guest without an agent simply never connects and
//! every caller falls back to the pre-existing teardown ladder. Agents may also
//! reconnect (guest reboot), so the accept loop runs for the supervisor's lifetime.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use limina_proto::{
    read_message, write_message, Message, Shutdown, Welcome, CHANNEL_CLIPBOARD, CHANNEL_CONTROL,
};

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

/// A connected, handshaken agent: the write half of its stream plus what it declared
/// in HELLO. Registered after WELCOME, removed when its serve thread ends (or a write
/// to it fails).
///
/// The write half is mutexed because a peer has TWO writers: its serve thread (replies)
/// and broadcasters (the clipboard poller, shutdown requests). `write_message` is two
/// `write_all`s, so unsynchronized writers could interleave mid-frame and corrupt the
/// stream.
struct Peer {
    id: u64,
    agent: String,
    caps: Vec<String>,
    stream: Arc<Mutex<UnixStream>>,
    /// When the peer last said anything (updated by its serve loop on every inbound
    /// message — heartbeats included). The liveness monitor reads this.
    last_seen: Arc<Mutex<Instant>>,
    /// Whether the monitor currently considers this peer silent (so transitions are
    /// logged once, not every sweep).
    silent: Arc<AtomicBool>,
}

impl Peer {
    fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    fn send(&self, msg: &Message, channel: u32) -> std::io::Result<()> {
        write_message(&mut *self.stream.lock().unwrap(), channel, msg)
    }
}

/// How long a peer may go without any inbound message before the supervisor reports it
/// silent. Agents heartbeat every second, so the default 5s means ~5 missed beats.
/// Override with `LIMINA_AGENT_SILENT_SECS` (tests shorten it).
fn silent_threshold() -> Duration {
    let secs = std::env::var("LIMINA_AGENT_SILENT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    Duration::from_secs(secs)
}

/// How long a write to a peer may block before we declare the peer wedged and drop it.
/// A peer that hasn't drained its socket buffer for this long has effectively stopped
/// serving its side of the protocol; without a bound, one such peer blocks whoever is
/// sending to it forever — historically the Ctrl-C shutdown ladder, since a blocking
/// send also serialized against registration and the liveness sweep. Override with
/// `LIMINA_CONTROL_WRITE_TIMEOUT_MS` (tests shorten it); 0 disables the bound.
fn write_timeout() -> Option<Duration> {
    let ms = std::env::var("LIMINA_CONTROL_WRITE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000u64);
    (ms > 0).then(|| Duration::from_millis(ms))
}

struct Inner {
    peers: Mutex<Vec<Arc<Peer>>>,
    next_id: AtomicU64,
    clipboard: crate::clipboard::Clipboard,
    /// M6 PSI autoballoon policy, driven by guest `MemPressure` reports. `None` unless `--memory`
    /// configured a dynamic range.
    balloon_policy: Option<crate::balloon_policy::BalloonPolicy>,
}

impl ControlPlane {
    /// Bind `socket_path` and start the accept thread (which spawns a serve thread per
    /// connection). The returned handle is what shutdown paths use; the threads run for
    /// the process's lifetime.
    pub fn start(
        socket_path: &Path,
        balloon_policy: Option<crate::balloon_policy::BalloonPolicy>,
    ) -> Result<ControlPlane> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding control socket {socket_path:?}"))?;
        *CLEANUP_PATH.lock().unwrap() = Some(socket_path.to_path_buf());

        let inner = Arc::new(Inner {
            peers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            clipboard: crate::clipboard::Clipboard::new(),
            balloon_policy,
        });
        let serve_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-control".into())
            .spawn(move || accept_loop(listener, serve_inner))
            .context("spawning the control-plane thread")?;

        // The clipboard poller: macOS has no pasteboard-change notification, so watch
        // changeCount and broadcast host copies to clipboard-capable peers.
        let poll_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-clipboard".into())
            .spawn(move || {
                let every = crate::clipboard::Clipboard::poll_interval();
                loop {
                    std::thread::sleep(every);
                    if let Some(offer) = poll_inner.clipboard.poll_local_change() {
                        poll_inner.broadcast_clipboard(&offer);
                    }
                }
            })
            .context("spawning the clipboard poll thread")?;

        // The liveness monitor: agents heartbeat every second; report (once) any peer
        // that goes quiet past the threshold, and its recovery. This is the signal a
        // status surface (CLI/UI) consumes later — for now the supervisor log IS the
        // surface.
        let live_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-liveness".into())
            .spawn(move || {
                let threshold = silent_threshold();
                loop {
                    std::thread::sleep(threshold.min(Duration::from_secs(1)));
                    for peer in live_inner.peers.lock().unwrap().iter() {
                        let quiet = peer.last_seen.lock().unwrap().elapsed();
                        if quiet > threshold {
                            if !peer.silent.swap(true, Ordering::Relaxed) {
                                log::warn!(
                                    "control: agent {} silent for {:.1}s (no heartbeat)",
                                    peer.agent,
                                    quiet.as_secs_f64()
                                );
                            }
                        } else if peer.silent.swap(false, Ordering::Relaxed) {
                            log::info!("control: agent {} heartbeating again", peer.agent);
                        }
                    }
                }
            })
            .context("spawning the liveness monitor thread")?;
        Ok(ControlPlane { inner })
    }

    /// Ask the guest to power off, via every connected `shutdown`-capable agent (the
    /// guest's power-off is idempotent — whichever acts first wins). Returns `true` if
    /// at least one request was sent (the caller should give it [`AGENT_GRACE`] before
    /// escalating); `false` if no capable agent is connected or every send failed
    /// (escalate immediately). Peers whose send fails are dropped from the registry.
    pub fn request_shutdown(&self, grace: Duration) -> bool {
        let msg = Message::Shutdown(Shutdown {
            grace_ms: grace.as_millis() as u64,
        });
        let mut sent = false;
        for peer in self
            .inner
            .send_to_capable("shutdown", &msg, CHANNEL_CONTROL)
        {
            log::info!("control: SHUTDOWN sent to {peer}");
            sent = true;
        }
        sent
    }
}

impl Inner {
    /// Send a clipboard message to every clipboard-capable peer, dropping any whose
    /// send fails (dead connection).
    fn broadcast_clipboard(&self, msg: &Message) {
        self.send_to_capable("clipboard", msg, CHANNEL_CLIPBOARD);
    }

    /// Send `msg` to every peer with `cap`, returning the agents that took it and
    /// dropping any whose send fails (dead or wedged connection).
    ///
    /// Sends happen on a SNAPSHOT of the registry, never while holding the peers lock:
    /// each peer's socket has a write timeout ([`write_timeout`]) rather than blocking
    /// forever, but even a bounded stall must not serialize registration, the liveness
    /// sweep, and the shutdown ladder behind one slow peer. (Snapshotting is what makes
    /// this safe: a peer that deregisters concurrently just gets a failed send here.)
    fn send_to_capable(&self, cap: &str, msg: &Message, channel: u32) -> Vec<String> {
        let targets: Vec<Arc<Peer>> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.has_cap(cap))
            .cloned()
            .collect();
        let mut delivered = Vec::new();
        let mut failed = Vec::new();
        for peer in targets {
            match peer.send(msg, channel) {
                Ok(()) => delivered.push(peer.agent.clone()),
                Err(e) => {
                    log::warn!("control: send to {} failed ({e}); dropping it", peer.agent);
                    failed.push(peer.id);
                }
            }
        }
        if !failed.is_empty() {
            self.peers
                .lock()
                .unwrap()
                .retain(|p| !failed.contains(&p.id));
        }
        delivered
    }
}

/// Accept connections forever, one serve thread per peer (agents are independent: the
/// root daemon and per-session helpers must be able to talk concurrently).
fn accept_loop(listener: UnixListener, inner: Arc<Inner>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                set_nosigpipe(&stream);
                // Bound every write to this peer (the option rides the socket, so the
                // serve thread's `try_clone` write half inherits it): a peer that stops
                // draining must produce a send ERROR to be dropped over, not a hang.
                if let Err(e) = stream.set_write_timeout(write_timeout()) {
                    log::warn!("control: set_write_timeout failed ({e}); sends may block");
                }
                let peer_inner = inner.clone();
                let spawned = std::thread::Builder::new()
                    .name("limina-control-peer".into())
                    .spawn(move || {
                        if let Err(e) = serve_agent(stream, &peer_inner) {
                            log::warn!("control: agent connection ended with error: {e}");
                        }
                    });
                if let Err(e) = spawned {
                    log::warn!("control: cannot spawn peer thread: {e}");
                }
            }
            Err(e) => {
                // Listener broken (e.g. socket unlinked early) — nothing left to serve.
                log::warn!("control: accept failed: {e}");
                return;
            }
        }
    }
}

/// One agent session: HELLO → WELCOME, register, then serve until EOF. Heartbeats keep
/// liveness; unknown types get ERROR(UNSUPPORTED) — never fatal, per the protocol's
/// ground rule. The peer is deregistered on any exit.
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
            caps: vec!["shutdown".to_string(), "clipboard".to_string()],
        }),
    )?;

    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let peer = Arc::new(Peer {
        id,
        agent: hello.agent.clone(),
        caps: hello.caps,
        stream: writer.clone(),
        last_seen: last_seen.clone(),
        silent: Arc::new(AtomicBool::new(false)),
    });
    // A late joiner needs the CURRENT host clipboard, not just the next change.
    if peer.has_cap("clipboard") {
        if let Some(offer) = inner.clipboard.initial_offer() {
            let _ = peer.send(&offer, CHANNEL_CLIPBOARD);
        }
    }
    inner.peers.lock().unwrap().push(peer);

    let result = serve_loop(&mut stream, &writer, inner, &last_seen);
    inner.peers.lock().unwrap().retain(|p| p.id != id);
    log::info!("control: guest agent disconnected: {}", hello.agent);
    result
}

/// The post-handshake read loop for one peer. Replies go through the peer's `writer`
/// mutex — never the raw read stream — because broadcasters write concurrently.
fn serve_loop(
    stream: &mut UnixStream,
    writer: &Mutex<UnixStream>,
    inner: &Inner,
    last_seen: &Mutex<Instant>,
) -> std::io::Result<()> {
    let reply = |msg: &Message, channel: u32| -> std::io::Result<()> {
        write_message(&mut *writer.lock().unwrap(), channel, msg)
    };
    loop {
        let msg = read_message(stream);
        if msg.is_ok() {
            // ANY inbound message proves the peer alive, not just heartbeats.
            *last_seen.lock().unwrap() = Instant::now();
        }
        match msg {
            Ok((_, Message::Heartbeat(_))) => {} // liveness only; tracked above
            Ok((_, Message::ShutdownAck)) => {
                log::info!("control: agent acknowledged shutdown");
            }
            // The clipboard conversation (see crate::clipboard for the protocol rules).
            Ok((_, Message::ClipOffer(o))) => {
                if let Some(msg) = inner.clipboard.on_offer(o) {
                    reply(&msg, CHANNEL_CLIPBOARD)?;
                }
            }
            Ok((_, Message::ClipRequest(r))) => {
                if let Some(msg) = inner.clipboard.on_request(r) {
                    reply(&msg, CHANNEL_CLIPBOARD)?;
                }
            }
            Ok((_, Message::ClipData(d))) => inner.clipboard.on_data(d),
            // M6: feed guest memory-pressure reports to the autoballoon policy (if configured).
            Ok((_, Message::MemPressure(p))) => {
                if let Some(policy) = &inner.balloon_policy {
                    policy.on_pressure(&p);
                }
            }
            Ok((_, Message::Error(e))) => {
                log::warn!("control: agent reported error: {e:?}");
            }
            Ok((_, Message::Unknown { msg_type, .. })) => {
                reply(&Message::unsupported(msg_type), CHANNEL_CONTROL)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use limina_proto::Hello;

    /// Connect a fake agent to the plane's socket, handshake, and return the stream.
    fn connect_agent(path: &Path, caps: &[&str]) -> UnixStream {
        let mut s = UnixStream::connect(path).unwrap();
        write_message(
            &mut s,
            CHANNEL_CONTROL,
            &Message::Hello(Hello {
                agent: "test-agent/0".into(),
                caps: caps.iter().map(|c| c.to_string()).collect(),
                pagesize: 4096,
            }),
        )
        .unwrap();
        let (_, welcome) = read_message(&mut s).unwrap();
        assert!(matches!(welcome, Message::Welcome(_)));
        s
    }

    /// A shutdown-capable agent that never drains its socket must not stall
    /// `request_shutdown`: `Peer::send` used to be a blocking write with no timeout,
    /// made while holding the peers lock — one wedged agent froze the Ctrl-C shutdown
    /// ladder, new-peer registration, and the liveness sweep. The plane must time the
    /// write out and drop the peer instead.
    #[test]
    fn wedged_peer_cannot_stall_request_shutdown() {
        std::env::set_var("LIMINA_CONTROL_WRITE_TIMEOUT_MS", "200");
        let path =
            std::env::temp_dir().join(format!("limina-ctl-test-{}.sock", std::process::id()));
        let plane = ControlPlane::start(&path, None).unwrap();
        // HELLO, then never read again: the socket buffers fill and stay full.
        let wedged = connect_agent(&path, &["shutdown"]);

        // Hammer SHUTDOWN until the buffers fill. Run in a thread so a regression
        // (send blocking forever) fails the test instead of hanging it. Each frame is
        // ~30 bytes; 8192 sends is comfortably past any default unix-socket buffering.
        let (tx, rx) = std::sync::mpsc::channel();
        let hammer = plane.clone();
        std::thread::spawn(move || {
            for _ in 0..8192 {
                if !hammer.request_shutdown(Duration::from_secs(0)) {
                    break; // peer dropped after the write timed out
                }
            }
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("request_shutdown wedged on a non-draining peer");

        // The wedged peer must have been dropped from the registry.
        assert!(!plane.request_shutdown(Duration::from_secs(0)));
        drop(wedged);
        let _ = std::fs::remove_file(&path);
    }
}
