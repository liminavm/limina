// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The GNOME-tier clipboard backend: a session-bus client of the `clipboard@limina`
//! gnome-shell extension (`guest/gnome-shell-extension/`).
//!
//! The extension runs inside the compositor, where the selection is directly
//! scriptable — so this tier needs neither a patched mutter (the retired
//! ext-data-control carry) nor a resident RemoteDesktop session (the stock fallback
//! whose cosmetic cost is GNOME's permanent "screen is shared" indicator). It also
//! survives distro mutter updates, which is what demoted the dogfood guest to the
//! indicator tier on 2026-07-11.
//!
//! Simpler contract than the other two backends: `Set` carries the full content and
//! the extension parks it in a `Meta.SelectionSourceMemory`, so the compositor itself
//! serves every guest paste — there is no transfer choreography and no
//! [`Event::Transfer`] on this path. Loop prevention is the extension's: the
//! owner-changed echo of our own `Set` arrives with `is_owner: true`, which the
//! bridge ignores.

use std::sync::mpsc;

use crate::Event;

const BRIDGE_BUS: &str = "org.limina.Clipboard";
const BRIDGE_PATH: &str = "/org/limina/Clipboard";
const BRIDGE_IFACE: &str = "org.limina.Clipboard";

pub struct BridgeClip {
    proxy: zbus::blocking::Proxy<'static>,
}

impl BridgeClip {
    /// Connect to the extension's bridge object, if the extension is live on the
    /// session bus. Spawns the signal-pump threads on success.
    pub fn connect(tx: mpsc::Sender<Event>) -> Result<BridgeClip, String> {
        let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
        // A Proxy is lazy — probe for a live owner first so the caller can fall
        // through to the next backend instead of erroring on the first call.
        if !name_has_owner(&conn, BRIDGE_BUS).map_err(|e| e.to_string())? {
            return Err("extension bridge not on the bus".into());
        }
        let proxy = zbus::blocking::Proxy::new(&conn, BRIDGE_BUS, BRIDGE_PATH, BRIDGE_IFACE)
            .map_err(|e| e.to_string())?;

        let owner = proxy
            .receive_signal("OwnerChanged")
            .map_err(|e| e.to_string())?;
        let tx_owner = tx.clone();
        std::thread::spawn(move || {
            for msg in owner {
                let (has_text, is_owner) = match msg.body().deserialize::<(bool, bool)>() {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("limina-agent-session: bad OwnerChanged body: {e}");
                        continue;
                    }
                };
                eprintln!(
                    "limina-agent-session: bridge selection changed (has_text={has_text} is_owner={is_owner})"
                );
                if tx_owner
                    .send(Event::OwnerChanged { has_text, is_owner })
                    .is_err()
                {
                    return;
                }
            }
            // The signal stream only ends with the bus connection: session gone.
            let _ = tx_owner.send(Event::SessionGone);
        });

        // Watch for the bridge name dropping (shell restart, or the user disabled the
        // extension): exit so systemd restarts us into a fresh backend probe.
        let dbus = zbus::blocking::Proxy::new(
            &conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .map_err(|e| e.to_string())?;
        let name_changes = dbus
            .receive_signal("NameOwnerChanged")
            .map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            for msg in name_changes {
                let Ok((name, _old, new_owner)) =
                    msg.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                if name == BRIDGE_BUS && new_owner.is_empty() {
                    eprintln!("limina-agent-session: extension bridge left the bus");
                    let _ = tx.send(Event::SessionGone);
                    return;
                }
            }
            let _ = tx.send(Event::SessionGone);
        });

        Ok(BridgeClip { proxy })
    }

    /// Read the current guest selection content for `mime` (plain method call — the
    /// content rides the reply, no fd dance).
    pub fn selection_read(&self, mime: &str) -> Result<Vec<u8>, String> {
        self.proxy
            .call::<_, _, Vec<u8>>("Read", &(mime,))
            .map_err(|e| e.to_string())
    }

    /// Own the guest selection with host content. The extension serves every
    /// subsequent paste in-process — no transfers come back to us.
    pub fn set_selection(&self, data: &[u8]) -> Result<(), String> {
        self.proxy
            .call::<_, _, ()>("Set", &(data,))
            .map_err(|e| e.to_string())
    }
}

/// `NameHasOwner` on the session bus (plain call — keeps us off version-sensitive
/// fdo iterator APIs, matching the file's raw-proxy style).
pub fn name_has_owner(conn: &zbus::blocking::Connection, name: &str) -> zbus::Result<bool> {
    let dbus = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    dbus.call::<_, _, bool>("NameHasOwner", &(name,))
}
