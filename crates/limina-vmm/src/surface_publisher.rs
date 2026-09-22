// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Hands the renderer's surfaces to the supervisor by Mach port.
//!
//! The venus and vrend scanouts are minted inside virglrs, which knows how to make a surface
//! reachable but not who should reach it. Without a publisher it mints them global, and any
//! process running as the user can then read the guest's screen by guessing ids. This installs
//! one that sends every surface over the same Mach channel the software-2D ring uses
//! (`limina-surfaceport`), so the surfaces stay private to the supervisor.
//!
//! Publishes and releases share one port on purpose: one port is one FIFO queue, and that is
//! what keeps a release for a recycled id from dropping the surface that inherited it (see the
//! `limina-surfaceport` module docs).

use std::sync::Arc;

use limina_surfaceport::SurfacePortSender;
use virglrenderer::metal::{Publisher, SurfacePort, set_publisher};

struct ToSupervisor {
    sender: SurfacePortSender,
    /// `LIMINA_GLOBAL_SCANOUT`: also mark the surfaces global, so the cross-process pixel
    /// oracle can find them by id. The handover happens either way.
    also_global: bool,
}

impl Publisher for ToSupervisor {
    fn publish(&self, id: u32, port: SurfacePort) {
        // Keyed by the surface's own id, which is what the supervisor's store resolves presents by.
        if let Err(e) = self.sender.send_port(id, port.into_raw()) {
            log::warn!("surface {id}: could not hand it to the supervisor ({e}); it will not show");
        }
    }

    fn release(&self, id: u32) {
        if let Err(e) = self.sender.release(id) {
            log::warn!("surface {id}: could not tell the supervisor it is gone ({e})");
        }
    }

    fn also_global(&self) -> bool {
        self.also_global
    }
}

/// Install the publisher for the supervisor's receiver `name`. If the receiver cannot be found,
/// nothing is installed and the renderer's surfaces stay global, since then an id is the only
/// way the supervisor can reach them at all.
pub fn install(name: &str) {
    match SurfacePortSender::lookup(name) {
        Ok(sender) => set_publisher(Some(Arc::new(ToSupervisor {
            sender,
            also_global: std::env::var_os("LIMINA_GLOBAL_SCANOUT").is_some(),
        }))),
        Err(e) => log::warn!(
            "no surface receiver at {name} ({e}): the renderer's surfaces stay global, readable by \
             any process that guesses their ids"
        ),
    }
}
