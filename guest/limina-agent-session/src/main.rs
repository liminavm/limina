// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-agent-session — the per-session guest helper: the guest side of the M5
//! clipboard bridge.
//!
//! Why a separate binary from `limina-agent`: the clipboard is session state — it lives
//! in the user's compositor, which the root limina-agent cannot reach. So this runs as
//! a systemd *user* unit inside the graphical session and holds its own vsock
//! connection to the host control plane (no root needed), advertising the `clipboard`
//! capability.
//!
//! Two backends, probed in tier order (see [`wayland_clip`]):
//! - **Enhanced**: ext-data-control-v1 Wayland client against our patched mutter
//!   (`patches/mutter/`) — focusless selection management with no side effects.
//! - **Stock fallback**: mutter's private RemoteDesktop D-Bus API on the session bus
//!   (the clipboard spike, `spikes/clipboard-remotedesktop/`, proved a background
//!   session-bus client can drive it — and that nothing else on stock mutter 49.5
//!   can). Cosmetic cost: GNOME shows the screen-share indicator while the
//!   RemoteDesktop session exists — which is exactly why the enhanced path exists.
//!
//! Bridge shape (see `crates/limina/src/clipboard.rs` for the host side and the protocol
//! rules — symmetric eager-pull, newest serial wins):
//! - guest copy → mutter emits SelectionOwnerChanged (not session-is-owner — that's our
//!   own echo, the loop-prevention rule) → CLIP_OFFER to the host; its CLIP_REQUEST is
//!   answered by SelectionRead → CLIP_DATA.
//! - host copy → CLIP_OFFER from the host → CLIP_REQUEST → CLIP_DATA → cache the text +
//!   SetSelection; guest apps pasting trigger SelectionTransfer → SelectionWrite(fd) from
//!   the cache → SelectionWriteDone.
//!
//! Spike-earned details encoded here: the RemoteDesktop session must stay resident to
//! service transfers; the transfer fds arrive O_NONBLOCK; EnableClipboard immediately
//! replays the current owner (skip it if it's us).

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::Duration;

use limina_proto::{
    read_message, write_message, ClipData, ClipOffer, ClipRequest, Heartbeat, Hello, Message,
    CHANNEL_CLIPBOARD, CHANNEL_CONTROL, CONTROL_PORT,
};

mod wayland_clip;
use wayland_clip::WaylandClip;

const TEXT_MIME: &str = "text/plain;charset=utf-8";
const OFFER_MIMES: [&str; 2] = [TEXT_MIME, "text/plain"];
/// Idle heartbeat cadence on the vsock channel.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);
/// Backoff between vsock reconnect attempts.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

/// Everything the main loop reacts to, funneled into one mpsc queue.
enum Event {
    /// A frame from the host control plane.
    Host(Message),
    /// The vsock channel died (reconnect).
    HostGone,
    /// The compositor selection changed owner.
    OwnerChanged { has_text: bool, is_owner: bool },
    /// A guest app wants our selection content (serial namespace is the backend's:
    /// mutter's on the D-Bus path, our own on the Wayland path).
    Transfer { serial: u32 },
    /// The session (compositor / D-Bus) is gone: exit, systemd restarts us.
    SessionGone,
}

