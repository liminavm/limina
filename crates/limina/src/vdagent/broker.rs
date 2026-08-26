// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The I/O half of the vdagent transport: one socket, one [`Session`], one pasteboard.
//!
//! Deliberately thin — every decision about *what* to say lives in [`super::session`], so
//! what remains here is framing bytes on and off the port and applying the effects. There
//! is no `poll`/`select`: the reader blocks on the socket, and a host copy arrives on the
//! clipboard poller's own thread. Both funnel through the session mutex, so the
//! conversation stays strictly ordered whichever side speaks first.
//!
//! Lifetime: one broker per worker *spawn*. The socketpair is created next to the spawn,
//! so a relaunch (guest reboot, resume) makes a new port, a new broker, and a fresh
//! [`Event::PortOpened`] — which is exactly the "announce once per port open" boundary the
//! protocol wants.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use super::codec::Reassembler;
use super::session::{Effect, Event, Session};
use crate::clipboard::Clipboard;

/// How long a write to the port may block before we give up on the agent. A guest that
/// stopped reading is a broken clipboard, not a reason to stall the host.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A live vdagent conversation. Held by the control plane, which pokes it when the host
/// pasteboard changes; the guest side drives itself from the reader thread.
pub struct VdAgent {
    session: Mutex<Session>,
    /// The write end. Separate lock from the session so a slow write can never be held
    /// across session state changes.
    out: Mutex<UnixStream>,
    clipboard: Arc<Clipboard>,
}

impl VdAgent {
    /// Take ownership of the port's host end and start the conversation.
    ///
    /// Announcing happens here, not on first use: `spice-vdagent` speaks only in reply, so
    /// a broker that waits to be spoken to waits forever.
    pub fn start(host_fd: OwnedFd, clipboard: Arc<Clipboard>) -> Result<Arc<VdAgent>> {
        let stream = UnixStream::from(host_fd);
        // A guest agent that stops draining the port must not be able to wedge us. Without
        // a bound, a full socket buffer blocks `write_all` forever on whichever thread is
        // sending — and for a host copy that is the SHARED clipboard poller, so a sick
        // vdagentd would take the control-plane transport down with it. Same reasoning as
        // `control::write_timeout` for peers.
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .context("bounding writes to the vdagent port")?;
        let reader = stream
            .try_clone()
            .context("cloning the vdagent port for the reader thread")?;

        let agent = Arc::new(VdAgent {
            session: Mutex::new(Session::new()),
            out: Mutex::new(stream),
            clipboard,
        });

        // The guest may not have opened its end yet — that is fine. The bytes sit in the
        // socket buffer until the port opens, and the agent's own greeting (which arrives
        // whenever it starts) re-synchronizes us either way.
        let opening = agent.session.lock().unwrap().on_event(Event::PortOpened);
        agent.apply(opening);

        let thread_agent = agent.clone();
        std::thread::Builder::new()
            .name("limina-vdagent".into())
            .spawn(move || thread_agent.read_loop(reader))
            .context("spawning the vdagent reader thread")?;

        Ok(agent)
    }

    /// The host pasteboard changed. Called from the clipboard poller, which owns the
    /// change-count bookkeeping for *both* transports — two pollers would race each other
    /// for the same change and each would see only half of them.
    pub fn host_copy(&self, text: String) {
        // Bind first: as a temporary inside the call, the session guard would live until the
        // end of the statement — i.e. across the write — which is the whole thing the two
        // separate locks exist to prevent.
        let effects = self.session.lock().unwrap().on_event(Event::HostCopy(text));
        self.apply(effects);
    }

