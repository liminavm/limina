// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Which guest connector shows what — the slot table and the policy that drives it.
//!
//! A VM's scanout pool is fixed at boot (`num_scanouts` is virtio-gpu config-space state read
//! once at probe, so a display cannot be added to a running device). Every display a VM may ever
//! show therefore exists from boot as a **disconnected** scanout, and everything at this layer is
//! about deciding which of those slots are connected, and to what.
//!
//! No AppKit and no sockets in here: this takes the host's panels and the VM's presentation state
//! and returns a plan. The caller performs it. That is what makes the rules below testable at all
//! — every one of them was otherwise only observable by booting a VM onto two physical monitors.
//!
//! ## A panel owns a slot, permanently
//!
//! mutter identifies a monitor by connector name **and** vendor/product/serial, all four
//! (`meta_monitor_spec_equals`), and the connector name is the slot: the guest driver creates its
//! outputs in scanout order at probe, so slot *i* is `Virtual-(i+1)` for the life of the boot.
//! A panel that lands on slot 0 in one session and slot 1 in the next is therefore *two different
//! monitors* to the guest, each with its own saved arrangement — the same thing that happens on
//! real hardware when a monitor moves from DP-1 to DP-2. So the assignment is keyed on
//! [`hostdisplay::panel_key`] and kept: a panel that goes away holds its slot, and gets it back
//! when it returns.
//!
//! ## Firmware paints slot 0, and only slot 0
//!
//! EDK2's virtio-gpu GOP driver hardcodes head 0 (`OvmfPkg/VirtioGpuDxe/Gop.c`) — it takes its
//! *resolution* from the first enabled scanout but always renders into head 0. So while firmware
//! and GRUB are drawing, slot 0 must be the connected one no matter which panel the window is on.
//! Only once the guest's OS driver takes over (its first `GET_EDID`, which firmware never sends —
//! see `guest_driver_ready` on the display backend) does the pool become ours to arrange.
//!
//! That is a real phase, not a startup detail: a guest reboot goes back to firmware, and the
//! plan must go back with it.

use limina_displayctl::{DisplayCommand, DisplayControl};

/// Where a slot's identity comes from.
///
/// `Virtual` is not used yet; it is what a user-defined display with a custom EDID will be, and
/// it is written here now so that adding one is a row in this table rather than a second
/// mechanism beside it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SlotSource {
    /// Mirrors an attached host panel, keyed on [`super::hostdisplay::panel_key`].
    Host(u64),
    /// A display the user defined, with an identity of their own. Never assigned yet.
    #[allow(dead_code)]
    Virtual,
    /// No display has claimed this slot.
    Unassigned,
}

/// Whether the guest is being driven by boot firmware or by its own OS driver.
///
/// The phase is observed, not declared: the device reports the guest's first `GET_EDID`, which
/// firmware never issues and every Linux connector probe does. So it needs no agent, no custom
/// kernel and no cooperation from the guest at all.
///
/// A VM resumed into a *fresh* supervisor process starts here at `Firmware` although the
/// restored guest's driver is long since up, and the driver re-probes its connectors only if
/// asked — so the restore path queues an empty display update after the GIC restore
/// (`limina-vmm/src/krun/mod.rs`, the re-probe nudge), whose config-change makes the guest
/// re-read every EDID and re-fire the handover. (Raised any earlier — e.g. from the GPU
/// worker's staged-replay branch — the interrupt is wiped by the GIC state restored after it;
/// measured.) In-process resume needs none of this: the phase survives the worker swap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Firmware/bootloader: slot 0 is the only scanout anything paints.
    #[default]
    Firmware,
    /// The guest's OS driver has taken over: every slot is addressable and hotplug works.
    Os,
}

/// What the VM is showing right now, which is what decides how many panels are lit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Presentation {
    /// One window on one panel: that panel's slot, and nothing else.
    Windowed { panel: u64 },
    /// Fullscreen across every attached panel. `panel` is still the one the *window* is on —
    /// the only slot that cannot be switched off, because it is the one the user is looking at.
    FullscreenAll { panel: u64 },
}

/// One connector's worth of state.
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub(crate) source: SlotSource,
    /// Whether the user wants this display used at all. Deliberately separate from
    /// [`Self::connected`]: that is what the guest is believed to see *now*, this is the standing
    /// instruction that decides it. A display switched off stays off across a fullscreen cycle, a
    /// migration and a VM restart, and comes back the moment it is switched on.
    pub(crate) enabled: bool,
    /// Whether the guest currently sees this connector as plugged in. This is what the table
    /// *believes*; [`DisplayTable::plan`] returns the pushes that make the guest agree.
    pub(crate) connected: bool,
    /// Whether this slot has been told WHAT it is — the panel's size and EDID — by us.
    ///
    /// Separate from [`Self::connected`], because a fresh device boots slot 0 connected while
    /// carrying virtio's own default EDID: up, but not ours. The one thing the connected bit
    /// alone cannot say, and saying it wrong renders the guest at a 10" panel's scale.
    pub(crate) announced: bool,
}