fn main() {
    let port = std::env::var("LIMINA_AGENT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CONTROL_PORT);
    eprintln!("limina-agent-session {}: starting", version());

    // Pick a clipboard backend (retry: gnome-shell may still be coming up when the
    // user unit starts). Enhanced tier first — ext-data-control on our patched
    // mutter; a compositor without the protocol is stock tier and falls back to the
    // RemoteDesktop D-Bus path. If the session dies later we exit and systemd
    // restarts us into the (new) graphical session.
    let (tx, rx) = mpsc::channel::<Event>();
    let clip = loop {
        // Any Wayland failure falls through to the D-Bus probe: environments with
        // no Wayland display at all (the L1 mock-mutter guest) must still reach
        // the fallback, and in a real session that's still coming up BOTH probes
        // fail and we retry the pair.
        let wl_err = match WaylandClip::connect(tx.clone()) {
            Ok(w) => {
                eprintln!("limina-agent-session: ext-data-control backend up (enhanced tier)");
                break Clip::Wayland(w);
            }
            Err(e) => e,
        };
        match ClipSession::connect() {
            Ok(c) => {
                eprintln!("limina-agent-session: RemoteDesktop backend up (wayland: {wl_err})");
                c.spawn_signal_threads(tx.clone());
                break Clip::RemoteDesktop(c);
            }
            Err(e) => {
                eprintln!(
                    "limina-agent-session: no backend yet (wayland: {wl_err}; remotedesktop: {e}); retrying"
                );
            }
        }
        std::thread::sleep(RECONNECT_EVERY);
    };

    let mut bridge = Bridge {
        clip,
        host: None,
        seq: 0,
        guest_serial: 0,
        host_serial: 0,
        cached_host_text: None,
        logged_waiting: false,
    };

    loop {
        if bridge.host.is_none() {
            bridge.try_connect(port, &tx);
        }
        match rx.recv_timeout(HEARTBEAT_EVERY) {
            Ok(ev) => bridge.handle(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => bridge.heartbeat(),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // All signal threads died — the D-Bus session is gone. Let systemd
                // restart us into a live session.
                eprintln!("limina-agent-session: D-Bus session lost; exiting");
                std::process::exit(0);
            }
        }
    }
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The clipboard backend behind the bridge: same [`Event`] vocabulary either way.
enum Clip {
    /// Enhanced tier: ext-data-control-v1 against our patched mutter.
    Wayland(WaylandClip),
    /// Stock tier: mutter's RemoteDesktop D-Bus API.
    RemoteDesktop(ClipSession),
}

impl Clip {
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>, String> {
        match self {
            Clip::Wayland(w) => w.selection_read(mime),
            Clip::RemoteDesktop(c) => c.selection_read(mime).map_err(|e| e.to_string()),
        }
    }

    fn set_selection(&self) -> Result<(), String> {
        match self {
            Clip::Wayland(w) => w.set_selection(),
            Clip::RemoteDesktop(c) => c.set_selection().map_err(|e| e.to_string()),
        }
    }

    fn selection_write(&self, serial: u32, data: &[u8]) -> Result<(), String> {
        match self {
            Clip::Wayland(w) => w.selection_write(serial, data),
            Clip::RemoteDesktop(c) => c.selection_write(serial, data).map_err(|e| e.to_string()),
        }
    }

    /// Transfer postlude: only the D-Bus path has an explicit done call (on the
    /// Wayland path closing the fd IS the completion).
    fn selection_write_done(&self, serial: u32, success: bool) -> Result<(), String> {
        match self {
            Clip::Wayland(_) => Ok(()),
            Clip::RemoteDesktop(c) => c
                .selection_write_done(serial, success)
                .map_err(|e| e.to_string()),
        }
    }
}

/// The bridge state machine (single-threaded: everything arrives as an [`Event`]).
struct Bridge {
    clip: Clip,
    /// Write half of the live vsock channel.
    host: Option<File>,
    /// Heartbeat sequence.
    seq: u64,
    /// Serial of OUR newest offer to the host (guest→host direction).
    guest_serial: u64,
    /// Newest host offer serial we've requested (host→guest direction).
    host_serial: u64,
    /// Host clipboard content we own the guest selection with, served on transfers.
    cached_host_text: Option<Vec<u8>>,
    logged_waiting: bool,
}

impl Bridge {
    /// One vsock connect + HELLO attempt; spawns the reader thread on success.
    fn try_connect(&mut self, port: u32, tx: &mpsc::Sender<Event>) {
        let Some(mut stream) = vsock_connect(port) else {
            if !self.logged_waiting {
                eprintln!("limina-agent-session: host not reachable yet; retrying quietly");
                self.logged_waiting = true;
            }
            std::thread::sleep(RECONNECT_EVERY);
            return;
        };
        let pagesize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let hello = Message::Hello(Hello {
            agent: format!("limina-agent-session/{}", version()),
            caps: vec!["clipboard".to_string()],
            pagesize,
        });
        if write_message(&mut stream, CHANNEL_CONTROL, &hello).is_err() {
            return;
        }
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            loop {
                match read_message(&mut reader) {
                    Ok((_, msg)) => {
                        if tx.send(Event::Host(msg)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Event::HostGone);
                        return;
                    }
                }
            }
        });
        eprintln!("limina-agent-session: connected to host");
        self.logged_waiting = false;
        self.host = Some(stream);
    }

    fn send(&mut self, channel: u32, msg: &Message) {
        if let Some(stream) = self.host.as_mut() {
            if write_message(stream, channel, msg).is_err() {
                self.host = None;
            }
        }
    }

    fn heartbeat(&mut self) {
        if self.host.is_some() {
            self.seq += 1;
            let beat = Message::Heartbeat(Heartbeat { seq: self.seq });
            self.send(CHANNEL_CONTROL, &beat);
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::HostGone => self.host = None,
            // --- guest→host -----------------------------------------------------
            Event::OwnerChanged { has_text, is_owner } => {
                // Our own SetSelection echoes back with session-is-owner: ignore it,
                // or we'd offer the host its own clipboard (the loop).
                if is_owner || !has_text {
                    return;
                }
                self.guest_serial += 1;
                let offer = Message::ClipOffer(ClipOffer {
                    serial: self.guest_serial,
                    mime_types: OFFER_MIMES.iter().map(|m| m.to_string()).collect(),
                });
                self.send(CHANNEL_CLIPBOARD, &offer);
            }
            Event::Host(Message::ClipRequest(r)) => {
                if r.serial != self.guest_serial {
                    return; // stale: a newer offer is already on its way
                }
                match self.clip.selection_read(&r.mime_type) {
                    // Content a frame can't carry gets an explicit error — a doomed
                    // write_message would error and tear the channel down instead.
                    Ok(data) if data.len() > limina_proto::MAX_CLIP_DATA => {
                        eprintln!(
                            "limina-agent-session: selection is {} bytes (> {} max); TOO_LARGE",
                            data.len(),
                            limina_proto::MAX_CLIP_DATA
                        );
                        let err = Message::Error(limina_proto::ErrorMsg {
                            code: limina_proto::ERR_TOO_LARGE,
                            ref_type: limina_proto::msg_type::CLIP_REQUEST,
                            detail: format!("guest selection is {} bytes", data.len()),
                        });
                        self.send(CHANNEL_CLIPBOARD, &err);
                    }
                    Ok(data) => {
                        let msg = Message::ClipData(ClipData {
                            serial: r.serial,
                            mime_type: r.mime_type,
                            data,
                        });
                        self.send(CHANNEL_CLIPBOARD, &msg);
                    }
                    Err(e) => eprintln!("limina-agent-session: SelectionRead failed: {e}"),
                }
            }
            // --- host→guest -----------------------------------------------------
            Event::Host(Message::ClipOffer(o)) => {
                if !o.mime_types.iter().any(|m| m == TEXT_MIME) {
                    return;
                }
                if o.serial < self.host_serial {
                    return; // stale offer
                }
                self.host_serial = o.serial;
                let req = Message::ClipRequest(ClipRequest {
                    serial: o.serial,
                    mime_type: TEXT_MIME.to_string(),
                });
                self.send(CHANNEL_CLIPBOARD, &req);
            }
            Event::Host(Message::ClipData(d)) => {
                if d.serial != self.host_serial {
                    return; // superseded by a newer host offer
                }
                self.cached_host_text = Some(d.data);
                if let Err(e) = self.clip.set_selection() {
                    eprintln!("limina-agent-session: SetSelection failed: {e}");
                }
            }
            Event::Transfer { serial } => {
                // A guest app is pasting the selection we own: serve from the cache.
                let data = self.cached_host_text.clone().unwrap_or_default();
                let ok = self.clip.selection_write(serial, &data).is_ok();
                if let Err(e) = self.clip.selection_write_done(serial, ok) {
                    eprintln!("limina-agent-session: SelectionWriteDone failed: {e}");
                }
            }
            Event::SessionGone => {
                eprintln!("limina-agent-session: session gone; exiting for restart");
                std::process::exit(0);
            }
            // Control-channel frames we don't act on (SHUTDOWN is the root agent's
            // job; unknown types get the protocol's standard non-fatal reply).
            Event::Host(Message::Unknown { msg_type, .. }) => {
                let reply = Message::unsupported(msg_type);
                self.send(CHANNEL_CONTROL, &reply);
            }
            Event::Host(_) => {}
        }
    }
}

