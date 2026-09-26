// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host side of the M5 clipboard bridge: NSPasteboard ↔ control-plane peers.
//!
//! Symmetric eager-pull protocol over [`limina_proto::CHANNEL_CLIPBOARD`]:
//! - **guest→host:** a clipboard-capable peer OFFERs; we REQUEST the text and write the
//!   DATA onto the pasteboard (recording the change count we caused, so the poller
//!   doesn't echo it back as a fresh host copy — the loop-prevention rule).
//! - **host→guest:** macOS has no pasteboard-change notification, so a poller watches
//!   `changeCount` (default 500 ms, `LIMINA_CLIP_POLL_MS` to tune/tighten in tests); on a
//!   change it caches the text and OFFERs it to every clipboard-capable peer, serving
//!   their REQUESTs from the cache.
//!
//! Only the **newest serial** in each direction is honored, so a stale in-flight
//! exchange can never resurrect an older clipboard. Host→guest serials come from one
//! counter here, but **guest→host serials are per-peer**: a guest runs one
//! `limina-agent-session` per graphical session, each numbering its own offers from 1, so
//! serials from different peers are unrelated. Ratcheting them together let a long-lived
//! session's high-water mark silently swallow every offer from a newer one — that session's
//! clipboard simply never arrived. The ratchet therefore lives
//! with the peer's serve loop ([`crate::control`]), one per connection.
//!
//! M5 scope is text-only; richer formats ride the same OFFER/REQUEST/DATA shape later.
//!
//! Tests point this at a private NAMED pasteboard via `LIMINA_PASTEBOARD` (the general
//! pasteboard is the product default) — see `crates/limina-test/tests/l1_clipboard.rs`.

// The locks are loom's under `--cfg loom`, so the control plane's model can interleave a host
// copy, the poller and an agent joining (`crate::control::loom_model`).
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(not(loom))]
use std::sync::Mutex;

