// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The frames of a run with no window (`--headless-capture`), for the frame capture and the still.
//!
//! The worker is launched exactly as a windowed one is (`--display-window`, a control socketpair,
//! the surface port), so it hands every scanout surface over by Mach port and names every guest
//! flip on a `frame` line. Nothing here touches AppKit: one reader thread per worker applies the
//! control lines to the same [`Shared`] state a window reads ([`present::deliver_line`]) and, for
//! each `frame`, resolves the surface and hands it to [`super::frame_capture::on_present`] and
//! [`super::still::note`] — every flip the supervisor receives, none coalesced, so a headless
//! capture has no `not_presented` runs unless a surface could not be resolved.
//!
//! **A display's still is its live worker's.** A relaunch (a guest reboot, `limina reset`), the
//! worker's exit and a `scanoutgone` drop the still notes they make stale, so a still taken before
//! the fresh guest flips is refused instead of handed a dead worker's frame.
//!
//! **What "presented" means here.** The frame the guest flipped, as the supervisor received it.
//! Nothing composites it, so the records carry no `layer_iosurface` and say `headless`.
//!
//! **The shown-ack is still owed.** With a control channel the worker holds a guest whose flush
//! carries a fence until the supervisor says `shown <id>` (libkrun's `fence_present_policy`), and
//! falls back after 150 ms per frame otherwise. So every flip is acked, from a thread of its own:
//! after the surface it replaced on its slot has left use — which is how a capture blit still
//! reading that surface keeps the guest off it, the same gate a window's acks pass — bounded by
//! [`ACK_GATE`].
//!
//! **Off by default.** Without `--headless-capture` none of this exists: the worker publishes no
//! surfaces and the supervisor receives no frames (`frame_capture::Source::None`).

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;

use super::frame_capture::{self, FrameTag, Source};
use super::present::{self, AckMsg, MAX_SCANOUTS, SendSurface, Shared, SurfaceMap};

/// The longest an ack waits for the replaced surface to leave use: a window's default cap.
const ACK_GATE: Duration = Duration::from_millis(20);

/// The still table's key for `slot` (a window's is its `NSWindow` address; 0 means "none").
fn still_key(slot: usize) -> usize {
    slot + 1
}

/// A headless run's display: the surface-port receiver and the present state, kept across worker
/// relaunches, and the size the worker is told.
pub struct HeadlessDisplay {
    /// Whether a worker has been attached yet: the first one reads at the state's starting epoch
    /// (0, as a window's first worker does), and only a relaunch swaps.
    attached: std::sync::atomic::AtomicBool,
    surface_port_name: Option<String>,
    map: SurfaceMap,
    shared: Arc<Mutex<Shared>>,
    width: u32,
    height: u32,
}

/// One worker's ends of the control channel, between [`HeadlessDisplay::prepare`] and
/// [`HeadlessDisplay::attach`].
pub struct HeadlessSpawn {
    /// The worker's end: inherit it, then drop it once the worker is spawned.
    pub worker_fd: OwnedFd,
    sup: OwnedFd,
}

impl HeadlessDisplay {
    /// Register the surface port and make this run's frames a capture source. Without the port
    /// the worker makes its surfaces global and they are resolved by lookup instead.
    pub fn new(width: u32, height: u32) -> HeadlessDisplay {
        let (surface_port_name, map) = match present::surface_rendezvous() {
            Ok((name, map)) => (Some(name), map),
            Err(e) => {
                log::warn!(
                    "headless capture: no surface port ({e}); the worker's surfaces will be \
                     global"
                );
                (None, present::empty_surface_map())
            }
        };
        frame_capture::set_source(Source::Headless);
        super::still::set_primary(still_key(0));
        HeadlessDisplay {
            attached: std::sync::atomic::AtomicBool::new(false),
            surface_port_name,
            map,
            shared: Shared::new(),
            width,
            height,
        }
    }

    /// A control channel for the next worker, and the worker arguments naming it.
    pub fn prepare(&self) -> anyhow::Result<(HeadlessSpawn, Vec<String>)> {
        let (sup, worker_fd) = crate::supervisor::socketpair(libc::SOCK_STREAM)?;
        let mut args = vec![
            "--display-window".to_string(),
            "--control-fd".to_string(),
            worker_fd.as_raw_fd().to_string(),
            "--display-size".to_string(),
            format!("{}x{}", self.width, self.height),
        ];
        if let Some(name) = &self.surface_port_name {
            args.push("--surface-port-name".into());
            args.push(name.clone());
        }
        Ok((HeadlessSpawn { worker_fd, sup }, args))
    }