/// One vsock connect attempt to `CID_HOST:port`.
fn vsock_connect(port: u32) -> Option<File> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_HOST;
        let r = libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if r == 0 {
            Some(File::from_raw_fd(fd))
        } else {
            libc::close(fd);
            None
        }
    }
}

// --- the mutter RemoteDesktop client (what rdclip.py prototyped) ----------------------

const RD_BUS: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_IFACE: &str = "org.gnome.Mutter.RemoteDesktop";
const SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";

struct ClipSession {
    session: zbus::blocking::Proxy<'static>,
}

impl ClipSession {
    /// Create + start a RemoteDesktop session and enable the clipboard (monitor mode:
    /// no mime-types — SetSelection comes later, per host offer).
    fn connect() -> zbus::Result<ClipSession> {
        let conn = zbus::blocking::Connection::session()?;
        let rd = zbus::blocking::Proxy::new(&conn, RD_BUS, RD_PATH, RD_IFACE)?;
        let session_path: zbus::zvariant::OwnedObjectPath = rd.call("CreateSession", &())?;
        let session =
            zbus::blocking::Proxy::new(&conn, RD_BUS, session_path.clone(), SESSION_IFACE)?;
        session.call::<_, _, ()>("Start", &())?;
        let no_options: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        session.call::<_, _, ()>("EnableClipboard", &(no_options,))?;
        Ok(ClipSession { session })
    }

