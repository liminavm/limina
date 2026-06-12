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
//! exchange can never resurrect an older clipboard. M5 scope is text-only; richer
//! formats ride the same OFFER/REQUEST/DATA shape later.
//!
//! Tests point this at a private NAMED pasteboard via `LIMINA_PASTEBOARD` (the general
//! pasteboard is the product default) — see `crates/limina-test/tests/l1_clipboard.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use limina_proto::{ClipData, ClipOffer, ClipRequest, Message};
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// The only format M5 speaks. (Guests also commonly advertise bare `text/plain`;
/// we accept either on offers and always request/serve the utf-8 one we can honor.)
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Host clipboard-bridge state, owned by the control plane's `Inner`.
pub struct Clipboard {
    /// All NSPasteboard access is serialized here (AppKit doesn't promise thread-safety;
    /// the poller and peer serve threads all funnel through this lock).
    pasteboard: Mutex<Pasteboard>,
    /// Serial of the host's current outstanding offer (0 = none yet).
    host_serial: AtomicU64,
    /// The text backing the host's current offer, served on peer REQUESTs.
    host_text: Mutex<Option<String>>,
    /// Newest guest offer serial we've requested; stale DATA is dropped.
    guest_serial: AtomicU64,
}

impl Clipboard {
    pub fn new() -> Clipboard {
        Clipboard {
            pasteboard: Mutex::new(Pasteboard::new()),
            host_serial: AtomicU64::new(0),
            host_text: Mutex::new(None),
            guest_serial: AtomicU64::new(0),
        }
    }

    /// The poll cadence (`LIMINA_CLIP_POLL_MS`, default 500 ms).
    pub fn poll_interval() -> std::time::Duration {
        let ms = std::env::var("LIMINA_CLIP_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        std::time::Duration::from_millis(ms)
    }

    /// One poller tick: returns an OFFER to broadcast if the pasteboard changed since
    /// the last look (and caches the content backing it).
    pub fn poll_local_change(&self) -> Option<Message> {
        let text = self.pasteboard.lock().unwrap().take_changed_text()?;
        Some(self.make_offer(text))
    }

    /// The host's current content as a fresh offer for a newly-connected peer (None if
    /// the pasteboard is empty). Late joiners need the current clipboard, not the next.
    pub fn initial_offer(&self) -> Option<Message> {
        // No change-count bump involved: read whatever is there right now.
        let text = self.pasteboard.lock().unwrap().current_text()?;
        Some(self.make_offer(text))
    }

    fn make_offer(&self, text: String) -> Message {
        let serial = self.host_serial.fetch_add(1, Ordering::Relaxed) + 1;
        *self.host_text.lock().unwrap() = Some(text);
        Message::ClipOffer(ClipOffer {
            serial,
            mime_types: vec![TEXT_MIME.to_string(), "text/plain".to_string()],
        })
    }

    /// A peer announced new guest clipboard content: request the text (if it has a
    /// format we speak).
    pub fn on_offer(&self, offer: ClipOffer) -> Option<Message> {
        if !offer
            .mime_types
            .iter()
            .any(|m| m == TEXT_MIME || m == "text/plain")
        {
            return None; // nothing we can represent yet
        }
        // Only ratchet forward: an offer older than one we've already requested is stale.
        let newest = self.guest_serial.fetch_max(offer.serial, Ordering::Relaxed);
        if offer.serial < newest {
            return None;
        }
        Some(Message::ClipRequest(ClipRequest {
            serial: offer.serial,
            mime_type: TEXT_MIME.to_string(),
        }))
    }

    /// A peer wants the data behind the host's offer `serial`.
    pub fn on_request(&self, req: ClipRequest) -> Option<Message> {
        if req.serial != self.host_serial.load(Ordering::Relaxed) {
            return None; // stale: they'll get a fresh offer soon enough
        }
        let text = self.host_text.lock().unwrap().clone()?;
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
    pub fn on_data(&self, data: ClipData) {
        if data.serial != self.guest_serial.load(Ordering::Relaxed) {
            return; // stale delivery for a superseded offer
        }
        let text = String::from_utf8_lossy(&data.data).into_owned();
        self.pasteboard.lock().unwrap().set_text(&text);
    }
}

/// The NSPasteboard wrapper: a named pasteboard for tests (`LIMINA_PASTEBOARD`) or the
/// general (real) one, plus the change-count bookkeeping that both detects app copies
/// and suppresses our own writes.
struct Pasteboard {
    pb: Retained<NSPasteboard>,
    /// The change count as of our last look (or our last write — which is what keeps
    /// guest-sourced content from echoing back to the guest).
    last_count: isize,
}

// SAFETY: NSPasteboard messages funnel to the pasteboard server; the class itself is
// documented thread-agnostic (no main-thread requirement), and all our access is
// additionally serialized behind the owning Mutex.
unsafe impl Send for Pasteboard {}

impl Pasteboard {
    fn new() -> Pasteboard {
        let pb = match std::env::var("LIMINA_PASTEBOARD") {
            Ok(name) if !name.is_empty() => {
                log::info!("clipboard: using named pasteboard {name:?} (LIMINA_PASTEBOARD)");
                NSPasteboard::pasteboardWithName(&NSString::from_str(&name))
            }
            _ => NSPasteboard::generalPasteboard(),
        };
        // Pre-existing content is not a "change"; late joiners get it via initial_offer.
        let last_count = pb.changeCount();
        Pasteboard { pb, last_count }
    }

    /// The text content if the pasteboard changed since the last look (consumes the
    /// change: subsequent calls return None until the next app copy).
    fn take_changed_text(&mut self) -> Option<String> {
        let count = self.pb.changeCount();
        if count == self.last_count {
            return None;
        }
        self.last_count = count;
        self.current_text()
    }

    fn current_text(&self) -> Option<String> {
        unsafe { self.pb.stringForType(NSPasteboardTypeString) }.map(|s| s.to_string())
    }

    /// Replace the pasteboard content, recording the resulting change count so the
    /// poller doesn't treat our own write as a fresh host copy.
    fn set_text(&mut self, text: &str) {
        unsafe {
            self.pb.clearContents();
            self.pb
                .setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
            self.last_count = self.pb.changeCount();
        }
    }
}
