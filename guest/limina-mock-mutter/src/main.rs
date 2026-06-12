// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-mock-mutter — a scripted stand-in for mutter's RemoteDesktop clipboard API.
//!
//! TEST-ONLY. Claims `org.gnome.Mutter.RemoteDesktop` on the guest session bus so the
//! REAL `limina-agent-session` can run (and be integration-tested) in the L1 guest, where
//! no GNOME exists. Implements exactly the clipboard subset the helper uses:
//! CreateSession / Start / EnableClipboard / SetSelection / SelectionRead /
//! SelectionWrite / SelectionWriteDone + the SelectionOwnerChanged and SelectionTransfer
//! signals.
//!
//! Scripting/observation happens through files in `/` — the virtio-fs rootfs IS a host
//! directory, so the harness reads and writes them directly (`LIMINA_MOCK_ID` names them,
//! keeping concurrent tests apart):
//! - `/mock-<id>.copy` (host-written): new content simulates a guest app COPY — the mock
//!   emits SelectionOwnerChanged and serves the content via SelectionRead.
//! - `/mock-<id>.log` (mock-written): one line per observed event. The interesting ones:
//!   `SET_SELECTION <mimes>` (helper owned the selection), `PASTED <content>` (the mock
//!   played a pasting app: SelectionTransfer → the helper's SelectionWrite fd content),
//!   `WRITE_DONE <serial> <ok>`.
//!
//! On SetSelection the mock immediately plays a pasting app (emits SelectionTransfer),
//! so a host→guest sync is observable as `PASTED <text>` with no extra choreography.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, OwnedObjectPath, Value};
use zbus::{fdo, interface};

const TEXT_MIME: &str = "text/plain;charset=utf-8";
const SESSION_PATH: &str = "/org/gnome/Mutter/RemoteDesktop/Session/u1";

fn mock_id() -> String {
    std::env::var("LIMINA_MOCK_ID").unwrap_or_else(|_| "0".into())
}

fn log_line(line: &str) {
    let path = format!("/mock-{}.log", mock_id());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
        let _ = f.sync_all(); // visible to the host reader NOW, not at page-cache leisure
    }
    eprintln!("mock-mutter: {line}");
}

/// Content served by SelectionRead — the last host-written `.copy` trigger.
type Shared = Arc<Mutex<Vec<u8>>>;

struct RemoteDesktop {
    read_content: Shared,
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let path = OwnedObjectPath::try_from(SESSION_PATH).expect("static path");
        let session = Session {
            read_content: self.read_content.clone(),
            transfer_serial: AtomicU32::new(1),
        };
        server
            .at(&path, session)
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        log_line("CREATE_SESSION");
        Ok(path)
    }
}

struct Session {
    read_content: Shared,
    transfer_serial: AtomicU32,
}

#[interface(name = "org.gnome.Mutter.RemoteDesktop.Session")]
impl Session {
    fn start(&self) {
        log_line("START");
    }

    fn stop(&self) {
        log_line("STOP");
    }

    fn enable_clipboard(&self, _options: HashMap<String, Value<'_>>) {
        log_line("ENABLE_CLIPBOARD");
    }

    fn disable_clipboard(&self) {
        log_line("DISABLE_CLIPBOARD");
    }

    /// The helper owns the selection (host content arrived). Record the formats, then
    /// immediately play a pasting guest app: SelectionTransfer for the text.
    async fn set_selection(
        &self,
        options: HashMap<String, Value<'_>>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let mimes = options
            .get("mime-types")
            .map(value_to_strings)
            .unwrap_or_default();
        log_line(&format!("SET_SELECTION {}", mimes.join(",")));
        if mimes.iter().any(|m| m == TEXT_MIME) {
            let serial = self.transfer_serial.fetch_add(1, Ordering::Relaxed);
            let _ = Self::selection_transfer(&emitter, TEXT_MIME, serial).await;
        }
    }