/// The VM's whole pool: one entry per scanout, plus which phase the guest is in.
#[derive(Clone, Debug)]
pub(crate) struct DisplayTable {
    slots: Vec<Slot>,
    phase: Phase,
    /// The pool has already complained that it cannot show something. Planning runs at frame
    /// rate and these conditions persist for as long as the arrangement does, so the warning is
    /// per transition, not per tick.
    exhausted: bool,
    /// Panels the user switched off, as loaded from the VM's state. Kept whole rather than
    /// consumed, because a panel can be given its slot long after the restore.
    saved_disabled: Vec<u64>,
}

/// One display the user can switch on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayRow {
    /// The connector it owns, or `None` until something has needed to show it.
    #[allow(dead_code)]
    pub(crate) slot: Option<u32>,
    pub(crate) panel: u64,
    pub(crate) enabled: bool,
}

/// What a push does to its slot. Deliberately not an `Option<panel>`: "connect carrying no
/// identity" and "disconnect" are opposite instructions, and connector plumbing is the last
/// place to leave those one `None` apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotAction {
    /// Bring the slot up carrying that panel's identity. The caller turns the panel key into an
    /// EDID — this layer never touches AppKit.
    Connect(u64),
    /// Take the slot down.
    Disconnect,
}

/// A push the caller should send, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotPush {
    pub(crate) slot: u32,
    pub(crate) action: SlotAction,
}

