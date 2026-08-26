// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The vdagent conversation, as a pure state machine.
//!
//! Everything that decides *what* to say to `spice-vdagent` lives here — capability
//! negotiation, clipboard ownership, staleness — and it does no I/O at all: events in,
//! [`Effect`]s out. The socket half is [`super::broker`], which is thin enough to read in
//! one sitting precisely because none of the policy is in it.
//!
//! ## The protocol, as we play it
//!
//! `CLIPBOARD_BY_DEMAND` is a *promise*, not a push. When either side copies, it sends a
//! `GRAB` naming the formats it could produce; the content moves only when the other side
//! actually pastes and sends a `REQUEST`. So:
//!
//! - **host copy** → we `GRAB`; later, maybe, the guest `REQUEST`s and we answer with
//!   `CLIPBOARD` carrying the text.
//! - **guest copy** → the agent `GRAB`s; we `REQUEST` immediately, because macOS has no
//!   promise-based pasteboard we could bridge lazily (an NSPasteboard promise would have
//!   to be fulfilled on the AppKit thread from a guest round-trip). Eager-pull matches
//!   what the control-plane transport already does.
//!
//! ## Two rules learned the hard way, both encoded as tests
//!
//! **Announce once per port open, then only on request.** Re-announcing on a timer looks
//! to `vdagentd` like a new SPICE client connecting over and over; it resets its clipboard
//! state each time and suppresses the very grabs we want (measured in
//! `spikes/m12-spice-port/`). And when we *answer* the agent's announce, we clear the
//! request bit — two peers that both ask for a reply ping-pong forever.
//!
//! **An agent announce that arrives mid-conversation means the agent restarted.** It has
//! forgotten that we hold the clipboard, so we re-`GRAB` if we still do; otherwise the
//! guest can never paste what the host copied before the restart, with nothing in any log
//! to say why.

use super::codec::{self, AgentMessage};

/// What the outside world tells the session about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The port came up (or came back): open the dialogue.
    PortOpened,
    /// The host pasteboard changed — this is the new content.
    HostCopy(String),
    /// The agent said something.
    Agent(AgentMessage),
    /// We can no longer hear the agent: the port closed, a read failed, or the byte
    /// stream desynchronized past any hope of resync. The write half may still work,
    /// which is exactly why this has to be said out loud — see [`Session::port_lost`].
    PortLost,
}

/// What the session wants done. Frames are already chunked and ready for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Write these bytes to the port.
    Send(Vec<u8>),
    /// Put this text on the host pasteboard (guest → host paste).
    SetPasteboard(String),
}

/// The negotiated conversation with one `spice-vdagent`.
pub struct Session {
    /// Capabilities the agent announced; `None` until it greets us. Its
    /// `CLIPBOARD_SELECTION` bit decides the on-wire layout of every clipboard message.
    agent_caps: Option<Vec<u32>>,
    /// Set once we have announced, so a re-open announces again but nothing else does.
    announced: bool,
    /// The host content behind our outstanding `GRAB` (`None` = the guest owns the
    /// clipboard, or we have never copied).
    host_text: Option<String>,
    /// Whether we are waiting on the guest to answer a `REQUEST` we sent. Anything that
    /// supersedes that request (a newer guest grab, a host copy) clears it, so a late
    /// answer to a superseded request cannot resurrect an older clipboard.
    awaiting_guest_data: bool,
    /// Set once the read side is gone. A `GRAB` is a promise to answer the `REQUEST` it
    /// provokes, and answering needs the read side — so once we are deaf, every further
    /// grab is a promise we cannot keep. Worse than useless: the agent takes X11
    /// clipboard ownership on our behalf and then cannot produce the content, so the
    /// guest's own clipboard is left owned-but-unreadable and a host copy *destroys*
    /// what the guest had. Staying quiet degrades; announcing anyway damages.
    port_lost: bool,
}

impl Session {
    pub fn new() -> Session {
        Session {
            agent_caps: None,
            announced: false,
            host_text: None,
            awaiting_guest_data: false,
            port_lost: false,
        }
    }

    /// Whether the read side is gone. Exposed so the I/O half can wait for the reader
    /// thread to have noticed, which is otherwise unobservable from outside.
    #[cfg(test)]
    pub fn port_is_lost(&self) -> bool {
        self.port_lost
    }