    fn read_loop(self: Arc<Self>, mut reader: UnixStream) {
        let mut frames = Reassembler::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => {
                    log::debug!("vdagent: port closed");
                    return self.port_lost();
                }
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn!("vdagent: read failed: {e}");
                    return self.port_lost();
                }
            };

            // A framing error means the byte stream desynchronized, and there is no way to
            // resync a stream protocol from the middle. Stop reading rather than feed the
            // session garbage — the clipboard degrades, the VM is untouched.
            //
            // At ERROR, not WARN: this is unrecoverable for the life of the worker, and
            // the 2026-08-26 outage sat unnoticed in a log nobody was reading at WARN.
            // Say what it costs, so the next reader does not have to infer it.
            let messages = match frames.push(&buf[..n]) {
                Ok(m) => m,
                Err(e) => {
                    log::error!(
                        "vdagent: {e:#}; the clipboard is off for the life of this VM \
                         (it comes back on the next guest reboot or resume)"
                    );
                    return self.port_lost();
                }
            };

            for (msg_type, data) in messages {
                let effects = {
                    let mut session = self.session.lock().unwrap();
                    match session.on_wire(msg_type, &data) {
                        Ok(effects) => effects,
                        Err(e) => {
                            // One malformed message is not a desync: the framing was fine,
                            // so skip it and keep the conversation.
                            log::warn!("vdagent: ignoring message type {msg_type}: {e:#}");
                            continue;
                        }
                    }
                };
                self.apply(effects);
            }
        }
    }

    /// Tell the session we have gone deaf, so it stops offering the guest a clipboard it
    /// can no longer serve. The write half often still works, which is the trap: without
    /// this, host copies keep announcing grabs that strip the guest's own clipboard and
    /// give nothing back.
    fn port_lost(&self) {
        let effects = self.session.lock().unwrap().on_event(Event::PortLost);
        self.apply(effects);
    }

    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(bytes) => {
                    let out = self.out.lock().unwrap();
                    if let Err(e) = (&*out).write_all(&bytes) {
                        // Either the port died or the agent stopped draining it past
                        // WRITE_TIMEOUT. A timed-out `write_all` may have written part of a
                        // message, so the stream is no longer parseable in either direction:
                        // shut it down, which also wakes the reader thread out of its blocking
                        // read. The clipboard degrades; the VM is untouched.
                        log::warn!("vdagent: write failed ({e}); abandoning the port");
                        let _ = out.shutdown(std::net::Shutdown::Both);
                        // The shutdown unblocks the reader's `read` with 0, and it marks
                        // the session lost from there. Saying it here as well would
                        // re-enter `apply` from inside itself for no gain.
                        return;
                    }
                }
                Effect::SetPasteboard(text) => self.clipboard.set_from_guest(&text),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdagent::codec::{self, AgentMessage};

    /// The guest half of the port: a hand-driven `spice-vdagent` that speaks the real wire
    /// format. This is what makes the test worth having over the session unit tests — it
    /// exercises the socket, the reader thread, and the pasteboard together.
    struct MockAgent {
        stream: UnixStream,
        frames: Reassembler,
        pending: std::collections::VecDeque<AgentMessage>,
    }

    impl MockAgent {
        fn new(stream: UnixStream) -> MockAgent {
            // Bounded, so a broker that never answers fails the test instead of hanging it.
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            MockAgent {
                stream,
                frames: Reassembler::new(),
                pending: std::collections::VecDeque::new(),
            }
        }

        fn send(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).unwrap();
        }

        /// The next message the broker sent us.
        fn recv(&mut self) -> AgentMessage {
            loop {
                if let Some(msg) = self.pending.pop_front() {
                    return msg;
                }
                let mut buf = [0u8; 4096];
                let n = self
                    .stream
                    .read(&mut buf)
                    .expect("timed out waiting for the broker to say something");
                assert_ne!(n, 0, "the broker closed the port");
                for (t, d) in self.frames.push(&buf[..n]).unwrap() {
                    self.pending.push_back(codec::decode(t, &d, true).unwrap());
                }
            }
        }
    }

    /// `LIMINA_PASTEBOARD` is process-global and read inside `Clipboard::new`, so two
    /// broker tests starting at once could each end up on the other's pasteboard. Held
    /// across the whole test, not just the construction, since the pasteboard content is
    /// what every assertion is about.
    static PASTEBOARD_ENV: Mutex<()> = Mutex::new(());

    /// A broker wired to a private named pasteboard (never the user's real one) and a mock
    /// agent on the far end of a real socketpair.
    fn setup(
        pasteboard: &str,
    ) -> (
        Arc<VdAgent>,
        Arc<Clipboard>,
        MockAgent,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = PASTEBOARD_ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LIMINA_PASTEBOARD", pasteboard);
        let clipboard = Arc::new(Clipboard::new());
        let (host, guest) = crate::supervisor::socketpair(libc::SOCK_STREAM).unwrap();
        let agent = VdAgent::start(host, clipboard.clone()).unwrap();
        (
            agent,
            clipboard,
            MockAgent::new(UnixStream::from(guest)),
            guard,
        )
    }

    /// Complete the greeting: the broker announces on port open, we announce back.
    fn greet(mock: &mut MockAgent) {
        match mock.recv() {
            AgentMessage::AnnounceCapabilities { request, .. } => {
                assert!(request, "the broker must ask the agent to announce back")
            }
            other => panic!("expected an announce, got {other:?}"),
        }
        mock.send(&codec::encode_announce(false));
    }

    #[test]
    fn a_guest_copy_travels_over_the_real_port_onto_the_pasteboard() {
        let (_agent, clipboard, mut mock, _env) = setup("limina-vdagent-test-guest-copy");
        greet(&mut mock);

        // The guest copies; the agent grabs; we pull it eagerly.
        mock.send(&codec::encode_grab(
            codec::SELECTION_CLIPBOARD,
            &[codec::CLIPBOARD_UTF8_TEXT],
        ));
        assert_eq!(
            mock.recv(),
            AgentMessage::ClipboardRequest {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT
            }
        );

        mock.send(&codec::encode_clipboard(
            codec::SELECTION_CLIPBOARD,
            codec::CLIPBOARD_UTF8_TEXT,
            "copied in the guest".as_bytes(),
        ));

        // The pasteboard write happens on the reader thread, so poll for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if clipboard.current_text().as_deref() == Some("copied in the guest") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the guest's copy never reached the pasteboard (got {:?})",
                clipboard.current_text()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn a_host_copy_is_offered_and_then_served_on_demand() {
        let (agent, _clipboard, mut mock, _env) = setup("limina-vdagent-test-host-copy");
        greet(&mut mock);

        agent.host_copy("copied on the host".into());
        assert_eq!(
            mock.recv(),
            AgentMessage::ClipboardGrab {
                selection: codec::SELECTION_CLIPBOARD,
                types: vec![codec::CLIPBOARD_UTF8_TEXT]
            }
        );

        mock.send(&codec::encode_request(
            codec::SELECTION_CLIPBOARD,
            codec::CLIPBOARD_UTF8_TEXT,
        ));
        assert_eq!(
            mock.recv(),
            AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT,
                data: b"copied on the host".to_vec()
            }
        );
    }

    /// Frame a message the way the guest really does — one unsplit chunk, no 2048 split
    /// (`vdagent_virtio_port_write_start`). The mock must not use our own `encode`, or it
    /// would politely chunk its writes and never reproduce what a real agent sends.
    fn encode_unsplit_clipboard(text: &str) -> Vec<u8> {
        let mut body = vec![codec::SELECTION_CLIPBOARD, 0, 0, 0];
        body.extend_from_slice(&codec::CLIPBOARD_UTF8_TEXT.to_le_bytes());
        body.extend_from_slice(text.as_bytes());

        // sizeof(VDAgentMessage): protocol + type + opaque + size.
        const MESSAGE_HEADER_LEN: usize = 4 + 4 + 8 + 4;

        let mut wire = Vec::new();
        wire.extend_from_slice(&codec::VDP_CLIENT_PORT.to_le_bytes());
        wire.extend_from_slice(&((MESSAGE_HEADER_LEN + body.len()) as u32).to_le_bytes());
        wire.extend_from_slice(&codec::PROTOCOL.to_le_bytes());
        wire.extend_from_slice(&codec::msg_type::CLIPBOARD.to_le_bytes());
        wire.extend_from_slice(&0u64.to_le_bytes());
        wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wire.extend_from_slice(&body);
        wire
    }

    #[test]
    fn a_big_guest_copy_arrives_unsplit_and_does_not_kill_the_clipboard() {
        // The 2026-08-26 outage end to end: a guest copy over ~2 KB comes in as a single
        // oversized chunk. It must land on the pasteboard, and — the part that made the
        // bug so expensive — the port must still be alive afterwards.
        let (agent, clipboard, mut mock, _env) = setup("limina-vdagent-test-unsplit");
        greet(&mut mock);

        let big = "x".repeat(3574);
        mock.send(&codec::encode_grab(
            codec::SELECTION_CLIPBOARD,
            &[codec::CLIPBOARD_UTF8_TEXT],
        ));
        assert!(matches!(mock.recv(), AgentMessage::ClipboardRequest { .. }));
        mock.send(&encode_unsplit_clipboard(&big));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if clipboard.current_text().as_deref() == Some(big.as_str()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the oversized guest chunk never reached the pasteboard"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Still talking: a host copy after the big paste must still be offered. Before the
        // fix the reader thread was gone by now and this grab never arrived.
        agent.host_copy("still alive".into());
        assert_eq!(
            mock.recv(),
            AgentMessage::ClipboardGrab {
                selection: codec::SELECTION_CLIPBOARD,
                types: vec![codec::CLIPBOARD_UTF8_TEXT]
            }
        );
    }

    #[test]
    fn a_host_copy_after_the_port_dies_does_not_announce_a_grab_it_cannot_serve() {
        // A grab we cannot answer makes the agent take X11 ownership in the guest and
        // then strand it: the guest's own clipboard becomes owned-but-unreadable. Silence
        // is the correct degrade.
        let (agent, _clipboard, mut mock, _env) = setup("limina-vdagent-test-deaf");
        greet(&mut mock);

        // Kill the guest end and wait for the reader to notice.
        mock.stream.shutdown(std::net::Shutdown::Both).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !agent.session.lock().unwrap().port_is_lost() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reader never noticed the port died"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // The write half may well still accept bytes; the point is that we produce none.
        // Asserted on the effects rather than the socket because "nothing was sent" is
        // not observable on a far end we just closed.
        let effects = agent
            .session
            .lock()
            .unwrap()
            .on_event(Event::HostCopy("copied while deaf".into()));
        assert!(
            effects.is_empty(),
            "a deaf broker still offered the guest a clipboard: {effects:?}"
        );
    }

    #[test]
    fn content_larger_than_one_chunk_survives_the_socket() {
        // The chunking is unit-tested, but only a real socket proves the reassembly holds
        // when the kernel hands the far end whatever split it feels like.
        let (agent, _clipboard, mut mock, _env) = setup("limina-vdagent-test-big");
        greet(&mut mock);

        let big = "a longer clipboard payload. ".repeat(2000); // ~56 KB, dozens of chunks
        agent.host_copy(big.clone());
        assert!(matches!(mock.recv(), AgentMessage::ClipboardGrab { .. }));

        mock.send(&codec::encode_request(
            codec::SELECTION_CLIPBOARD,
            codec::CLIPBOARD_UTF8_TEXT,
        ));
        assert_eq!(
            mock.recv(),
            AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT,
                data: big.into_bytes()
            }
        );
    }
}