impl DisplayTable {
    /// A pool of `pool` slots, all unassigned, in the firmware phase.
    ///
    /// Slot 0 starts life connected because that is how the worker boots the device: firmware
    /// needs a scanout to paint before anything here has run.
    pub(crate) fn new(pool: u32) -> Self {
        // Clamped to what the presenter tracks and the device allows — a table wider than
        // `Shared::slots` would hand out a slot nothing can ever present.
        let pool = (pool.max(1) as usize).min(super::present::MAX_SCANOUTS);
        let mut slots = vec![
            Slot {
                source: SlotSource::Unassigned,
                enabled: true,
                connected: false,
                announced: true,
            };
            pool
        ];
        slots[0].connected = true;
        Self {
            slots,
            phase: Phase::Firmware,
            exhausted: false,
            saved_disabled: Vec::new(),
        }
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    /// The guest's OS driver took over. Returns whether this changed anything, so the caller can
    /// skip the re-plan on the (many) ticks where it did not.
    pub(crate) fn enter_os_phase(&mut self) -> bool {
        let changed = self.phase == Phase::Firmware;
        self.phase = Phase::Os;
        changed
    }

    /// Back to firmware — a guest reboot. The assignment survives (the panels did not move);
    /// only the phase and the connector state go back to how the device boots.
    pub(crate) fn reset_to_firmware(&mut self) {
        self.phase = Phase::Firmware;
        self.reset_connectors_to_boot();
    }

    /// The device these beliefs described is gone — a fresh worker was swapped in. Whatever the
    /// guest was showing, a new virtio-gpu device boots the way every one of them does: slot 0
    /// connected, every other slot down. So the table has to stop believing what it told the old
    /// worker, and say the arrangement again.
    ///
    /// Connector state only. The assignment is not device state (the panels did not move), and
    /// neither is the phase: a REBOOT goes back to firmware and calls
    /// [`Self::reset_to_firmware`], but a guest RESTORED from a snapshot has its driver already
    /// up and will never send the second `GET_EDID` that would earn the OS phase back.
    ///
    /// Left out, this is the 2026-08-22 stuck resume: the table kept slot 1 connected, the plan
    /// diffed to nothing, the guest came back on slot 0 alone, and the window sat watching a slot
    /// nothing drove.
    pub(crate) fn reset_connectors_to_boot(&mut self) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            slot.connected = i == 0;
            // Slot 0 included, and it is the whole reason this bit exists: the device really
            // does boot it connected, so the diff alone would leave it carrying virtio's
            // default identity for good — invisible unless that slot belongs to a panel other
            // than the window's, which is the only case where nothing else describes it.
            slot.announced = false;
        }
    }

    /// The slot this panel owns, assigning it one if it has none.
    ///
    /// Assignment is first-come, lowest-free — so the panel the VM opens on takes slot 0 without
    /// needing a rule of its own, and every later panel gets the next free connector. When the
    /// pool is full, a slot belonging to a panel that is no longer attached is recycled; if even
    /// that fails there is no connector for this panel and it cannot be shown.
    pub(crate) fn slot_for(&mut self, panel: u64, attached: &[u64]) -> Option<u32> {
        if let Some(i) = self.find(panel) {
            return Some(i);
        }
        let free = self
            .slots
            .iter()
            .position(|s| s.source == SlotSource::Unassigned)
            .or_else(|| {
                // Recycle the slot of a panel that is no longer attached. This costs that panel
                // its saved arrangement in the guest, which is why it is the fallback and not
                // the policy.
                self.slots.iter().position(|s| match s.source {
                    SlotSource::Host(key) => !attached.contains(&key),
                    _ => false,
                })
            });
        match free {
            Some(i) => {
                if let SlotSource::Host(old) = self.slots[i].source {
                    log::info!(
                        "display: slot {i} recycled from detached panel {old:x} for panel {panel:x}; \
                         the guest will treat it as a new monitor"
                    );
                }
                // A recycled slot is a DIFFERENT monitor on the same connector, so the guest
                // has to be told even though the connector never went away in our own
                // bookkeeping. Marking it disconnected here is what makes `plan`'s diff emit
                // the cycle that carries the new identity — leave it `connected` and the diff
                // sees no change, pushes nothing, and the guest keeps showing the departed
                // panel's identity and mode on a connector now presenting another display.
                let recycled =
                    matches!(self.slots[i].source, SlotSource::Host(old) if old != panel);
                self.slots[i].source = SlotSource::Host(panel);
                if recycled {
                    self.slots[i].connected = false;
                }
                // A panel switched off in an earlier run has no slot to be switched off *on*
                // until it gets one, so the remembered set is applied at assignment rather than
                // at restore. Without this, unplugging and replugging a disabled display would
                // quietly switch it back on.
                self.slots[i].enabled = !self.saved_disabled.contains(&panel);
                self.exhausted = false;
                Some(i as u32)
            }
            None => {
                if !self.exhausted {
                    self.exhausted = true;
                    log::warn!(
                        "display: no free scanout for panel {panel:x} (pool of {}); it cannot \
                         be shown to the guest — raise --display-pool to use more displays",
                        self.slots.len()
                    );
                }
                None
            }
        }
    }

    /// The slot the window's own picture is on right now.
    ///
    /// Firmware paints head 0 and no other, so for as long as the guest is in that phase the
    /// window follows slot 0 whatever its panel owns. Without this a guest rebooted while the
    /// window sat on a panel holding slot 1 put GRUB in a view-only secondary window on the
    /// other panel, with the main window frozen on its last pre-reboot frame — observed on the
    /// two-panel rig, 2026-08-17. The panel keeps its assignment throughout; only which slot is
    /// *shown* changes, and the OS phase hands the window straight back to it.
    pub(crate) fn present_slot(&mut self, panel: u64, attached: &[u64]) -> u32 {
        let owned = self.slot_for(panel, attached).unwrap_or(0);
        match self.phase {
            Phase::Firmware => 0,
            Phase::Os => owned,
        }
    }

    fn find(&self, panel: u64) -> Option<u32> {
        self.slots
            .iter()
            .position(|s| s.source == SlotSource::Host(panel))
            .map(|i| i as u32)
    }

    /// Which slots should be connected, given the phase, what the VM is showing, and which
    /// panels are attached to the host.
    fn wanted(&mut self, presentation: Presentation, attached: &[u64]) -> Vec<bool> {
        let mut want = vec![false; self.slots.len()];
        // Firmware paints head 0 and no other, whichever panel the window is on.
        if self.phase == Phase::Firmware {
            want[0] = true;
            return want;
        }
        // The panel the window is on is wanted whatever its `enabled` says. The user cannot switch
        // off the display they are looking at (the menu greys that row), but a panel switched off
        // while the window was elsewhere can still become the window's — and a window showing a
        // connector the guest has unplugged shows nothing at all.
        let primary = match presentation {
            Presentation::Windowed { panel } | Presentation::FullscreenAll { panel } => panel,
        };
        let panels: &[u64] = match presentation {
            Presentation::Windowed { .. } => &[primary],
            Presentation::FullscreenAll { .. } => attached,
        };
        for &panel in panels {
            let Some(slot) = self.slot_for(panel, attached) else {
                continue;
            };
            if panel == primary || self.slots[slot as usize].enabled {
                want[slot as usize] = true;
            }
        }
        // A guest with no connected display at all is a state no compositor handles gracefully,
        // and it is reachable: a panel with no slot, or an empty screen list mid-transition.
        // Keep whatever is already connected rather than blanking the guest.
        if !want.iter().any(|w| *w) {
            if !self.exhausted {
                self.exhausted = true;
                log::warn!("display: no slot to show; leaving the guest's connectors as they are");
            }
            for (i, slot) in self.slots.iter().enumerate() {
                want[i] = slot.connected;
            }
        }
        want
    }

    /// The pushes that make the guest match [`Self::wanted`], and the table's own state with
    /// them applied.
    ///
    /// Disconnects come first. A slot handover (the window moved to another panel) is then a
    /// disconnect followed by a connect, which is the connector cycle the guest already
    /// understands — and the ordering rule that earned its measurements holds here too: the
    /// arriving identity rides the *connect*, never the disconnect.
    pub(crate) fn plan(&mut self, presentation: Presentation, attached: &[u64]) -> Vec<SlotPush> {
        let mut want = self.wanted(presentation, attached);
        // A slot that is up but was never told what it is (a fresh device's slot 0) is owed a
        // connect — but only once there is a DRIVER to hear it. Firmware reads the boot scanout
        // and paints head 0; an identity said to it may not survive the driver's probe reset
        // anyway, and the phase change re-announces on the way out. So in firmware the diff is
        // the plain one it always was.
        let identity_owed = self.phase == Phase::Os;
        let described = |slot: &Slot| slot.connected && (slot.announced || !identity_owed);
        // A slot with no panel behind it has no identity to arrive with, so there is nothing to
        // connect: hold it wherever it already is and let the next tick re-diff once it has a
        // source. (Slot 0 in the firmware phase is already connected — the device booted it that
        // way, carrying the boot EDID — so it never reaches here.)
        for (i, slot) in self.slots.iter().enumerate() {
            if want[i] && !described(slot) && !matches!(slot.source, SlotSource::Host(_)) {
                want[i] = slot.connected;
            }
        }
        let mut pushes = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.connected && !want[i] {
                pushes.push(SlotPush {
                    slot: i as u32,
                    action: SlotAction::Disconnect,
                });
            }
        }
        for (i, slot) in self.slots.iter().enumerate() {
            if want[i] && !described(slot) {
                let SlotSource::Host(key) = slot.source else {
                    continue;
                };
                pushes.push(SlotPush {
                    slot: i as u32,
                    action: SlotAction::Connect(key),
                });
            }
        }
        for (i, slot) in self.slots.iter_mut().enumerate() {
            slot.connected = want[i];
            // A connect carries the panel's size and EDID, so a slot the plan keeps up is
            // described as of now. One that goes down is owed the identity again when it
            // returns — the connector cycle is how the guest is told at all.
            slot.announced = want[i] && matches!(slot.source, SlotSource::Host(_));
        }
        pushes
    }

    /// The slots the guest should currently be showing, in order — what the presenter opens
    /// windows for.
    pub(crate) fn connected_slots(&self) -> Vec<u32> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.connected)
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// The panel a slot is showing, if it has one.
    pub(crate) fn panel_of(&self, slot: u32) -> Option<u64> {
        match self.slots.get(slot as usize)?.source {
            SlotSource::Host(key) => Some(key),
            _ => None,
        }
    }

    /// The assignment, for persistence: `(panel_key, slot)` pairs. A VM that remembers this
    /// gives a panel the same connector across restarts, which is the whole point of keying on
    /// the panel in the first place.
    pub(crate) fn assignment(&self) -> Vec<(u64, u32)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s.source {
                SlotSource::Host(key) => Some((key, i as u32)),
                _ => None,
            })
            .collect()
    }

    /// One row of the Displays menu per **attached** panel — not per assigned slot.
    ///
    /// A panel only earns a connector when something wants to show it, so gating the menu on the
    /// assignment would hide exactly the display the user wants to switch off *before* going
    /// fullscreen. Intent is keyed on the panel and survives having no slot, so a row without one
    /// is still switchable; it takes effect when the panel is given a connector.
    pub(crate) fn rows(&self, attached: &[u64]) -> Vec<DisplayRow> {
        attached
            .iter()
            .map(|&panel| DisplayRow {
                slot: self.find(panel),
                panel,
                enabled: self.is_enabled(panel),
            })
            .collect()
    }

    /// Whether the user wants this panel used, whether or not it has a connector yet.
    pub(crate) fn is_enabled(&self, panel: u64) -> bool {
        match self.find(panel) {
            Some(slot) => self.slots[slot as usize].enabled,
            None => !self.saved_disabled.contains(&panel),
        }
    }

    /// Switch a panel's display on or off. Unknown panels are ignored — a menu built one tick and
    /// clicked the next can name a display that has since been unplugged.
    pub(crate) fn set_enabled(&mut self, panel: u64, enabled: bool) {
        // The remembered set is kept in step whether or not the panel is attached, so a display
        // switched back on does not come back off when it is next plugged in.
        self.saved_disabled.retain(|p| *p != panel);
        if !enabled {
            self.saved_disabled.push(panel);
        }
        let Some(slot) = self.find(panel) else {
            return;
        };
        let slot = &mut self.slots[slot as usize];
        if slot.enabled != enabled {
            slot.enabled = enabled;
            // The arrangement changed, so a pool that had nothing to show may now have something.
            self.exhausted = false;
        }
    }

    /// The panels the user has switched off, for persistence. Keyed on the panel and not the slot
    /// because that is what the user turned off — a display, not a connector. This is the
    /// remembered *intent* set, not a slot scan: the slots' `enabled` flags are derived from it
    /// ([`Self::set_enabled`], [`Self::slot_for`]), and a switched-off display that has no slot
    /// this session — never plugged in, or its slot recycled for another panel — must still
    /// persist as off, or the frame-rate save erases the user's decision.
    pub(crate) fn disabled_panels(&self) -> Vec<u64> {
        self.saved_disabled.clone()
    }

    /// Restore the switched-off set. The set is *kept*, not consumed: a panel that has no slot
    /// yet gets its state applied by [`Self::slot_for`] when it is given one.
    pub(crate) fn restore_disabled(&mut self, saved: &[u64]) {
        for &panel in saved {
            self.set_enabled(panel, false);
        }
    }

    /// Restore a remembered assignment. Entries beyond this VM's pool are dropped — the pool may
    /// have been configured smaller since — and the connector state is left alone, because it is
    /// decided by the phase and the presentation, never by what a previous run was showing.
    pub(crate) fn restore_assignment(&mut self, saved: &[(u64, u32)]) {
        for &(panel, slot) in saved {
            match self.slots.get_mut(slot as usize) {
                Some(s) if s.source == SlotSource::Unassigned => {
                    s.source = SlotSource::Host(panel);
                }
                Some(_) => log::warn!("display: saved slot {slot} for panel {panel:x} is taken"),
                None => log::info!(
                    "display: saved slot {slot} for panel {panel:x} is outside this VM's pool"
                ),
            }
        }
    }
}