    /// The worker `spawn` was prepared for is running: read its frames.
    pub fn attach(&self, spawn: HeadlessSpawn) -> anyhow::Result<()> {
        let HeadlessSpawn { worker_fd, sup } = spawn;
        drop(worker_fd);
        let ack = sup.try_clone()?;
        // Nothing the previous worker put up is the new guest's: a still before its first flip
        // is refused rather than handed the dead worker's frame.
        forget_stills();
        if self
            .attached
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            present::mark_headless_worker_swapped(&self.shared);
        }
        let epoch = self.shared.lock().unwrap().reader_epoch;
        let (tx, rx) = sync_channel::<AckMsg>(64);
        std::thread::Builder::new()
            .name("headless-ack".into())
            .spawn(move || send_acks(ack, rx))?;
        let (shared, map) = (self.shared.clone(), self.map.clone());
        std::thread::Builder::new()
            .name("headless-frames".into())
            .spawn(move || read_frames(sup, shared, map, epoch, tx))?;
        // Once per run, with the first worker: a capture asked for in the environment.
        static FROM_ENV: std::sync::Once = std::sync::Once::new();
        FROM_ENV.call_once(frame_capture::start_from_env);
        Ok(())
    }
}

/// Apply one worker's control lines, and hand every guest flip to the capture and the still.
fn read_frames(
    sup: OwnedFd,
    shared: Arc<Mutex<Shared>>,
    map: SurfaceMap,
    epoch: u64,
    acks: SyncSender<AckMsg>,
) {
    // Per slot, the surface the last flip showed: what the next one's ack waits on.
    let mut last: Vec<Option<CFRetained<IOSurfaceRef>>> = vec![None; MAX_SCANOUTS];
    for line in BufReader::new(File::from(sup)).lines() {
        let Ok(line) = line else { break };
        if present::deliver_line(&shared, epoch, &map, &line).is_none() {
            log::info!("headless capture: a swapped-out worker's reader stops");
            return;
        }
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("frame") => {}
            // The slot has no scanout: what it last showed is no longer the guest's display.
            Some("scanoutgone") => {
                if let Some(slot) = present::parse_slot(parts.next()) {
                    super::still::forget(still_key(slot));
                    last[slot] = None;
                }
                continue;
            }
            _ => continue,
        }
        let Some(id) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(slot) = present::parse_slot(parts.next()) else {
            continue;
        };
        let (frames, resource) = {
            let s = shared.lock().unwrap();
            (s.slots[slot].frames, s.slots[slot].resource)
        };
        let Some(surface) = resolve(&map, id, &acks) else {
            let _ = acks.try_send(AckMsg::Shown(id, None));
            continue;
        };
        let tag = FrameTag::new(slot, frames, epoch).with_resource(resource);
        frame_capture::on_present(tag, id, &surface);
        super::still::note(still_key(slot), tag, id, &surface);
        let prev = last[slot]
            .replace(surface.clone())
            .filter(|p| !std::ptr::eq::<IOSurfaceRef>(&**p, &*surface));
        let _ = acks.try_send(AckMsg::Shown(id, prev.map(SendSurface::new)));
    }
    log::info!("headless capture: control channel closed (worker gone)");
    // The worker is gone; unless a fresh one has already taken over, so are its frames.
    if shared.lock().unwrap().reader_epoch == epoch {
        forget_stills();
    }
}

/// Drop every slot's still note.
fn forget_stills() {
    for slot in 0..MAX_SCANOUTS {
        super::still::forget(still_key(slot));
    }
}

/// The surface the worker named, from the surface port (or, with no port, by global lookup). A
/// surface we no longer hold is asked for again, as a window asks.
fn resolve(
    map: &SurfaceMap,
    id: u32,
    acks: &SyncSender<AckMsg>,
) -> Option<CFRetained<IOSurfaceRef>> {
    let held = {
        let mut m = map.lock().unwrap();
        m.pin_presented(id);
        m.get(id)
    };
    #[allow(deprecated)]
    let found = held.or_else(|| objc2_io_surface::IOSurfaceLookup(id));
    if found.is_none() {
        let (why, ask) = {
            let mut m = map.lock().unwrap();
            (m.why_gone(id), m.request_resurface(id))
        };
        if ask {
            log::warn!(
                "headless capture: surface {id} unresolved ({why:?}); asking the worker for it"
            );
            let _ = acks.try_send(AckMsg::Resurface(id));
        }
    }
    found
}

/// Send the acks and re-publish requests to the worker, in order.
fn send_acks(ack: OwnedFd, rx: Receiver<AckMsg>) {
    let mut out = File::from(ack);
    let fd: RawFd = out.as_raw_fd();
    while let Ok(msg) = rx.recv() {
        let line = match msg {
            AckMsg::Resurface(id) => format!("resurface {id}\n"),
            AckMsg::Shown(id, prev) => {
                if let Some(prev) = prev {
                    let t0 = Instant::now();
                    while prev.is_in_use() && t0.elapsed() < ACK_GATE {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
                format!("shown {id}\n")
            }
        };
        if let Err(e) = out.write_all(line.as_bytes()) {
            log::info!("headless capture: the worker stopped reading acks on fd {fd}: {e}");
            return;
        }
    }
}
