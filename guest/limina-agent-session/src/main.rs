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
//! Three backends, probed in tier order (see [`wayland_clip`] and [`ext_bridge`]):
//! - **ext-data-control-v1** Wayland client — focusless selection management with no
//!   side effects, on compositors that ship the protocol (KDE, wlroots; GNOME
//!   upstream declines it, mutter#524).
//! - **The `clipboard@limina` shell-extension bridge** (`guest/gnome-shell-extension/`)
//!   — the GNOME tier: the extension scripts Meta.Selection inside the compositor
//!   and we drive it over the session bus. Quiet like ext-data-control, and immune
//!   to distro mutter updates (which is what retired the patched-mutter carry).
//! - **Stock fallback**: mutter's private RemoteDesktop D-Bus API on the session bus
//!   (the clipboard spike, `spikes/clipboard-remotedesktop/`, proved a background
//!   session-bus client can drive it — and that nothing else on stock mutter 49.5
//!   can). Cosmetic cost: GNOME shows the screen-share indicator while the
//!   RemoteDesktop session exists — which is exactly why the quieter tiers exist.
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

mod ext_bridge;
mod wayland_clip;
use ext_bridge::BridgeClip;
use wayland_clip::WaylandClip;

const TEXT_MIME: &str = "text/plain;charset=utf-8";
const OFFER_MIMES: [&str; 2] = [TEXT_MIME, "text/plain"];
/// Idle heartbeat cadence on the vsock channel.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);
/// Backoff between vsock reconnect attempts.
const RECONNECT_EVERY: Duration = Duration::from_secs(2);
/// Backend-probe attempts before the RemoteDesktop fallback is taken even while the
/// extension tier still looks imminent (× RECONNECT_EVERY ≈ 20 s) — the cap on a
/// stuck "imminent" verdict, e.g. an enabled extension that crashes before exporting.
const RD_GRACE_ATTEMPTS: u32 = 10;

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
    // user unit starts). Tier order: ext-data-control → the clipboard@limina
    // extension bridge → mutter's RemoteDesktop D-Bus API. Any failure falls
    // through to the next probe — environments with no Wayland display at all (the
    // L1 mock guest) must still reach the fallbacks, and in a real session that's
    // still coming up ALL probes fail and we retry the set. The RemoteDesktop
    // fallback (the one with the screen-share indicator) is additionally parked
    // while the extension tier is still plausibly coming ([`BridgeOutlook`]), so a
    // booting GNOME session lands on the quiet tier instead of racing to the loud
    // one. If the session dies later we exit and systemd restarts us into the
    // (new) graphical session.
    let (tx, rx) = mpsc::channel::<Event>();
    let mut kick = ExtensionKick::new();
    let mut attempts: u32 = 0;
    let clip = loop {
        attempts += 1;
        let wl_err = match WaylandClip::connect(tx.clone()) {
            Ok(w) => {
                eprintln!("limina-agent-session: ext-data-control backend up (enhanced tier)");
                break Clip::Wayland(w);
            }
            Err(e) => e,
        };
        let br_err = match BridgeClip::connect(tx.clone()) {
            Ok(b) => {
                eprintln!("limina-agent-session: extension-bridge backend up (wayland: {wl_err})");
                break Clip::Bridge(b);
            }
            Err(e) => e,
        };
        if attempts > RD_GRACE_ATTEMPTS || kick.outlook() == BridgeOutlook::Never {
            match ClipSession::connect() {
                Ok(c) => {
                    eprintln!(
                        "limina-agent-session: RemoteDesktop backend up (wayland: {wl_err}; bridge: {br_err})"
                    );
                    c.spawn_signal_threads(tx.clone());
                    break Clip::RemoteDesktop(c);
                }
                Err(e) => {
                    eprintln!(
                        "limina-agent-session: no backend yet (wayland: {wl_err}; bridge: {br_err}; remotedesktop: {e}); retrying"
                    );
                }
            }
        } else {
            eprintln!(
                "limina-agent-session: extension bridge expected; parking the RemoteDesktop fallback (wayland: {wl_err}; bridge: {br_err})"
            );
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

/// The clipboard backend behind the bridge: same [`Event`] vocabulary each way.
enum Clip {
    /// ext-data-control-v1, on compositors that ship the protocol.
    Wayland(WaylandClip),
    /// The clipboard@limina shell-extension bridge (stock GNOME, quiet).
    Bridge(BridgeClip),
    /// Last resort: mutter's RemoteDesktop D-Bus API (screen-share indicator).
    RemoteDesktop(ClipSession),
}

impl Clip {
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>, String> {
        match self {
            Clip::Wayland(w) => w.selection_read(mime),
            Clip::Bridge(b) => b.selection_read(mime),
            Clip::RemoteDesktop(c) => c.selection_read(mime).map_err(|e| e.to_string()),
        }
    }

    /// Own the guest selection with host content. `data` is the bridge's cache: the
    /// Wayland/RemoteDesktop backends ignore it (they announce formats and serve the
    /// content later, per transfer); the extension bridge sends it up front (the
    /// compositor serves every paste in-process, so no transfers come back).
    fn set_selection(&self, data: &[u8]) -> Result<(), String> {
        match self {
            Clip::Wayland(w) => w.set_selection(),
            Clip::Bridge(b) => b.set_selection(data),
            Clip::RemoteDesktop(c) => c.set_selection().map_err(|e| e.to_string()),
        }
    }

    fn selection_write(&self, serial: u32, data: &[u8]) -> Result<(), String> {
        match self {
            Clip::Wayland(w) => w.selection_write(serial, data),
            Clip::Bridge(_) => Err("extension bridge serves pastes in-shell (no transfers)".into()),
            Clip::RemoteDesktop(c) => c.selection_write(serial, data).map_err(|e| e.to_string()),
        }
    }

    /// Transfer postlude: only the RemoteDesktop path has an explicit done call (on
    /// the Wayland path closing the fd IS the completion; the extension bridge has
    /// no transfers at all).
    fn selection_write_done(&self, serial: u32, success: bool) -> Result<(), String> {
        match self {
            Clip::Wayland(_) | Clip::Bridge(_) => Ok(()),
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
                let data = self.cached_host_text.as_deref().unwrap_or_default();
                if let Err(e) = self.clip.set_selection(data) {
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

// --- the extension-tier outlook (drives the RemoteDesktop fallback timing) ------------

const EXTENSION_UUID: &str = "clipboard@limina";

/// gnome-shell extension state numbers (js ExtensionState; `GetExtensionInfo.state`).
const EXT_STATE_ENABLED: u32 = 1;
const EXT_STATE_ENABLING: u32 = 8;

/// Whether the probe loop may still hope for the extension bridge this session.
#[derive(PartialEq, Clone, Copy)]
enum BridgeOutlook {
    /// gnome-shell is up and the extension is enabled/enabling (or we just enabled
    /// it): the bridge export is moments away — keep RemoteDesktop parked.
    Imminent,
    /// No gnome-shell, no extension installed, or the user disabled it: fall back.
    Never,
}

/// The extension availability oracle + its one-time self-enable.
///
/// The enhanced installer drops the extension under /usr/share/gnome-shell/extensions
/// but cannot enable it: `enabled-extensions` is per-user dconf state and no session
/// exists at install time. So the first session probe enables it from here, ONCE per
/// user (stamp file) — a later disable by the user is their call and is never
/// overridden; those sessions ride the RemoteDesktop tier instead.
struct ExtensionKick {
    /// EnableExtension was tried this run (never hammer the shell from the loop).
    attempted: bool,
}

impl ExtensionKick {
    fn new() -> ExtensionKick {
        ExtensionKick { attempted: false }
    }

    fn outlook(&mut self) -> BridgeOutlook {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return BridgeOutlook::Never; // no session bus: nothing extension-shaped coming
        };
        if !ext_bridge::name_has_owner(&conn, "org.gnome.Shell").unwrap_or(false) {
            return BridgeOutlook::Never; // not GNOME (or the shell is gone)
        }
        match extension_state(&conn) {
            // Enabled or mid-enable: the export is imminent. This also covers a
            // still-starting shell that hasn't loaded extensions yet.
            Some(EXT_STATE_ENABLED | EXT_STATE_ENABLING) => BridgeOutlook::Imminent,
            // Installed but not enabled (fresh install / user-disabled): see below.
            Some(_) => self.enable_once(&conn),
            // Not installed (basic tier without the payload): don't stall clipboard.
            None => BridgeOutlook::Never,
        }
    }

    fn enable_once(&mut self, conn: &zbus::blocking::Connection) -> BridgeOutlook {
        if self.attempted {
            return BridgeOutlook::Never; // this run's verdict is already in
        }
        let Some(stamp) = enable_stamp_path() else {
            return BridgeOutlook::Never;
        };
        if stamp.exists() {
            return BridgeOutlook::Never; // enabled once before: the user turned it off
        }
        self.attempted = true;
        match enable_extension(conn) {
            Ok(true) => {
                if let Some(dir) = stamp.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&stamp, b"");
                eprintln!("limina-agent-session: enabled the {EXTENSION_UUID} shell extension");
                BridgeOutlook::Imminent
            }
            Ok(false) => BridgeOutlook::Never,
            Err(e) => {
                // The shell may still be booting its Extensions service; retry on
                // the next probe pass (bounded by RD_GRACE_ATTEMPTS regardless).
                eprintln!("limina-agent-session: EnableExtension failed: {e}");
                self.attempted = false;
                BridgeOutlook::Imminent
            }
        }
    }
}

/// The shell's state number for our extension (`GetExtensionInfo`); None when the
/// extension isn't installed (empty dict) or the query itself fails.
fn extension_state(conn: &zbus::blocking::Connection) -> Option<u32> {
    let p = shell_extensions_proxy(conn).ok()?;
    let info: HashMap<String, zbus::zvariant::OwnedValue> =
        p.call("GetExtensionInfo", &(EXTENSION_UUID,)).ok()?;
    // The shell packs `state` as a DOUBLE (it's a GJS number).
    match unwrap_value(info.get("state")?) {
        zbus::zvariant::Value::F64(v) => Some(*v as u32),
        zbus::zvariant::Value::U32(v) => Some(*v),
        _ => None,
    }
}

fn enable_extension(conn: &zbus::blocking::Connection) -> Result<bool, String> {
    let p = shell_extensions_proxy(conn).map_err(|e| e.to_string())?;
    p.call::<_, _, bool>("EnableExtension", &(EXTENSION_UUID,))
        .map_err(|e| e.to_string())
}

fn shell_extensions_proxy(
    conn: &zbus::blocking::Connection,
) -> zbus::Result<zbus::blocking::Proxy<'static>> {
    zbus::blocking::Proxy::new(
        conn,
        "org.gnome.Shell.Extensions",
        "/org/gnome/Shell/Extensions",
        "org.gnome.Shell.Extensions",
    )
}

/// `$XDG_STATE_HOME/limina/clipboard-extension-enabled` (~/.local/state fallback).
fn enable_stamp_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(s) if !s.is_empty() => std::path::PathBuf::from(s),
        _ => std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".local/state"),
    };
    Some(base.join("limina/clipboard-extension-enabled"))
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