/// What the window's shape means for the pool. Fullscreen takes over the other panels only
/// when the user asked for it (the Displays menu's "Use Other Screens When Fullscreen", off by
/// default) — otherwise the VM occupies exactly the panel its window is on, fullscreen or not.
/// `None` — the window is not on any panel yet — presents slot 0, matching what firmware paints.
pub(crate) fn presentation_for(
    panel: Option<u64>,
    fullscreen: bool,
    use_other_screens: bool,
) -> Presentation {
    match panel {
        Some(panel) if fullscreen && use_other_screens => Presentation::FullscreenAll { panel },
        Some(panel) => Presentation::Windowed { panel },
        None => Presentation::Windowed { panel: 0 },
    }
}

/// Turn a push into the wire command for a *disconnect*. The connect half needs an EDID and so
/// is built by the caller from [`super::hostdisplay::describe`].
pub(crate) fn disconnect_command(slot: u32) -> DisplayCommand {
    DisplayCommand::Display(DisplayControl {
        display_id: slot,
        connected: Some(false),
        ..DisplayControl::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A display the user switched off is not connected when fullscreen lights up the rest.
    #[test]
    fn a_display_switched_off_is_left_unplugged() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.connected_slots().len(), 2);

        t.set_enabled(STUDIO, false);
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert_eq!(
            pushes,
            vec![SlotPush {
                slot: studio,
                action: SlotAction::Disconnect
            }]
        );
        assert!(!t.connected_slots().contains(&studio));

        // And back on, carrying its identity on the connect as every arrival does.
        t.set_enabled(STUDIO, true);
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            pushes,
            vec![SlotPush {
                slot: studio,
                action: SlotAction::Connect(STUDIO)
            }]
        );
    }

    /// Switching a display off must not cost it its connector: the whole point of the per-panel
    /// assignment is that the guest sees the same monitor on the same connector next time.
    #[test]
    fn a_switched_off_display_keeps_its_slot() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        t.set_enabled(STUDIO, false);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.slot_for(STUDIO, &[BUILT_IN, STUDIO]), Some(studio));
        assert_eq!(t.assignment().len(), 2);
    }

    /// The window's own panel is never switched off. The user cannot reach that state through the
    /// menu, but a panel switched off while the window was elsewhere can become the window's.
    #[test]
    fn the_window_s_own_panel_is_shown_even_when_switched_off() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        t.set_enabled(STUDIO, false);
        t.plan(
            Presentation::Windowed { panel: STUDIO },
            &[BUILT_IN, STUDIO],
        );
        let studio = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert_eq!(t.connected_slots(), vec![studio]);
    }

    /// The switched-off set is remembered against the panel, so a display the user turned off
    /// stays off when it is unplugged and plugged back in — the case a slot-indexed set misses,
    /// because at restore time the panel has no slot yet.
    #[test]
    fn a_display_switched_off_stays_off_across_a_replug_and_a_restart() {
        let mut first = os_table(4);
        first.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        first.set_enabled(STUDIO, false);
        let assignment = first.assignment();
        let disabled = first.disabled_panels();
        assert_eq!(disabled, vec![STUDIO]);

        let mut next = os_table(4);
        next.restore_assignment(&assignment);
        next.restore_disabled(&disabled);
        next.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio = next.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert!(!next.connected_slots().contains(&studio));

        // A VM that has never seen the panel before still honours the remembered set the moment
        // the panel is given a slot.
        let mut fresh = os_table(4);
        fresh.restore_disabled(&disabled);
        fresh.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio = fresh.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert!(!fresh.connected_slots().contains(&studio));
    }

    /// Persistence reads the switched-off set every frame. A session where the switched-off
    /// display never shows up (or its slot is recycled for another panel) has no slot carrying
    /// the flag — the set must come from intent, not from the slots, or that first frame
    /// persists an empty set and erases the user's decision.
    #[test]
    fn the_off_set_survives_a_session_where_the_display_never_appears() {
        let mut t = os_table(4);
        t.restore_disabled(&[STUDIO]);
        t.plan(Presentation::FullscreenAll { panel: BUILT_IN }, &[BUILT_IN]);
        assert_eq!(t.disabled_panels(), vec![STUDIO]);

        // Same law from the other direction: switching off a panel that has no slot yet (the
        // menu's own "switch it off before going fullscreen" case) must persist immediately,
        // not only once the panel is given a connector.
        let mut t = os_table(4);
        t.set_enabled(STUDIO, false);
        assert_eq!(t.disabled_panels(), vec![STUDIO]);
    }

    /// Switching a display back on has to clear the remembered set too, or the next replug would
    /// resurrect a decision the user has already undone.
    #[test]
    fn switching_a_display_back_on_forgets_that_it_was_off() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        t.set_enabled(STUDIO, false);
        t.set_enabled(STUDIO, true);
        assert!(t.disabled_panels().is_empty());
    }

    /// The menu is built from these rows: one per **attached** panel, whether or not it has been
    /// given a connector yet — otherwise the display a user wants to switch off before going
    /// fullscreen is the one display the menu does not list.
    #[test]
    fn the_rows_describe_every_attached_panel_slot_or_not() {
        let mut t = os_table(4);
        t.plan(
            Presentation::Windowed { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let rows = t.rows(&[BUILT_IN, STUDIO]);
        assert_eq!(rows.len(), 2);
        let studio = rows.iter().find(|r| r.panel == STUDIO).unwrap();
        assert_eq!(studio.slot, None, "nothing has needed to show it yet");
        assert!(studio.enabled);

        // Switched off before it ever had a connector, and still off once it gets one.
        t.set_enabled(STUDIO, false);
        assert!(!t.rows(&[BUILT_IN, STUDIO])[1].enabled);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert!(!t.connected_slots().contains(&studio));
    }

    const BUILT_IN: u64 = 0x1111;
    const STUDIO: u64 = 0x2222;
    const THIRD: u64 = 0x3333;

    fn os_table(pool: u32) -> DisplayTable {
        let mut t = DisplayTable::new(pool);
        t.enter_os_phase();
        t
    }

    #[test]
    fn firmware_paints_slot_zero_whatever_the_window_is_on() {
        let mut t = DisplayTable::new(4);
        // Even fullscreen across two panels: firmware renders into head 0 and nothing else, so
        // lighting another slot would paint a display nothing draws to.
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert!(pushes.is_empty(), "slot 0 already boots connected");
        assert_eq!(t.connected_slots(), vec![0]);
    }

    #[test]
    fn a_panel_keeps_its_slot_when_another_comes_and_goes() {
        let mut t = os_table(4);
        t.plan(Presentation::Windowed { panel: BUILT_IN }, &[BUILT_IN]);
        let built_in_slot = t.slot_for(BUILT_IN, &[BUILT_IN]).unwrap();

        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio_slot = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();
        assert_ne!(built_in_slot, studio_slot);

        // Unplug the studio display, work for a while, plug it back in.
        t.plan(Presentation::Windowed { panel: BUILT_IN }, &[BUILT_IN]);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.slot_for(STUDIO, &[BUILT_IN, STUDIO]), Some(studio_slot));
        assert_eq!(
            t.slot_for(BUILT_IN, &[BUILT_IN, STUDIO]),
            Some(built_in_slot)
        );
    }

    #[test]
    fn the_panel_the_vm_opens_on_takes_slot_zero() {
        let mut t = os_table(4);
        t.plan(
            Presentation::Windowed { panel: STUDIO },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.slot_for(STUDIO, &[BUILT_IN, STUDIO]), Some(0));
    }

    #[test]
    fn fullscreen_lights_every_attached_panel_and_leaving_collapses_to_one() {
        let mut t = os_table(4);
        t.plan(
            Presentation::Windowed { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.connected_slots(), vec![0]);

        let enter = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            enter,
            vec![SlotPush {
                slot: 1,
                action: SlotAction::Connect(STUDIO)
            }],
            "entering fullscreen only ADDS the other panel; the window's own slot never cycles"
        );
        assert_eq!(t.connected_slots(), vec![0, 1]);

        let leave = t.plan(
            Presentation::Windowed { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            leave,
            vec![SlotPush {
                slot: 1,
                action: SlotAction::Disconnect
            }]
        );
        assert_eq!(t.connected_slots(), vec![0]);
    }

    #[test]
    fn moving_the_window_between_panels_is_a_disconnect_then_a_connect() {
        let mut t = os_table(4);
        t.plan(
            Presentation::Windowed { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let pushes = t.plan(
            Presentation::Windowed { panel: STUDIO },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            pushes,
            vec![
                SlotPush {
                    slot: 0,
                    action: SlotAction::Disconnect
                },
                SlotPush {
                    slot: 1,
                    action: SlotAction::Connect(STUDIO)
                },
            ],
            "the disconnect comes first, and the arriving identity rides the connect"
        );
    }

    /// Recycling the slot of a departed panel gives that connector to a DIFFERENT monitor, and
    /// the guest has to be told: the identity, the mode and the saved arrangement all belong to
    /// the panel that left. Leaving the slot `connected` made `plan`'s diff see no change and
    /// push nothing, so the guest kept showing the old monitor's identity on a connector now
    /// presenting a different physical display.
    #[test]
    fn recycling_a_connected_slot_cycles_it_for_the_new_panel() {
        let mut t = os_table(2);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let studio_slot = t.slot_for(STUDIO, &[BUILT_IN, STUDIO]).unwrap();

        // The Studio is unplugged and a third monitor takes its place. The pool is full, so the
        // only slot available is the one the Studio just vacated.
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, THIRD],
        );
        assert_eq!(
            t.slot_for(THIRD, &[BUILT_IN, THIRD]),
            Some(studio_slot),
            "the third monitor takes the recycled slot"
        );
        let pushes = if pushes.is_empty() {
            t.plan(
                Presentation::FullscreenAll { panel: BUILT_IN },
                &[BUILT_IN, THIRD],
            )
        } else {
            pushes
        };
        assert!(
            pushes
                .iter()
                .any(|p| p.slot == studio_slot && p.action == SlotAction::Connect(THIRD)),
            "the recycled slot must be connected carrying the NEW panel's identity; got {pushes:?}"
        );
    }

    #[test]
    fn an_unchanged_arrangement_pushes_nothing() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert!(t
            .plan(
                Presentation::FullscreenAll { panel: BUILT_IN },
                &[BUILT_IN, STUDIO]
            )
            .is_empty());
    }

    #[test]
    fn a_pool_of_one_still_shows_the_window_s_panel() {
        let mut t = os_table(1);
        t.plan(Presentation::Windowed { panel: BUILT_IN }, &[BUILT_IN]);
        // The second panel has nowhere to go, but fullscreen must not blank the first.
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert!(pushes.is_empty());
        assert_eq!(t.connected_slots(), vec![0]);
    }

    #[test]
    fn a_full_pool_recycles_a_detached_panel_s_slot() {
        let mut t = os_table(2);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.slot_for(STUDIO, &[BUILT_IN, STUDIO]), Some(1));

        // The studio display is gone; a different one arrives with the pool already full.
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, THIRD],
        );
        assert_eq!(t.slot_for(THIRD, &[BUILT_IN, THIRD]), Some(1));
        assert_eq!(t.connected_slots(), vec![0, 1]);
    }

    #[test]
    fn a_resume_re_asserts_the_arrangement_onto_the_fresh_device() {
        // The trigger, from the 2026-08-22 rig run: the external panel is switched off, so the
        // built-in — which owns slot 1 — is the only panel left, and the window shows slot 1.
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: STUDIO },
            &[STUDIO, BUILT_IN],
        );
        assert_eq!(t.slot_for(BUILT_IN, &[STUDIO, BUILT_IN]), Some(1));
        let before = t.assignment();

        // Suspend, then resume in place. The restored guest's driver is up — no second
        // `GET_EDID` is coming — but the WORKER is new, and a fresh virtio-gpu device boots
        // the way every device boots: slot 0 connected, nothing else.
        t.reset_connectors_to_boot();
        assert_eq!(
            t.phase(),
            Phase::Os,
            "the restored guest's driver is still up"
        );
        assert_eq!(t.assignment(), before, "the panels did not move");

        // So the next plan has to say the whole arrangement again. Left believing what it told
        // the OLD worker, the table diffs to nothing, the guest stays on slot 0, and the window
        // watches a slot nothing drives — the frozen frame and the stuck "Resuming…" overlay.
        let pushes = t.plan(Presentation::Windowed { panel: BUILT_IN }, &[BUILT_IN]);
        assert_eq!(
            pushes,
            vec![
                SlotPush {
                    slot: 0,
                    action: SlotAction::Disconnect
                },
                SlotPush {
                    slot: 1,
                    action: SlotAction::Connect(BUILT_IN)
                },
            ]
        );
    }

    #[test]
    fn the_slot_a_fresh_device_boots_up_is_still_owed_its_identity() {
        // Rig 2026-08-22, the resume that came back letterboxed. A fresh device has slot 0
        // CONNECTED — but carrying virtio's own boot EDID, not the panel's, and sized to
        // whatever the worker was spawned with. If that slot belongs to a panel which is not
        // the window's, nothing else will ever describe it: the primary's size-and-identity
        // push follows the WINDOW's panel, and a plan that only re-emits slots it believes are
        // down skips this one. The guest is left reading virtio's default 10" panel, picks a
        // 250% scale for it, and renders a mode the real panel cannot show.
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            t.connected_slots(),
            vec![0, 1],
            "built-in on 0, studio on 1"
        );

        // Resume, with the window now on the STUDIO — so slot 1 is the primary's, and slot 0
        // is a panel only this plan speaks for.
        t.reset_connectors_to_boot();
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: STUDIO },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            pushes,
            vec![
                SlotPush {
                    slot: 0,
                    action: SlotAction::Connect(BUILT_IN)
                },
                SlotPush {
                    slot: 1,
                    action: SlotAction::Connect(STUDIO)
                },
            ],
            "every slot the arrangement keeps must be described again, slot 0 included"
        );
        // Said once: the tick runs at frame rate.
        assert!(t
            .plan(
                Presentation::FullscreenAll { panel: STUDIO },
                &[BUILT_IN, STUDIO]
            )
            .is_empty());
    }

    #[test]
    fn re_asserting_an_unchanged_arrangement_after_a_resume_says_it_once() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(t.connected_slots(), vec![0, 1]);

        t.reset_connectors_to_boot();
        let first = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            first,
            vec![
                SlotPush {
                    slot: 0,
                    action: SlotAction::Connect(BUILT_IN)
                },
                SlotPush {
                    slot: 1,
                    action: SlotAction::Connect(STUDIO)
                },
            ],
            "slot 0 boots connected but unidentified, so it is owed a connect like the rest"
        );
        // And the tick runs at frame rate: having said it, the table must go quiet.
        let again = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert!(again.is_empty(), "re-assert once, not every tick");
    }

    #[test]
    fn a_reboot_returns_to_firmware_but_keeps_the_assignment() {
        let mut t = os_table(4);
        t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        let before = t.assignment();

        t.reset_to_firmware();
        assert_eq!(t.phase(), Phase::Firmware);
        assert_eq!(t.connected_slots(), vec![0], "firmware paints head 0 only");
        assert_eq!(t.assignment(), before, "the panels did not move");

        // And once the rebooted guest's driver is up, the same panels light the same slots —
        // slot 0 included. Its connector never went down, but the device it is on is new and
        // carries virtio's boot EDID until something says otherwise, and firmware was the wrong
        // audience for that (it paints head 0 and reads nothing else).
        assert!(t.enter_os_phase());
        let pushes = t.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[BUILT_IN, STUDIO],
        );
        assert_eq!(
            pushes,
            vec![
                SlotPush {
                    slot: 0,
                    action: SlotAction::Connect(BUILT_IN)
                },
                SlotPush {
                    slot: 1,
                    action: SlotAction::Connect(STUDIO)
                },
            ]
        );
    }

    #[test]
    fn a_remembered_assignment_gives_a_panel_the_same_connector_next_run() {
        let mut first = os_table(4);
        first.plan(Presentation::Windowed { panel: STUDIO }, &[STUDIO]);
        first.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[STUDIO, BUILT_IN],
        );
        let saved = first.assignment();

        // Next run, and this time the VM opens on the OTHER panel: without the saved table the
        // built-in would take slot 0 and both panels would read as new monitors to the guest.
        let mut next = DisplayTable::new(4);
        next.restore_assignment(&saved);
        next.enter_os_phase();
        next.plan(
            Presentation::FullscreenAll { panel: BUILT_IN },
            &[STUDIO, BUILT_IN],
        );
        assert_eq!(next.assignment(), saved);
    }

    #[test]
    fn the_window_follows_slot_0_while_the_guest_is_in_firmware() {
        let mut t = DisplayTable::new(4);
        // A panel that owns slot 1 — the arrangement a reboot after a window migration leaves.
        t.enter_os_phase();
        t.slot_for(BUILT_IN, &[BUILT_IN, STUDIO]);
        assert_eq!(t.present_slot(STUDIO, &[BUILT_IN, STUDIO]), 1);

        t.reset_to_firmware();
        assert_eq!(
            t.present_slot(STUDIO, &[BUILT_IN, STUDIO]),
            0,
            "firmware paints head 0, so the window shows head 0"
        );
        // ...and the panel never lost its slot; the OS phase hands it straight back.
        t.enter_os_phase();
        assert_eq!(t.present_slot(STUDIO, &[BUILT_IN, STUDIO]), 1);
    }

    #[test]
    fn a_slot_with_no_panel_behind_it_is_never_connected_blind() {
        // A connect carries an identity; there is none to carry for an unassigned slot, and
        // bringing one up bare would have the guest save a configuration for a monitor that
        // does not exist. The table holds it where it is instead and re-diffs next tick.
        let mut t = os_table(2);
        t.plan(Presentation::Windowed { panel: BUILT_IN }, &[BUILT_IN]);
        // Slot 1 is unassigned; asking for every panel cannot conjure one for it.
        let pushes = t.plan(Presentation::FullscreenAll { panel: BUILT_IN }, &[BUILT_IN]);
        assert!(pushes.is_empty(), "nothing to connect, so nothing pushed");
        assert_eq!(t.connected_slots(), vec![0]);
    }

    #[test]
    fn a_saved_slot_outside_a_shrunken_pool_is_dropped_not_panicked_on() {
        let mut t = DisplayTable::new(2);
        t.restore_assignment(&[(BUILT_IN, 0), (STUDIO, 5)]);
        t.enter_os_phase();
        assert_eq!(t.slot_for(BUILT_IN, &[BUILT_IN]), Some(0));
        assert_eq!(t.slot_for(STUDIO, &[BUILT_IN, STUDIO]), Some(1));
    }

    #[test]
    fn fullscreen_takes_the_other_panels_only_by_request() {
        // The default: fullscreen means THIS screen. Only the menu switch widens it.
        assert_eq!(
            presentation_for(Some(BUILT_IN), true, false),
            Presentation::Windowed { panel: BUILT_IN }
        );
        assert_eq!(
            presentation_for(Some(BUILT_IN), true, true),
            Presentation::FullscreenAll { panel: BUILT_IN }
        );
        // Windowed is windowed whatever the switch says.
        assert_eq!(
            presentation_for(Some(BUILT_IN), false, true),
            Presentation::Windowed { panel: BUILT_IN }
        );
        // No panel yet — show what firmware paints.
        assert_eq!(
            presentation_for(None, true, true),
            Presentation::Windowed { panel: 0 }
        );
    }
}