    /// Subscribe SelectionOwnerChanged + SelectionTransfer, each pumped into the main
    /// event queue from its own thread (zbus blocking signal streams are iterators).
    fn spawn_signal_threads(&self, tx: mpsc::Sender<Event>) {
        let owner = self
            .session
            .receive_signal("SelectionOwnerChanged")
            .expect("subscribing SelectionOwnerChanged");
        let tx_owner = tx.clone();
        std::thread::spawn(move || {
            for msg in owner {
                type Options = HashMap<String, zbus::zvariant::OwnedValue>;
                let (options,) = match msg.body().deserialize::<(Options,)>() {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("limina-agent-session: bad SelectionOwnerChanged body: {e}");
                        continue;
                    }
                };
                let has_text = options
                    .get("mime-types")
                    .map(value_to_strings)
                    .is_some_and(|m| m.iter().any(|m| m == TEXT_MIME));
                let is_owner = options
                    .get("session-is-owner")
                    .map(value_to_bool)
                    .unwrap_or(false);
                eprintln!(
                    "limina-agent-session: selection owner changed (has_text={has_text} is_owner={is_owner})"
                );
                if tx_owner
                    .send(Event::OwnerChanged { has_text, is_owner })
                    .is_err()
                {
                    return;
                }
            }
        });

        let transfers = self
            .session
            .receive_signal("SelectionTransfer")
            .expect("subscribing SelectionTransfer");
        std::thread::spawn(move || {
            for msg in transfers {
                let Ok((_mime, serial)) = msg.body().deserialize::<(String, u32)>() else {
                    continue;
                };
                if tx.send(Event::Transfer { serial }).is_err() {
                    return;
                }
            }
        });
    }

    /// Read the current guest selection content for `mime` (the spike: the fd arrives
    /// O_NONBLOCK — make it blocking and read to EOF).
    fn selection_read(&self, mime: &str) -> zbus::Result<Vec<u8>> {
        let fd: zbus::zvariant::OwnedFd = self.session.call("SelectionRead", &(mime,))?;
        let mut file = File::from(std::os::fd::OwnedFd::from(fd));
        set_blocking(&file);
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|e| zbus::Error::InputOutput(std::sync::Arc::new(e)))?;
        Ok(data)
    }

    /// Own the guest selection with our text formats (content served on transfers).
    fn set_selection(&self) -> zbus::Result<()> {
        let mimes: Vec<String> = OFFER_MIMES.iter().map(|m| m.to_string()).collect();
        let mut options: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        options.insert("mime-types", mimes.into());
        self.session.call("SetSelection", &(options,))
    }

    /// Answer a SelectionTransfer: get the write fd and push the content through it.
    fn selection_write(&self, serial: u32, data: &[u8]) -> zbus::Result<()> {
        let fd: zbus::zvariant::OwnedFd = self.session.call("SelectionWrite", &(serial,))?;
        let mut file = File::from(std::os::fd::OwnedFd::from(fd));
        set_blocking(&file);
        write_ignoring_epipe(&mut file, data)
            .map_err(|e| zbus::Error::InputOutput(std::sync::Arc::new(e)))
    }

    fn selection_write_done(&self, serial: u32, success: bool) -> zbus::Result<()> {
        self.session.call("SelectionWriteDone", &(serial, success))
    }
}

/// Strip the wrapping layers mutter's GVariant marshalling puts around `a{sv}` values:
/// variant nesting AND a single-field tuple (the same `(['…'],)` shape the Python spike
/// saw — the value of `mime-types` arrives as a Structure holding the array).
fn unwrap_value<'a>(mut val: &'a zbus::zvariant::Value<'a>) -> &'a zbus::zvariant::Value<'a> {
    use zbus::zvariant::Value;
    loop {
        match val {
            Value::Value(inner) => val = inner,
            Value::Structure(s) if s.fields().len() == 1 => val = &s.fields()[0],
            _ => return val,
        }
    }
}

/// Pull a `Vec<String>` out of an `a{sv}` option value (see [`unwrap_value`]).
fn value_to_strings(v: &zbus::zvariant::OwnedValue) -> Vec<String> {
    use zbus::zvariant::Value;
    match unwrap_value(v) {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match unwrap_value(item) {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Pull a bool out of an `a{sv}` option value (see [`unwrap_value`]).
fn value_to_bool(v: &zbus::zvariant::OwnedValue) -> bool {
    matches!(unwrap_value(v), zbus::zvariant::Value::Bool(true))
}

/// Clear O_NONBLOCK (mutter hands out non-blocking pipe ends; see the spike notes).
fn set_blocking(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

/// A pasting app may close its read end early (e.g. it only wanted a prefix) — EPIPE
/// there is a successful transfer, not an error.
fn write_ignoring_epipe(file: &mut File, data: &[u8]) -> std::io::Result<()> {
    match file.write_all(data) {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}
