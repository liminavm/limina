// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Report the guest compositor's monitor arrangement to the host.
//!
//! ## Why the host needs to be told
//!
//! The guest has one absolute pointing device and its compositor spreads that device across the
//! whole desktop, so mapping a point in a host window to a position on it requires knowing where
//! each monitor sits. The host cannot infer that: the arrangement is the compositor's, either its
//! own default or whatever the user set and it saved. Measured 2026-08-18 on the two-panel rig —
//! a guest rearranged to match the host ended up exactly transposed from the host's slot-order
//! assumption, so the pointer drove the wrong monitor and its cursor was drawn on a display it
//! was not on.
//!
//! ## The rectangles are LOGICAL, and only the compositor knows them
//!
//! The compositor transforms the absolute device against its monitors' *logical* extents (mode
//! divided by the scale it chose — measured, `spikes/pointer-units-oracle/RESULTS.md`), so
//! logical rectangles are what the host must be given. Core `wl_output` cannot state them: it
//! carries the pixel mode and an **integer** scale, and under a fractional scale the compositor
//! rounds the integer up — mutter at 1.25 advertises scale 2, so mode / scale says 1280 where
//! the compositor runs 2048. That 1.6x disagreement was the measured 20%-unreachable band.
//!
//! `zxdg_output_v1` exists for exactly this gap: the compositor reports `logical_position` and
//! `logical_size` itself, fractional transforms already applied. We require version 3, where its
//! events are committed by the same `wl_output.done` we already wait on. Where the manager is
//! missing (or older than 3), mode / integer-scale is the floor we fall back to — correct at
//! whole-number scales, and no worse than the pre-xdg behavior under fractional ones.
//!
//! ## Why `wl_output` stays bound
//!
//! `wl_output` is core Wayland and still carries the DRM connector name (version 4's `name`,
//! which is what ties a monitor back to a scanout — `zxdg_output_v1.name` is deprecated in favor
//! of it). Asking mutter over its `DisplayConfig` D-Bus interface would have been easier and
//! would have made this GNOME-only; `wl_output` + `zxdg_output_v1` work on mutter, KWin,
//! wlroots, and synoik alike.
//!
//! Version 4 is requested because of `wl_output.name`. Without it a monitor cannot be tied to a
//! connector at all, and guessing by index is the class of assumption this module exists to
//! remove — so on an older compositor we report nothing and the host stays on its own default.

use std::collections::HashMap;

use limina_proto::{DisplayLayout, GuestMonitor};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

/// What one `wl_output` has told us so far. The compositor sends the properties as separate
/// events and commits them with `done`, so a monitor is only reportable once it is complete.
#[derive(Default, Clone)]
struct Output {
    /// The bound proxy, kept so departure can `release` it instead of leaking the id.
    wl: Option<wl_output::WlOutput>,
    /// The companion `zxdg_output_v1`, when the compositor has the manager.
    xdg: Option<ZxdgOutputV1>,
    name: Option<String>,
    x: i32,
    y: i32,
    /// Mode size in the output's own pixels.
    width: u32,
    height: u32,
    /// Integer scale (`wl_output.scale`). Only the fallback divisor — see the module doc.
    scale: i32,
    /// The compositor's own statement of the logical rect (`zxdg_output_v1`). Preferred over
    /// everything above whenever present.
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(u32, u32)>,
    /// Set by `done`: everything above has been delivered at least once.
    ///
    /// The xdg events ride the same `done` (that is what requiring manager version 3 buys),
    /// but on the very first burst the compositor may commit the core properties before the
    /// `get_xdg_output` round-trips — one report built from the fallback division, corrected
    /// on the next `done`. Tolerated rather than gated on: a transiently absent monitor is
    /// worse than a transiently mis-sized one, and the caller dedups re-sends anyway.
    ready: bool,
}

pub struct Outputs {
    outputs: HashMap<u32, Output>,
    /// The `zxdg_output_manager_v1` global, when the compositor ships one at version >= 3.
    manager: Option<ZxdgOutputManagerV1>,
    /// Set when a `done` arrived, so the caller knows to re-read and re-send.
    dirty: bool,
}

impl Outputs {
    pub fn new() -> Self {
        Outputs {
            outputs: HashMap::new(),
            manager: None,
            dirty: false,
        }
    }

    /// Bind every `wl_output` that existed before we connected.
    ///
    /// The registry `Dispatch` handles arrivals and departures from here on, but the globals
    /// present at connect time are delivered only through this list — a session that was
    /// already running (which is all of them) has all its monitors here and none in an event.
    pub fn bind_existing(
        &mut self,
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
    ) {
        // The manager is a singleton: present now or never, so binding it here covers the
        // hotplug arrivals the registry Dispatch handles too.
        self.manager = globals
            .bind::<ZxdgOutputManagerV1, _, _>(qh, 3..=3, ())
            .ok();
        let mut bound = Vec::new();
        globals.contents().with_list(|list| {
            for g in list {
                if g.interface != wl_output::WlOutput::interface().name || g.version < 4 {
                    continue;
                }
                let wl = globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, _>(g.name, 4, qh, g.name);
                bound.push((g.name, wl));
            }
        });
        for (name, wl) in bound {
            self.insert_output(name, wl, qh);
        }
    }