use limina_proto::{ClipData, ClipOffer, ClipRequest, Message};
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Put text on the general pasteboard: the menu's copy verbs (the SSH command, the About
/// build stamp), which are one-shot writes with none of the bridge's bookkeeping.
///
/// Deliberately the general pasteboard and NOT the bridge's [`Pasteboard`]: a copy the user
/// asked for is a host copy like any other and should reach the guest the same way, whereas a
/// write through the bridge would record its own change count to suppress exactly that.
pub(crate) fn copy_to_pasteboard(text: &str) {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

/// The only format M5 speaks. (Guests also commonly advertise bare `text/plain`;
/// we accept either on offers and always request/serve the utf-8 one we can honor.)
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Host clipboard-bridge state, owned by the control plane's `Inner`.
///
/// Generic over the pasteboard itself only so the control plane's loom model can stand in for
/// NSPasteboard; everything else is [`NsPasteboard`].
pub struct Clipboard<S: PasteboardServer = NsPasteboard> {
    /// All NSPasteboard access is serialized here (AppKit doesn't promise thread-safety;
    /// the poller and peer serve threads all funnel through this lock).
    pasteboard: Mutex<Pasteboard<S>>,
    /// The host's current offer. Taken before `pasteboard` wherever both are held.
    host: Mutex<HostOffer>,
}

/// The host's current outstanding offer: its serial (0 = none yet) and the text served on peer
/// REQUESTs for it. One lock for both, so a REQUEST is never answered with another serial's text.
#[derive(Default)]
struct HostOffer {
    serial: u64,
    text: Option<String>,
}

impl HostOffer {
    /// Offer `text`, under a new serial only if it is not what the current one already offers.
    ///
    /// A new serial retires the old one: every peer holding it has its REQUEST refused, and
    /// nothing re-offers it. That is right when the content changed — the new offer goes to them
    /// too — but a late joiner's greeting offers only to itself, so re-minting the same text
    /// there stranded every other guest mid-exchange (found by `crate::control::loom_model`).
    fn offer(&mut self, text: String) -> Message {
        if self.text.as_deref() != Some(text.as_str()) {
            self.serial += 1;
            self.text = Some(text);
        }
        Message::ClipOffer(ClipOffer {
            serial: self.serial,
            mime_types: vec![TEXT_MIME.to_string(), "text/plain".to_string()],
        })
    }
}

impl Clipboard {
    pub fn new() -> Clipboard {
        Clipboard::with_server(NsPasteboard::from_env())
    }

    /// The poll cadence (`LIMINA_CLIP_POLL_MS`, default 500 ms).
    pub fn poll_interval() -> std::time::Duration {
        let ms = std::env::var("LIMINA_CLIP_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        std::time::Duration::from_millis(ms)
    }
}

impl<S: PasteboardServer> Clipboard<S> {
    pub fn with_server(server: S) -> Clipboard<S> {
        Clipboard {
            pasteboard: Mutex::new(Pasteboard::new(server)),
            host: Mutex::new(HostOffer::default()),
        }
    }

    /// One poller tick: the new host clipboard text if the pasteboard changed since the
    /// last look, else `None`.
    ///
    /// Returns the *text* rather than a ready-made OFFER because two transports now want
    /// it (the control plane's peers and the vdagent port), and the change may only be
    /// consumed once: `take_changed_text` advances the change-count high-water mark, so a
    /// second poller would see "no change" and that transport would silently never learn
    /// about the copy. One poller, one consumption, fan out afterwards.
    pub fn poll_local_change(&self) -> Option<String> {
        self.pasteboard.lock().unwrap().take_changed_text()
    }

    /// The host's current content as an offer for a newly-connected peer (None if the
    /// pasteboard is empty). Late joiners need the current clipboard, not the next.
    ///
    /// The pasteboard is read with the offer held, so the serial order is the order the
    /// content was read in. Read first and offered after, a poller offering a newer copy in
    /// between was outranked by this older text, which then answered every peer's REQUEST
    /// while the pasteboard held the newer one — and the poller, having consumed the change,
    /// never offered it again.
    pub fn initial_offer(&self) -> Option<Message> {
        let mut host = self.host.lock().unwrap();
        // No change-count bump involved: read whatever is there right now.
        let text = self.pasteboard.lock().unwrap().current_text()?;
        Some(host.offer(text))
    }

    /// Put guest-sourced text on the host pasteboard, whichever transport carried it.
    /// The write records its own change count, so the poller does not read it back as a
    /// fresh host copy and bounce it to the guest again.
    pub fn set_from_guest(&self, text: &str) {
        self.pasteboard.lock().unwrap().set_text(text);
    }

    /// Whatever is on the host pasteboard right now, without consuming a change. Only the
    /// vdagent broker's tests read this — the product code learns about content through
    /// the poller, which must consume the change exactly once.
    #[cfg(test)]
    pub fn current_text(&self) -> Option<String> {
        self.pasteboard.lock().unwrap().current_text()
    }

    /// Wrap host text as a control-plane OFFER, caching the content behind it for the
    /// REQUESTs that follow ([`HostOffer::offer`]).
    pub fn make_offer(&self, text: String) -> Message {
        self.host.lock().unwrap().offer(text)
    }

    /// A peer announced new guest clipboard content: request the text (if it has a
    /// format we speak).
    ///
    /// `peer_serial` is **that peer's** high-water mark, owned by its serve loop — never a
    /// shared one. Peers number their offers independently, so comparing across them would
    /// mute whichever session started later (see the module docs).
    pub fn on_offer(&self, offer: ClipOffer, peer_serial: &mut u64) -> Option<Message> {
        if !offer
            .mime_types
            .iter()
            .any(|m| m == TEXT_MIME || m == "text/plain")
        {
            return None; // nothing we can represent yet
        }
        // Only ratchet forward: an offer older than one we've already requested *from this
        // peer* is stale.
        if offer.serial < *peer_serial {
            return None;
        }
        *peer_serial = offer.serial;
        Some(Message::ClipRequest(ClipRequest {
            serial: offer.serial,
            mime_type: TEXT_MIME.to_string(),
        }))
    }

    /// A peer wants the data behind the host's offer `serial`.
    pub fn on_request(&self, req: ClipRequest) -> Option<Message> {
        let text = {
            let host = self.host.lock().unwrap();
            if req.serial != host.serial {
                return None; // stale: they'll get a fresh offer soon enough
            }
            host.text.clone()?
        };
        // Content a frame can't carry gets an explicit error, not a doomed write that
        // would kill the peer's serve thread (and never silent truncation). The guest
        // keeps its current clipboard; chunking is the future fix.
        if text.len() > limina_proto::MAX_CLIP_DATA {
            log::warn!(
                "clipboard: host content is {} bytes (> {} max); answering TOO_LARGE",
                text.len(),
                limina_proto::MAX_CLIP_DATA
            );
            return Some(Message::Error(limina_proto::ErrorMsg {
                code: limina_proto::ERR_TOO_LARGE,
                ref_type: limina_proto::msg_type::CLIP_REQUEST,
                detail: format!("clipboard content is {} bytes", text.len()),
            }));
        }
        Some(Message::ClipData(ClipData {
            serial: req.serial,
            mime_type: TEXT_MIME.to_string(),
            data: text.into_bytes(),
        }))
    }

    /// A peer delivered the guest clipboard content we requested: put it on the
    /// pasteboard (self-change suppressed inside).
    ///
    /// `peer_serial` is the same per-connection high-water mark [`Self::on_offer`] advanced.
    pub fn on_data(&self, data: ClipData, peer_serial: u64) {
        if data.serial != peer_serial {
            return; // stale delivery for a superseded offer
        }
        let text = String::from_utf8_lossy(&data.data).into_owned();
        self.pasteboard.lock().unwrap().set_text(&text);
    }
}

/// What the bridge needs from a pasteboard: the three calls it makes on NSPasteboard. A seam so
/// the control plane's loom model can play the pasteboard server, whose change count other
/// apps move under us.
pub trait PasteboardServer: Send {
    fn change_count(&self) -> isize;
    fn text(&self) -> Option<String>;
    /// `clearContents` then `setString`: AppKit bumps the change count on the first only.
    fn replace(&self, text: &str);
}

/// The real pasteboard: a named one for tests (`LIMINA_PASTEBOARD`) or the general one.
pub struct NsPasteboard(Retained<NSPasteboard>);

// SAFETY: NSPasteboard messages funnel to the pasteboard server; the class itself is
// documented thread-agnostic (no main-thread requirement), and all our access is
// additionally serialized behind the owning Mutex.
unsafe impl Send for NsPasteboard {}

impl NsPasteboard {
    fn from_env() -> NsPasteboard {
        NsPasteboard(match std::env::var("LIMINA_PASTEBOARD") {
            Ok(name) if !name.is_empty() => {
                log::info!("clipboard: using named pasteboard {name:?} (LIMINA_PASTEBOARD)");
                NSPasteboard::pasteboardWithName(&NSString::from_str(&name))
            }
            _ => NSPasteboard::generalPasteboard(),
        })
    }
}

impl PasteboardServer for NsPasteboard {
    fn change_count(&self) -> isize {
        self.0.changeCount()
    }

    fn text(&self) -> Option<String> {
        unsafe { self.0.stringForType(NSPasteboardTypeString) }.map(|s| s.to_string())
    }

    fn replace(&self, text: &str) {
        unsafe {
            self.0.clearContents();
            self.0
                .setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
        }
    }
}

/// The pasteboard plus the change-count bookkeeping that both detects app copies and
/// suppresses our own writes.
struct Pasteboard<S> {
    server: S,
    /// The change count as of our last look (or our last write — which is what keeps
    /// guest-sourced content from echoing back to the guest).
    last_count: isize,
}

impl<S: PasteboardServer> Pasteboard<S> {
    fn new(server: S) -> Pasteboard<S> {
        // Pre-existing content is not a "change"; late joiners get it via initial_offer.
        let last_count = server.change_count();
        Pasteboard { server, last_count }
    }

    /// The text content if the pasteboard changed since the last look (consumes the
    /// change: subsequent calls return None until the next app copy).
    ///
    /// A change with no text is NOT consumed: AppKit bumps `changeCount` on the writer's
    /// `clearContents` and not on the `setString` that follows, so a poll landing between
    /// the two sees the bump while the pasteboard is still empty. Advancing `last_count`
    /// there made the copy permanently invisible — the string arrives with no further bump
    /// and every later poll saw "no change" (offers silently dropped ~10% of the time for
    /// large content, whose NSString construction widens the window to milliseconds).
    /// Leaving `last_count` stale keeps the change pending until the text lands; a
    /// genuinely text-less change (image copy, bare clear) just re-reads on each poll and
    /// never offers, which is the correct behavior for it anyway.
    fn take_changed_text(&mut self) -> Option<String> {
        let count = self.server.change_count();
        if count == self.last_count {
            return None;
        }
        let text = self.current_text()?;
        self.last_count = count;
        Some(text)
    }

    fn current_text(&self) -> Option<String> {
        self.server.text()
    }

    /// Replace the pasteboard content, recording the resulting change count so the
    /// poller doesn't treat our own write as a fresh host copy.
    fn set_text(&mut self, text: &str) {
        self.server.replace(text);
        self.last_count = self.server.change_count();
    }
}