    /// Serve the scripted "guest app copy" content.
    fn selection_read(&self, mime_type: String) -> fdo::Result<OwnedFd> {
        let content = self.read_content.lock().unwrap().clone();
        log_line(&format!(
            "SELECTION_READ {mime_type} ({} bytes)",
            content.len()
        ));
        let (read_end, mut write_end) = pipe()?;
        // Small content (well under the 64 KiB pipe buffer): write + close, no thread.
        write_end
            .write_all(&content)
            .map_err(|e| fdo::Error::Failed(format!("pipe write: {e}")))?;
        drop(write_end);
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(read_end)))
    }

    /// The pasting-app side of a transfer: hand out the write end, log what arrives.
    fn selection_write(&self, serial: u32) -> fdo::Result<OwnedFd> {
        let (mut read_end, write_end) = pipe()?;
        std::thread::spawn(move || {
            let mut content = Vec::new();
            let _ = read_end.read_to_end(&mut content);
            log_line(&format!("PASTED {}", String::from_utf8_lossy(&content)));
            let _ = serial;
        });
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(write_end)))
    }

    fn selection_write_done(&self, serial: u32, success: bool) {
        log_line(&format!("WRITE_DONE {serial} {success}"));
    }

    #[zbus(signal)]
    async fn selection_owner_changed(
        emitter: &SignalEmitter<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn selection_transfer(
        emitter: &SignalEmitter<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;
}

fn pipe() -> fdo::Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(fdo::Error::Failed("pipe() failed".into()));
    }
    unsafe {
        Ok((
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        ))
    }
}

fn value_to_strings(v: &Value<'_>) -> Vec<String> {
    match v {
        Value::Value(inner) => value_to_strings(inner),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match item {
                Value::Str(s) => Some(s.to_string()),
                Value::Value(inner) => match &**inner {
                    Value::Str(s) => Some(s.to_string()),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn main() {
    let id = mock_id();
    let read_content: Shared = Arc::new(Mutex::new(Vec::new()));
    eprintln!("limina-mock-mutter: starting (id {id})");

    zbus::block_on(async {
        // The session bus may still be coming up (init spawns dbus-daemon just before us).
        let conn = loop {
            match zbus::connection::Builder::session().and_then(|b| {
                b.name("org.gnome.Mutter.RemoteDesktop")?.serve_at(
                    "/org/gnome/Mutter/RemoteDesktop",
                    RemoteDesktop {
                        read_content: read_content.clone(),
                    },
                )
            }) {
                Ok(builder) => match builder.build().await {
                    Ok(conn) => break conn,
                    Err(e) => eprintln!("limina-mock-mutter: bus not ready ({e}); retrying"),
                },
                Err(e) => eprintln!("limina-mock-mutter: builder failed ({e}); retrying"),
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        log_line("CLAIMED_NAME");

        // Watch the host-written trigger file; new content = a scripted guest app copy.
        let trigger = format!("/mock-{id}.copy");
        let mut last: Vec<u8> = Vec::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let Ok(content) = std::fs::read(&trigger) else {
                continue;
            };
            if content.is_empty() || content == last {
                continue;
            }
            last = content.clone();
            *read_content.lock().unwrap() = content;
            let server = conn.object_server();
            let Ok(iface) = server.interface::<_, Session>(SESSION_PATH).await else {
                continue; // no session yet — the helper hasn't connected
            };
            let mimes = vec![TEXT_MIME, "text/plain"];
            let mut options: HashMap<&str, Value> = HashMap::new();
            options.insert("mime-types", mimes.into());
            options.insert("session-is-owner", false.into());
            match Session::selection_owner_changed(iface.signal_emitter(), options).await {
                Ok(()) => log_line("OWNER_CHANGED_EMITTED"),
                Err(e) => eprintln!("limina-mock-mutter: emit failed: {e}"),
            }
        }
    });
}