    /// Whether the agent negotiated `CLIPBOARD_SELECTION` (which shifts every clipboard
    /// message by 4 bytes). Before it has announced we assume our own advertisement holds,
    /// which is also what we encode with — the only messages we send before its greeting
    /// are announces, which carry no selection field at all.
    fn selection(&self) -> bool {
        match &self.agent_caps {
            Some(caps) => codec::has_cap(caps, codec::cap::CLIPBOARD_SELECTION),
            None => true,
        }
    }

    /// Whether the agent can speak the by-demand handshake at all. A guest whose agent
    /// only offers the legacy push clipboard gets no clipboard rather than a broken one —
    /// the stock-degrade rule: missing enhancements degrade, they do not misbehave.
    fn by_demand(&self) -> bool {
        match &self.agent_caps {
            Some(caps) => codec::has_cap(caps, codec::cap::CLIPBOARD_BY_DEMAND),
            // Not yet known: act as if it will, and let its announce correct us.
            None => true,
        }
    }

    /// Decode a reassembled message with the capabilities *this* session negotiated, then
    /// act on it. The selection capability shifts every clipboard message by 4 bytes, so
    /// only the session knows how to read its own peer's bytes — which is why the I/O side
    /// hands over `(msg_type, data)` rather than a decoded message.
    pub fn on_wire(&mut self, msg_type: u32, data: &[u8]) -> anyhow::Result<Vec<Effect>> {
        let msg = codec::decode(msg_type, data, self.selection())?;
        Ok(self.on_event(Event::Agent(msg)))
    }