    /// Register a freshly bound `wl_output` and give it its xdg companion.
    fn insert_output(&mut self, name: u32, wl: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        let xdg = self
            .manager
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, name));
        self.outputs.insert(
            name,
            Output {
                wl: Some(wl),
                xdg,
                ..Output::default()
            },
        );
    }

    /// Take the pending-change flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The arrangement, or `None` while nothing complete has arrived yet.
    ///
    /// Monitors without a connector name are dropped rather than reported positionally: the
    /// host keys them onto scanouts by name, and a nameless entry could only be matched by
    /// guessing.
    pub fn layout(&self) -> Option<DisplayLayout> {
        let mut monitors: Vec<GuestMonitor> = self
            .outputs
            .values()
            .filter(|o| o.ready)
            .filter_map(|o| {
                let connector = o.name.clone()?;
                // The compositor's own logical rect when it can state one; mode divided by
                // the integer scale as the floor (see the module doc for what that misses).
                let (x, y) = o.logical_pos.unwrap_or((o.x, o.y));
                let (width, height) = o.logical_size.unwrap_or_else(|| {
                    let scale = o.scale.max(1) as u32;
                    (o.width / scale, o.height / scale)
                });
                Some(GuestMonitor {
                    connector,
                    x,
                    y,
                    width,
                    height,
                })
            })
            .collect();
        if monitors.is_empty() {
            return None;
        }
        // Left to right, then top to bottom. The host cares about the ordering more than the
        // absolute coordinates, and a stable order keeps an unchanged arrangement from looking
        // like a new one and re-sending.
        monitors.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
        Some(DisplayLayout { monitors })
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Outputs {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                // 4 is where `name` (the connector) arrives; without it a monitor cannot be
                // tied to a scanout, so an older compositor simply yields no layout.
                if version >= 4 {
                    let wl = registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, name);
                    state.insert_output(name, wl, qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(o) = state.outputs.remove(&name) {
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if let Some(wl) = o.wl {
                        wl.release();
                    }
                    state.dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                o.x = x;
                o.y = y;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Only the CURRENT mode describes the arrangement; a compositor advertises every
                // mode the monitor supports and the rest would overwrite it with a size nothing
                // is displaying.
                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(wl_output::Mode::Current))
                {
                    o.width = width.max(0) as u32;
                    o.height = height.max(0) as u32;
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Done => {
                o.ready = true;
                state.dirty = true;
            }
            _ => {}
        }
    }
}

/// No events: the manager only hands out `zxdg_output_v1` objects.
impl Dispatch<ZxdgOutputManagerV1, ()> for Outputs {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, u32> for Outputs {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width.max(0) as u32, height.max(0) as u32));
            }
            // `done` is deprecated at the version we bind (3): these events are committed by
            // the owning `wl_output.done`. `name`/`description` are deprecated too.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mutter running a monitor at fractional scale 1.25 advertises `wl_output.scale = 2`
    /// (the protocol carries only integers, and mutter rounds UP so buffers stay large
    /// enough), while placing the monitor at logical 2560/1.25 = 2048. The reported rect
    /// must be the compositor's own logical size — mode divided by the integer scale is
    /// wrong by 1.6x here, which was the measured 20% unreachable band
    /// (spikes/pointer-units-oracle/RESULTS.md, measurement 2).
    #[test]
    fn fractional_scale_reports_the_compositors_logical_size() {
        let mut o = Outputs::new();
        o.outputs.insert(
            1,
            Output {
                name: Some("Virtual-1".into()),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 2,
                logical_size: Some((2048, 1152)),
                ready: true,
                ..Default::default()
            },
        );
        let l = o.layout().expect("one ready monitor");
        assert_eq!(
            (l.monitors[0].width, l.monitors[0].height),
            (2048, 1152),
            "must report the compositor's logical size, not mode / integer scale"
        );
    }

    /// The compositor's logical position wins over `wl_output.geometry` for the same reason
    /// its size does: both halves of the rect must come from the same statement, or a
    /// mixed-source rectangle would not tile with its neighbours.
    #[test]
    fn logical_position_wins_over_geometry() {
        let mut o = Outputs::new();
        o.outputs.insert(
            1,
            Output {
                name: Some("Virtual-2".into()),
                x: 2560,
                y: 0,
                width: 3024,
                height: 1896,
                scale: 2,
                logical_pos: Some((2048, 0)),
                logical_size: Some((1512, 948)),
                ready: true,
                ..Default::default()
            },
        );
        let m = &o.layout().expect("one ready monitor").monitors[0];
        assert_eq!((m.x, m.y, m.width, m.height), (2048, 0, 1512, 948));
    }

    /// Without `zxdg_output_v1` (no manager, or older than 3) the floor is mode divided by
    /// the integer scale: correct at whole-number scales, knowingly wrong under fractional
    /// ones — the degraded tier, not a bug.
    #[test]
    fn without_xdg_output_falls_back_to_mode_over_integer_scale() {
        let mut o = Outputs::new();
        o.outputs.insert(
            1,
            Output {
                name: Some("Virtual-1".into()),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 2,
                ready: true,
                ..Default::default()
            },
        );
        let m = &o.layout().expect("one ready monitor").monitors[0];
        assert_eq!((m.width, m.height), (1280, 720));
    }
}
