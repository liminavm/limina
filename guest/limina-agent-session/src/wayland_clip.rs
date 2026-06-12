// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The enhanced-tier clipboard backend: a Wayland ext-data-control-v1 client.
//!
//! Our patched mutter (see `patches/mutter/`) exposes the standardized
//! ext-data-control-v1 protocol, which lets a focusless client manage the seat
//! selection directly — no RemoteDesktop session, so no permanent "screen is being
//! shared" indicator in the GNOME panel. Stock mutter doesn't ship the protocol
//! (privacy stance, mutter#524), so [`WaylandClip::connect`] reports
//! [`ConnectError::NoDataControl`] there and the caller falls back to the
//! RemoteDesktop D-Bus backend.
//!
//! Shape: one dispatch thread owns the event queue and pumps compositor events into
//! the bridge's mpsc queue (same [`Event`] vocabulary as the D-Bus backend).
//! Requests (set_selection, receive) are issued from the bridge thread — the
//! wayland-client backend is thread-safe — with shared state behind a mutex.
//!
//! Loop prevention: mutter broadcasts every selection change to all data-control
//! devices, *including the one that set it*. We track our own live source; the echo
//! arrives with `our_source` still set and maps to `is_owner: true`, which the
//! bridge ignores. When a guest app takes the selection mutter cancels our source
//! *before* broadcasting the new owner, so the flag is already clear.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use crate::{Event, OFFER_MIMES, TEXT_MIME};

/// Why [`WaylandClip::connect`] didn't produce a backend.
pub enum ConnectError {
    /// Connected fine, but the compositor doesn't offer ext-data-control —
    /// stock mutter. Fall back to RemoteDesktop, don't retry this path.
    NoDataControl,
    /// Couldn't reach/handshake the display (session may still be coming up).
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoDataControl => write!(f, "compositor has no ext-data-control"),
            ConnectError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Per-offer user data: mime types accumulate from `offer` events between
/// `data_offer` and the `selection` event that references the offer.
type OfferMimes = Mutex<Vec<String>>;

/// State shared between the bridge thread (requests) and the dispatch thread (events).
struct Shared {
    /// The current selection offer (None = empty/unowned selection).
    selection: Option<ExtDataControlOfferV1>,
    /// Our live source while we own the selection (the loop-prevention flag).
    our_source: Option<ExtDataControlSourceV1>,
    /// Paste fds from `send` events, keyed by our own transfer serial.
    transfers: HashMap<u32, File>,
    next_serial: u32,
}

pub struct WaylandClip {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    shared: Arc<Mutex<Shared>>,
}

struct State {
    tx: mpsc::Sender<Event>,
    shared: Arc<Mutex<Shared>>,
}

impl WaylandClip {
    /// Connect to the session compositor and bind the data-control device.
    /// Spawns the dispatch thread on success.
    pub fn connect(tx: mpsc::Sender<Event>) -> Result<WaylandClip, ConnectError> {
        let conn = connect_display()?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| ConnectError::Other(e.to_string()))?;
        let qh = queue.handle();

