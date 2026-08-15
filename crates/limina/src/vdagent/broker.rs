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
        agent.apply(agent.session.lock().unwrap().on_event(Event::PortOpened));

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
        self.apply(self.session.lock().unwrap().on_event(Event::HostCopy(text)));
    }

    fn read_loop(self: Arc<Self>, mut reader: UnixStream) {
        let mut frames = Reassembler::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => {
                    log::debug!("vdagent: port closed");
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn!("vdagent: read failed: {e}");
                    return;
                }
            };

            // A framing error means the byte stream desynchronized, and there is no way to
            // resync a stream protocol from the middle. Stop reading rather than feed the
            // session garbage — the clipboard degrades, the VM is untouched.
            let messages = match frames.push(&buf[..n]) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("vdagent: {e:#}; abandoning the port");
                    return;
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

    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(bytes) => {
                    if let Err(e) = self.out.lock().unwrap().write_all(&bytes) {
                        log::warn!("vdagent: write failed: {e}");
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