    pub fn on_event(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::PortOpened => {
                // A fresh port is a fresh conversation: whatever the last agent knew is
                // gone with it.
                self.agent_caps = None;
                self.awaiting_guest_data = false;
                self.announced = true;
                self.port_lost = false;
                vec![Effect::Send(codec::encode_announce(true))]
            }

            Event::PortLost => {
                self.port_lost = true;
                self.awaiting_guest_data = false;
                Vec::new()
            }

            Event::HostCopy(text) => {
                // We own the clipboard now; any answer still in flight from the guest is
                // for content the user has already superseded.
                self.awaiting_guest_data = false;
                self.host_text = Some(text);
                if !self.by_demand() || self.port_lost {
                    return Vec::new();
                }
                vec![Effect::Send(codec::encode_grab(
                    self.selection_byte(),
                    &[codec::CLIPBOARD_UTF8_TEXT],
                ))]
            }

            Event::Agent(msg) => self.on_agent(msg),
        }
    }

    fn on_agent(&mut self, msg: AgentMessage) -> Vec<Effect> {
        match msg {
            AgentMessage::AnnounceCapabilities { request, caps } => {
                let restarted = self.agent_caps.is_some();
                self.agent_caps = Some(caps);
                let mut out = Vec::new();
                // Answer its request, with the request bit CLEARED (see the module docs).
                if request {
                    out.push(Effect::Send(codec::encode_announce(false)));
                }
                // A second announce means a restarted agent: it no longer knows we hold
                // the clipboard, so tell it again or the guest can never paste.
                if restarted {
                    self.awaiting_guest_data = false;
                    if self.host_text.is_some() && self.by_demand() {
                        out.push(Effect::Send(codec::encode_grab(
                            self.selection_byte(),
                            &[codec::CLIPBOARD_UTF8_TEXT],
                        )));
                    }
                }
                out
            }

            AgentMessage::ClipboardGrab { selection, types } => {
                if !self.is_our_selection(selection) {
                    return Vec::new();
                }
                // The guest owns the clipboard now; our cached content is stale and must
                // not be served to a later request.
                self.host_text = None;
                if !types.contains(&codec::CLIPBOARD_UTF8_TEXT) {
                    // An image or file-list copy: nothing we can represent yet. Not
                    // requesting is the whole answer — the host pasteboard simply keeps
                    // what it had.
                    self.awaiting_guest_data = false;
                    return Vec::new();
                }
                self.awaiting_guest_data = true;
                vec![Effect::Send(codec::encode_request(
                    selection,
                    codec::CLIPBOARD_UTF8_TEXT,
                ))]
            }

            AgentMessage::ClipboardRequest { selection, format } => {
                if !self.is_our_selection(selection) {
                    return Vec::new();
                }
                // Always answer. A request we drop on the floor is a guest paste that
                // hangs waiting for content that will never come; the protocol's way to
                // say "I cannot" is an empty CLIPBOARD with format NONE.
                let text = match (&self.host_text, format) {
                    (Some(t), codec::CLIPBOARD_UTF8_TEXT) => Some(t.clone()),
                    _ => None,
                };
                let frame = match text {
                    Some(t) => {
                        codec::encode_clipboard(selection, codec::CLIPBOARD_UTF8_TEXT, t.as_bytes())
                    }
                    None => codec::encode_clipboard(selection, codec::CLIPBOARD_NONE, &[]),
                };
                vec![Effect::Send(frame)]
            }

            AgentMessage::Clipboard {
                selection,
                format,
                data,
            } => {
                if !self.is_our_selection(selection) {
                    return Vec::new();
                }
                // Only ever an answer to a REQUEST we sent. If we are not waiting for one,
                // it answers a superseded request and delivering it would resurrect an
                // older clipboard over whatever the user has since copied.
                if !self.awaiting_guest_data {
                    return Vec::new();
                }
                self.awaiting_guest_data = false;
                if format != codec::CLIPBOARD_UTF8_TEXT || data.is_empty() {
                    // The agent's "I could not produce it" reply.
                    return Vec::new();
                }
                // Lossy on purpose: the guest declared UTF-8, but a truncated or
                // mis-encoded payload should land as replacement characters rather than
                // silently dropping the user's paste.
                vec![Effect::SetPasteboard(
                    String::from_utf8_lossy(&data).into_owned(),
                )]
            }

            // The guest released the clipboard. macOS has no counterpart — NSPasteboard
            // content simply persists — so there is nothing to do but stop expecting data.
            AgentMessage::ClipboardRelease { selection } => {
                if self.is_our_selection(selection) {
                    self.awaiting_guest_data = false;
                }
                Vec::new()
            }

            AgentMessage::Other { .. } => Vec::new(),
        }
    }

    /// The selection byte we put on messages we originate.
    fn selection_byte(&self) -> u8 {
        codec::SELECTION_CLIPBOARD
    }

    /// Whether a message concerns the selection we bridge. With the selection capability
    /// negotiated, an agent also reports PRIMARY (the X11 middle-click selection), which
    /// has no NSPasteboard counterpart — mirroring it would rewrite the user's clipboard
    /// on every mouse drag inside the guest.
    fn is_our_selection(&self, selection: u8) -> bool {
        !self.selection() || selection == codec::SELECTION_CLIPBOARD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode the frames in `effects` back into messages, so tests assert on protocol
    /// meaning rather than on byte strings.
    fn sent(effects: &[Effect]) -> Vec<AgentMessage> {
        let mut r = codec::Reassembler::new();
        let mut out = Vec::new();
        for e in effects {
            if let Effect::Send(bytes) = e {
                for (t, d) in r.push(bytes).unwrap() {
                    out.push(codec::decode(t, &d, true).unwrap());
                }
            }
        }
        out
    }

    fn pasted(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::SetPasteboard(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// A session that has completed the greeting, which is where most tests start.
    fn negotiated() -> Session {
        let mut s = Session::new();
        s.on_event(Event::PortOpened);
        s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: false,
            caps: vec![
                (1 << codec::cap::CLIPBOARD_BY_DEMAND) | (1 << codec::cap::CLIPBOARD_SELECTION),
            ],
        }));
        s
    }

    #[test]
    fn a_lost_port_stops_offering_the_guest_a_clipboard() {
        let mut s = negotiated();
        assert!(
            !sent(&s.on_event(Event::HostCopy("before".into()))).is_empty(),
            "a healthy session must offer the copy"
        );

        s.on_event(Event::PortLost);
        assert!(
            s.on_event(Event::HostCopy("after".into())).is_empty(),
            "a grab we cannot answer strands the guest's own clipboard"
        );
    }

    #[test]
    fn reopening_the_port_offers_again() {
        // The lost flag must not outlive the port it describes, or a guest reboot would
        // come back to a permanently silent clipboard.
        let mut s = Session::new();
        s.on_event(Event::PortOpened);
        s.on_event(Event::PortLost);
        s.on_event(Event::PortOpened);
        s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: false,
            caps: vec![
                (1 << codec::cap::CLIPBOARD_BY_DEMAND) | (1 << codec::cap::CLIPBOARD_SELECTION),
            ],
        }));
        assert_eq!(
            sent(&s.on_event(Event::HostCopy("after the reboot".into()))),
            vec![AgentMessage::ClipboardGrab {
                selection: codec::SELECTION_CLIPBOARD,
                types: vec![codec::CLIPBOARD_UTF8_TEXT]
            }]
        );
    }

    #[test]
    fn opening_the_port_announces_once_and_asks_for_an_answer() {
        let mut s = Session::new();
        let out = s.on_event(Event::PortOpened);
        assert_eq!(
            sent(&out),
            vec![AgentMessage::AnnounceCapabilities {
                request: true,
                caps: vec![codec::our_caps()]
            }]
        );
    }

    #[test]
    fn answering_the_agents_greeting_clears_the_request_bit() {
        // Both sides asking for an answer is an infinite polite exchange.
        let mut s = Session::new();
        s.on_event(Event::PortOpened);
        let out = s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: true,
            caps: vec![1 << codec::cap::CLIPBOARD_BY_DEMAND],
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::AnnounceCapabilities {
                request: false,
                caps: vec![codec::our_caps()]
            }]
        );
    }

    #[test]
    fn nothing_but_a_port_open_ever_announces() {
        // The anti-pattern this guards: a periodic announce reads to vdagentd as a client
        // reconnecting over and over, which resets its clipboard state each time.
        let mut s = negotiated();
        let mut announces = 0;
        for i in 0..20 {
            let out = s.on_event(Event::HostCopy(format!("copy {i}")));
            announces += sent(&out)
                .iter()
                .filter(|m| matches!(m, AgentMessage::AnnounceCapabilities { .. }))
                .count();
            let out = s.on_event(Event::Agent(AgentMessage::ClipboardRequest {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT,
            }));
            announces += sent(&out)
                .iter()
                .filter(|m| matches!(m, AgentMessage::AnnounceCapabilities { .. }))
                .count();
        }
        assert_eq!(announces, 0);
    }

    #[test]
    fn a_host_copy_grabs_but_sends_no_content_until_asked() {
        let mut s = negotiated();
        let out = s.on_event(Event::HostCopy("hello".into()));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::ClipboardGrab {
                selection: codec::SELECTION_CLIPBOARD,
                types: vec![codec::CLIPBOARD_UTF8_TEXT]
            }],
            "by-demand means the grab is a promise, not a delivery"
        );

        let out = s.on_event(Event::Agent(AgentMessage::ClipboardRequest {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT,
                data: b"hello".to_vec()
            }]
        );
    }

    #[test]
    fn a_guest_copy_is_pulled_eagerly_and_lands_on_the_pasteboard() {
        let mut s = negotiated();
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::ClipboardRequest {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_UTF8_TEXT
            }]
        );

        let out = s.on_event(Event::Agent(AgentMessage::Clipboard {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
            data: b"from the guest".to_vec(),
        }));
        assert_eq!(pasted(&out), vec!["from the guest".to_string()]);
    }

    #[test]
    fn a_request_we_cannot_satisfy_is_still_answered() {
        // Silence would hang the guest's paste forever.
        let mut s = negotiated();
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardRequest {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_NONE,
                data: vec![]
            }]
        );
    }

    #[test]
    fn a_request_for_a_format_we_do_not_have_gets_none_not_the_text() {
        let mut s = negotiated();
        s.on_event(Event::HostCopy("hello".into()));
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardRequest {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_IMAGE_PNG,
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_NONE,
                data: vec![]
            }],
            "answering a PNG request with utf-8 text would be a lie about the format"
        );
    }

    #[test]
    fn a_guest_grab_makes_our_cached_content_unservable() {
        // The guest owns the clipboard now; serving our old text to a later request would
        // hand the user back something they replaced.
        let mut s = negotiated();
        s.on_event(Event::HostCopy("host text".into()));
        s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardRequest {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
        }));
        assert_eq!(
            sent(&out),
            vec![AgentMessage::Clipboard {
                selection: codec::SELECTION_CLIPBOARD,
                format: codec::CLIPBOARD_NONE,
                data: vec![]
            }]
        );
    }

    #[test]
    fn a_late_answer_to_a_superseded_request_never_reaches_the_pasteboard() {
        // The staleness rule, in the shape it actually occurs: the guest copies, we ask,
        // and before the answer arrives the user copies something on the host. The stale
        // delivery must not overwrite the newer host copy.
        let mut s = negotiated();
        s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        s.on_event(Event::HostCopy("newer host copy".into()));

        let out = s.on_event(Event::Agent(AgentMessage::Clipboard {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
            data: b"older guest copy".to_vec(),
        }));
        assert!(
            pasted(&out).is_empty(),
            "a superseded answer must not resurrect an older clipboard"
        );
    }

    #[test]
    fn unsolicited_clipboard_data_is_ignored() {
        // A legacy-mode agent pushes content without being asked; with by-demand
        // negotiated we never asked, so it cannot be an answer to anything.
        let mut s = negotiated();
        let out = s.on_event(Event::Agent(AgentMessage::Clipboard {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_UTF8_TEXT,
            data: b"unasked for".to_vec(),
        }));
        assert!(pasted(&out).is_empty());
    }

    #[test]
    fn a_restarted_agent_is_told_again_that_the_host_holds_the_clipboard() {
        // Second announce = the daemon restarted and forgot our grab. Without a re-grab
        // the guest can never paste what the host copied before the restart, and nothing
        // in any log says why.
        let mut s = negotiated();
        s.on_event(Event::HostCopy("survives the restart".into()));

        let out = s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: true,
            caps: vec![
                (1 << codec::cap::CLIPBOARD_BY_DEMAND) | (1 << codec::cap::CLIPBOARD_SELECTION),
            ],
        }));
        assert_eq!(
            sent(&out),
            vec![
                AgentMessage::AnnounceCapabilities {
                    request: false,
                    caps: vec![codec::our_caps()]
                },
                AgentMessage::ClipboardGrab {
                    selection: codec::SELECTION_CLIPBOARD,
                    types: vec![codec::CLIPBOARD_UTF8_TEXT]
                },
            ]
        );
    }

    #[test]
    fn a_restarted_agent_is_not_re_grabbed_when_the_guest_owns_the_clipboard() {
        let mut s = negotiated();
        s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        let out = s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: false,
            caps: vec![1 << codec::cap::CLIPBOARD_BY_DEMAND],
        }));
        assert!(
            sent(&out).is_empty(),
            "we hold nothing, so there is nothing to re-announce ownership of"
        );
    }

    #[test]
    fn an_agent_without_by_demand_gets_no_grabs_at_all() {
        // Stock-degrade: a guest whose agent only speaks the legacy push clipboard loses
        // the feature rather than getting a half-working one.
        let mut s = Session::new();
        s.on_event(Event::PortOpened);
        s.on_event(Event::Agent(AgentMessage::AnnounceCapabilities {
            request: false,
            caps: vec![1 << codec::cap::CLIPBOARD], // legacy only
        }));
        let out = s.on_event(Event::HostCopy("hello".into()));
        assert!(sent(&out).is_empty());
    }

    #[test]
    fn the_primary_selection_is_left_alone() {
        // PRIMARY is the X11 middle-click selection — it changes on every drag inside the
        // guest, and mirroring it would rewrite the user's macOS clipboard constantly.
        let mut s = negotiated();
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: 1, // VD_AGENT_CLIPBOARD_SELECTION_PRIMARY
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        assert!(sent(&out).is_empty());

        // And an unsolicited PRIMARY delivery cannot sneak onto the pasteboard either.
        let out = s.on_event(Event::Agent(AgentMessage::Clipboard {
            selection: 1,
            format: codec::CLIPBOARD_UTF8_TEXT,
            data: b"drag selection".to_vec(),
        }));
        assert!(pasted(&out).is_empty());
    }

    #[test]
    fn a_guest_copy_in_a_format_we_cannot_represent_is_not_requested() {
        let mut s = negotiated();
        let out = s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_IMAGE_PNG],
        }));
        assert!(sent(&out).is_empty());
    }

    #[test]
    fn an_agent_that_cannot_produce_its_own_grab_leaves_the_pasteboard_alone() {
        // The agent answers a request it cannot satisfy with format NONE. That is not
        // content, and it must not clear or overwrite the host clipboard.
        let mut s = negotiated();
        s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        let out = s.on_event(Event::Agent(AgentMessage::Clipboard {
            selection: codec::SELECTION_CLIPBOARD,
            format: codec::CLIPBOARD_NONE,
            data: vec![],
        }));
        assert!(pasted(&out).is_empty());
    }

    #[test]
    fn reopening_the_port_forgets_the_previous_agents_capabilities() {
        // A new port is a new guest. Carrying the old agent's capability set over would
        // make us encode for a layout the new one may not have negotiated.
        let mut s = negotiated();
        s.on_event(Event::Agent(AgentMessage::ClipboardGrab {
            selection: codec::SELECTION_CLIPBOARD,
            types: vec![codec::CLIPBOARD_UTF8_TEXT],
        }));
        assert!(s.awaiting_guest_data);

        s.on_event(Event::PortOpened);
        assert!(s.agent_caps.is_none());
        assert!(!s.awaiting_guest_data);
    }
}