        let manager: ExtDataControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| ConnectError::NoDataControl)?;
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| ConnectError::Other(format!("no wl_seat: {e}")))?;
        let device = manager.get_data_device(&seat, &qh, ());
        conn.flush()
            .map_err(|e| ConnectError::Other(e.to_string()))?;

        let shared = Arc::new(Mutex::new(Shared {
            selection: None,
            our_source: None,
            transfers: HashMap::new(),
            next_serial: 1,
        }));
        let mut state = State {
            tx,
            shared: shared.clone(),
        };
        std::thread::spawn(move || {
            loop {
                if queue.blocking_dispatch(&mut state).is_err() {
                    // Compositor gone (session ending / shell crash): tell the
                    // bridge so it can exit and let systemd restart us into the
                    // next session.
                    let _ = state.tx.send(Event::SessionGone);
                    return;
                }
            }
        });

        Ok(WaylandClip {
            conn,
            qh,
            manager,
            device,
            shared,
        })
    }

    /// Read the current guest selection content for `mime` (receive + read to EOF).
    pub fn selection_read(&self, mime: &str) -> Result<Vec<u8>, String> {
        let offer = self
            .shared
            .lock()
            .unwrap()
            .selection
            .clone()
            .ok_or("no current selection offer")?;
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        offer.receive(mime.to_string(), writer.as_fd());
        drop(writer); // the source's dup is the only write end left
        self.conn.flush().map_err(|e| e.to_string())?;
        let mut reader = reader;
        let mut data = Vec::new();
        reader.read_to_end(&mut data).map_err(|e| e.to_string())?;
        Ok(data)
    }

    /// Own the seat selection with our text formats (content served on transfers).
    pub fn set_selection(&self) -> Result<(), String> {
        let source = self.manager.create_data_source(&self.qh, ());
        for mime in OFFER_MIMES {
            source.offer(mime.to_string());
        }
        self.device.set_selection(Some(&source));
        // Flag ownership BEFORE flushing: the echo broadcast must find it set.
        // Replacing our own previous source cancels it, but `cancelled` only
        // clears the flag when it names the *live* source.
        self.shared.lock().unwrap().our_source = Some(source);
        self.conn.flush().map_err(|e| e.to_string())
    }

    /// Serve a paste: the fd arrived with the `send` event, stashed by serial.
    pub fn selection_write(&self, serial: u32, data: &[u8]) -> Result<(), String> {
        let mut file = self
            .shared
            .lock()
            .unwrap()
            .transfers
            .remove(&serial)
            .ok_or("unknown transfer serial")?;
        crate::write_ignoring_epipe(&mut file, data).map_err(|e| e.to_string())
    }
}

/// Connect to the session's Wayland display. User units reliably have
/// `XDG_RUNTIME_DIR` but not always `WAYLAND_DISPLAY` — fall back to the
/// default socket name.
fn connect_display() -> Result<Connection, ConnectError> {
    if let Ok(conn) = Connection::connect_to_env() {
        return Ok(conn);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| ConnectError::Other("XDG_RUNTIME_DIR not set".into()))?;
    let stream = UnixStream::connect(Path::new(&dir).join("wayland-0"))
        .map_err(|e| ConnectError::Other(format!("connecting wayland-0: {e}")))?;
    Connection::from_socket(stream).map_err(|e| ConnectError::Other(e.to_string()))
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_data_control_device_v1::Event as DeviceEvent;
        match event {
            // The offer's mime types accumulate via its own `offer` events; nothing
            // to do until a selection event references it.
            DeviceEvent::DataOffer { .. } => {}
            DeviceEvent::Selection { id } => {
                let (has_text, is_owner) = {
                    let mut sh = state.shared.lock().unwrap();
                    if let Some(prev) = sh.selection.take() {
                        prev.destroy();
                    }
                    let has_text = id
                        .as_ref()
                        .and_then(|o| o.data::<OfferMimes>())
                        .map(|m| m.lock().unwrap().iter().any(|m| m == TEXT_MIME))
                        .unwrap_or(false);
                    sh.selection = id;
                    (has_text, sh.our_source.is_some())
                };
                eprintln!(
                    "limina-agent-session: wayland selection changed (has_text={has_text} is_owner={is_owner})"
                );
                let _ = state.tx.send(Event::OwnerChanged { has_text, is_owner });
            }
            // We don't bridge the primary selection; drop its offers immediately.
            DeviceEvent::PrimarySelection { id: Some(offer) } => offer.destroy(),
            DeviceEvent::PrimarySelection { id: None } => {}
            DeviceEvent::Finished => {
                let _ = state.tx.send(Event::SessionGone);
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferMimes> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        mimes: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            mimes.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use ext_data_control_source_v1::Event as SourceEvent;
        match event {
            // A guest app is pasting from our selection: park the fd, let the
            // bridge serve it from its cache (Event::Transfer → selection_write).
            SourceEvent::Send { mime_type: _, fd } => {
                let serial = {
                    let mut sh = state.shared.lock().unwrap();
                    let serial = sh.next_serial;
                    sh.next_serial += 1;
                    sh.transfers.insert(serial, File::from(fd));
                    serial
                };
                let _ = state.tx.send(Event::Transfer { serial });
            }
            SourceEvent::Cancelled => {
                let mut sh = state.shared.lock().unwrap();
                // Only the LIVE source clears ownership — replacing our own
                // source cancels the old one and must not flip the flag.
                if sh
                    .our_source
                    .as_ref()
                    .is_some_and(|s| s.id() == source.id())
                {
                    sh.our_source = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}
