#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Break the code on purpose, and report which breakages the checkers notice.

A passing test, proof or model says nothing about what it would catch. This says it directly:
each entry below is a one-line edit that makes the code wrong in a way that matters, applied
alone to a clean file, checked, and reverted. `RED` is the witness catching it. `SURVIVED` is a
hole, named. Ported from virglrs's `harness/sabotage/sweep.py`; the design is in
`docs/design/in-crate-checkers.md`.

    scripts/sabotage-sweep.py [pattern ...]

Entries are matched by substring against their name; with none, every entry runs.

The edits are exact string replacements and every one asserts it matched, so an entry whose target
has been refactored away fails loudly instead of quietly testing nothing -- a sweep that reports
`RED` for an edit it never made is worse than no sweep. Every target is checked up front, before
anything is built, so a refactor costs one message naming all of them.

An entry names the file it edits and the crate directory its witness runs in, both relative to
the limina root, so an entry can target the libkrun fork under `third_party/libkrun` as well as
limina's own crates. Its witness is one of:

- a `cargo test` filter, run in the crate directory;
- `kani:<harness>`, one Kani proof (`cargo kani --harness`);
- `loom:<test>`, one loom model, built with `--cfg loom` in `target/loom` so the loom build and
  the normal one do not evict each other;
- `doc:<filter>`, the doctests under that filter.

Nothing here touches HVF: `LIMINA_HVF_TESTS` is removed from the environment, so the boot tests
skip exactly as they do under a plain `cargo test`.
"""

import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]

# Arguments a crate's tests and proofs need beyond `cargo test` / `cargo kani`. Without them a witness can be compiled out,
# and a filter that matches no test passes -- which the sweep would report as a hole it is not.
TEST_ARGS = {
    'third_party/libkrun/src/devices': ['--features', 'usb'],
}

# (name, file to edit, what to replace, what with, crate directory, witness)
SABOTAGES = [
    (
        'a control-plane header may size a payload past MAX_PAYLOAD',
        'crates/limina-proto/src/lib.rs',
        """        if h.len > MAX_PAYLOAD {
            return Err(HeaderFault::TooLong(h.len));""",
        """        if h.len > MAX_PAYLOAD + 1 {
            return Err(HeaderFault::TooLong(h.len));""",
        'crates/limina-proto',
        'kani:proofs::parse_accepts_exactly_the_bounded_headers',
    ),
    (
        'a control-plane header is accepted without its magic',
        'crates/limina-proto/src/lib.rs',
        """        if b[0..4] != MAGIC {
            return Err(HeaderFault::Magic);""",
        """        if b[0..3] != MAGIC[0..3] {
            return Err(HeaderFault::Magic);""",
        'crates/limina-proto',
        'kani:proofs::parse_accepts_exactly_the_bounded_headers',
    ),
    (
        'a control-plane header decodes its channel from the wrong bytes',
        'crates/limina-proto/src/lib.rs',
        """            channel: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            len: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        };
        if h.len > MAX_PAYLOAD {
            return Err(HeaderFault::TooLong""",
        """            channel: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            len: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        };
        if h.len > MAX_PAYLOAD {
            return Err(HeaderFault::TooLong""",
        'crates/limina-proto',
        'kani:proofs::every_bounded_header_round_trips',
    ),
    (
        'the balloon may grow past the room it was given',
        'crates/limina/src/balloon_policy.rs',
        """                i.current.saturating_add(avail_pages - bound).min(i.room)""",
        """                i.current.saturating_add(avail_pages - bound)""",
        'crates/limina',
        'kani:balloon_policy::proofs::a_target_never_leaves_the_room',
    ),
    (
        'a guest starved of cache is not released',
        'crates/limina/src/balloon_policy.rs',
        """    if p.some_avg10 >= PRESSURE_HIGH || guest_starved(p) {""",
        """    if p.some_avg10 >= PRESSURE_HIGH {""",
        'crates/limina',
        'kani:balloon_policy::proofs::acute_pressure_only_releases',
    ),
    (
        'inflation ignores the guest\'s sustained pressure',
        'crates/limina/src/balloon_policy.rs',
        """        if p.some_avg10 > PRESSURE_LOW || p.some_avg60 > PRESSURE_LOW {
            return Decision::Hold(Hold::NotCalm);""",
        """        if p.some_avg10 > PRESSURE_LOW {
            return Decision::Hold(Hold::NotCalm);""",
        'crates/limina',
        'kani:balloon_policy::proofs::inflation_needs_calm_and_moves_one_step',
    ),
    (
        'an old agent\'s guest is inflated by two steps at once',
        'crates/limina/src/balloon_policy.rs',
        """        let step = if p.mem_free_kib == 0 {
            INFLATE_STEP_PAGES""",
        """        let step = if p.mem_free_kib == 0 {
            2 * INFLATE_STEP_PAGES""",
        'crates/limina',
        'kani:balloon_policy::proofs::inflation_needs_calm_and_moves_one_step',
    ),
    (
        'the pacing clamp forgets the free-list margin',
        'crates/limina/src/balloon_policy.rs',
        """            let headroom = free_pages.saturating_sub(free_margin_pages(i.mode));
            let cap = i.actual_pages.unwrap_or(i.current).saturating_add(headroom);
            let cap_step""",
        """            let headroom = free_pages.saturating_sub(free_margin_pages(i.mode));
            let cap = i.actual_pages.unwrap_or(i.current).saturating_add(free_pages);
            let cap_step""",
        'crates/limina',
        'kani:balloon_policy::proofs::at_host_normal_inflation_stays_within_the_free_margin',
    ),
    (
        "a reported run's start is not rounded up to a whole guest page",
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    let start = (addr + GUEST_PAGE - 1) & !(GUEST_PAGE - 1); // round up""",
        """    let start = addr; // round up""",
        'third_party/libkrun/src/devices',
        'kani:virtio::balloon::device::proofs::every_marked_page_lies_inside_its_run_at_its_gpa',
    ),
    (
        "a reported run's end is rounded up, taking a partly covered page",
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    let end = (addr + len) & !(GUEST_PAGE - 1); // round down""",
        """    let end = (addr + len) | (GUEST_PAGE - 1); // round down""",
        'third_party/libkrun/src/devices',
        'kani:virtio::balloon::device::proofs::every_marked_page_lies_inside_its_run_at_its_gpa',
    ),
    (
        "a guest page is filed under its own base, not its host page's",
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    let base = p & !(host_page - 1);""",
        """    let base = p & !(GUEST_PAGE - 1);""",
        'third_party/libkrun/src/devices',
        'kani:virtio::balloon::device::proofs::every_marked_page_lies_inside_its_run_at_its_gpa',
    ),
    (
        'a guest page is filed in the slot of its offset into the run',
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    let sub = (p - base) / GUEST_PAGE;""",
        """    let sub = (p - addr) / GUEST_PAGE;""",
        'third_party/libkrun/src/devices',
        'kani:virtio::balloon::device::proofs::every_marked_page_lies_inside_its_run_at_its_gpa',
    ),
    (
        "a host page is filed under the GPA of the run's first page",
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    let gpa_base = gpa + (p - addr) as u64 - (p - base) as u64;""",
        """    let gpa_base = gpa + (p - addr) as u64;""",
        'third_party/libkrun/src/devices',
        'kani:virtio::balloon::device::proofs::every_marked_page_lies_inside_its_run_at_its_gpa',
    ),
    (
        'Address Device takes any slot the guest names',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """        if slot_id == 0 || !matches!(self.slots.get(slot_id as usize), Some(Some(_))) {""",
        """        if slot_id == 0 {""",
        'third_party/libkrun/src/devices',
        'address_device_refuses_a_slot_it_never_enabled',
    ),
    (
        'an event ring segment may run past the top of memory',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """        if !EventRing::fits(base, size) {""",
        """        if base == 1 {""",
        'third_party/libkrun/src/devices',
        'an_event_segment_past_the_top_of_memory_is_refused',
    ),
    (
        'a registration asking for no user presence is served',
        'crates/limina/src/fido/request.rs',
        """    if requested_up(&root, 7) == Some(false) {""",
        """    if requested_up(&root, 7) == Some(true) && false {""",
        'crates/limina',
        'registration_refuses_up_false',
    ),
    (
        'an empty allowList is read as a list naming nothing',
        'crates/limina/src/fido/request.rs',
        """        Some(list) if !list.is_empty() => Some(descriptor_ids(list)),""",
        """        Some(list) => Some(descriptor_ids(list)),""",
        'crates/limina',
        'an_allow_list_without_ids_is_not_an_absent_one',
    ),
    (
        'a registration without ES256 on offer is served',
        'crates/limina/src/fido/request.rs',
        """    if !wants_es256 {
        return Err(CTAP2_ERR_UNSUPPORTED_ALGORITHM);""",
        """    if !wants_es256 && false {
        return Err(CTAP2_ERR_UNSUPPORTED_ALGORITHM);""",
        'crates/limina',
        'a_registration_must_offer_es256',
    ),
    (
        'the ring walker returns a Link TRB as work',
        'third_party/libkrun/src/devices/src/usb/xhci/trb.rs',
        """            if trb.trb_type() == trb_type::LINK {""",
        """            if trb.trb_type() == trb_type::LINK && trb.status == 0 {""",
        'third_party/libkrun/src/devices',
        'kani:usb::xhci::trb::proofs::a_step_returns_only_a_published_work_trb',
    ),
    (
        "the ring walker ignores a Link's Toggle Cycle",
        'third_party/libkrun/src/devices/src/usb/xhci/trb.rs',
        """                if trb.toggle_cycle() {
                    self.ccs = !self.ccs;""",
        """                if trb.toggle_cycle() && trb.status == 0 {
                    self.ccs = !self.ccs;""",
        'third_party/libkrun/src/devices',
        'kani:usb::xhci::trb::proofs::a_step_returns_only_a_published_work_trb',
    ),
    (
        'the ring walker skips a TRB after each one it returns',
        'third_party/libkrun/src/devices/src/usb/xhci/trb.rs',
        """            self.ptr = addr.wrapping_add(16);
            return Ok(Some((addr, trb)));""",
        """            self.ptr = addr.wrapping_add(32);
            return Ok(Some((addr, trb)));""",
        'third_party/libkrun/src/devices',
        'kani:usb::xhci::trb::proofs::a_step_returns_only_a_published_work_trb',
    ),
    (
        'the ring walker consumes a TRB the producer has not published',
        'third_party/libkrun/src/devices/src/usb/xhci/trb.rs',
        """            if trb.cycle() != self.ccs {
                // Producer hasn't published""",
        """            if trb.cycle() != self.ccs && trb.status == 0 {
                // Producer hasn't published""",
        'third_party/libkrun/src/devices',
        'kani:usb::xhci::trb::proofs::a_step_returns_only_a_published_work_trb',
    ),
    (
        'the ring walker follows a Link to an unaligned target',
        'third_party/libkrun/src/devices/src/usb/xhci/trb.rs',
        """                self.ptr = trb.link_target();""",
        """                self.ptr = trb.parameter;""",
        'third_party/libkrun/src/devices',
        'kani:usb::xhci::trb::proofs::a_step_returns_only_a_published_work_trb',
    ),
    (
        'Disable Slot leaves the slot in the table',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """                if let Some(s) = self.slots.get_mut(slot as usize) {
                    *s = None;""",
        """                if let Some(s) = self.slots.get_mut(slot as usize) {
                    let _ = s;""",
        'third_party/libkrun/src/devices',
        'every_slot_command_sequence_matches_the_model',
    ),
    (
        'Enable Slot hands out a slot already in use',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """            if self.slots[id].is_none() {""",
        """            if id <= 2 {""",
        'third_party/libkrun/src/devices',
        'every_slot_command_sequence_matches_the_model',
    ),
    (
        'Address Device ignores Block Set Address Request',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """            let new_state = if bsr { ss::DEFAULT } else { ss::ADDRESSED };""",
        """            let new_state = if bsr { ss::ADDRESSED } else { ss::ADDRESSED };""",
        'third_party/libkrun/src/devices',
        'every_slot_command_sequence_matches_the_model',
    ),
    (
        'Reset Device keeps the data endpoints',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """                    // Data endpoints are torn down; any late completion finds no ring.
                    s.eps.clear();""",
        """                    // Data endpoints are torn down; any late completion finds no ring.
                    let _ = &s.eps;""",
        'third_party/libkrun/src/devices',
        'every_slot_command_sequence_matches_the_model',
    ),
    (
        'deconfiguring keeps the data endpoints',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """                s.state = ss::ADDRESSED;
                s.eps.clear();""",
        """                s.state = ss::ADDRESSED;
                let _ = &s.eps;""",
        'third_party/libkrun/src/devices',
        'every_slot_command_sequence_matches_the_model',
    ),
    (
        'a failed release rolls back pages released before it',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """            error!("released-ram: hv_vm_unmap(gpa={gpa:#x}, len={len:#x}) failed: {ret:#x}");
            return false;""",
        """            error!("released-ram: hv_vm_unmap(gpa={gpa:#x}, len={len:#x}) failed: {ret:#x}");
            remove_overlaps(&mut released, gpa, gpa + len);
            return false;""",
        'third_party/libkrun/src/hvf',
        'a_failed_release_keeps_the_pages_released_before_it',
    ),
    (
        'a failed map forgets the ranges after it',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """                for &(s, l) in &ranges[i + 1..] {
                    insert_range(released, s, l);
                }""",
        """                let _ = i;""",
        'third_party/libkrun/src/hvf',
        'a_failed_reclaim_keeps_the_ranges_it_did_not_reach',
    ),
    (
        'a heal maps a coalesced range from one region\'s host',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """                    let e = (start + len).min(r.gpa + r.len);""",
        """                    let e = start + len;""",
        'third_party/libkrun/src/hvf',
        'a_heal_across_adjacent_regions_maps_each_from_its_own_host',
    ),
    (
        'the heal window runs a chunk past a clipped start',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """        let window_end = (aligned + self.chunk).min(region.gpa + region.len);""",
        """        let window_end = (window_start + self.chunk).min(region.gpa + region.len);""",
        'third_party/libkrun/src/hvf',
        'every_release_and_heal_sequence_matches_the_model',
    ),
    (
        'a split range loses its tail',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """        if e > end {
            map.insert(end, e - end);
        }""",
        """        let _ = e > end;""",
        'third_party/libkrun/src/hvf',
        'every_release_and_heal_sequence_matches_the_model',
    ),
    (
        'a heal forgets to take the pages out of the reusable state',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """        if let Err(e) = self.stage2.reuse(host, len) {""",
        """        if let Err(e) = Ok::<(), std::io::Error>(()) {""",
        'third_party/libkrun/src/hvf',
        'every_release_and_heal_sequence_matches_the_model',
    ),
    (
        'a release discards its pages after letting the lock go',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """        insert_range(&mut released, gpa, len);
        let host = Self::host_of(region, gpa);""",
        """        insert_range(&mut released, gpa, len);
        drop(released);
        let host = Self::host_of(region, gpa);""",
        'third_party/libkrun/src/hvf',
        'loom:released_ram::loom_model::a_release_races_a_heal_of_its_window',
    ),
    (
        'a vCPU that lost the heal race falls through to MMIO',
        'third_party/libkrun/src/hvf/src/released_ram.rs',
        """            return FaultOutcome::Retry;""",
        """            return FaultOutcome::NotHandled;""",
        'third_party/libkrun/src/hvf',
        'loom:released_ram::loom_model::two_vcpus_fault_on_one_page',
    ),
    (
        'a partly reported host page is released',
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """            if mask == full {""",
        """            if mask != 0 {""",
        'third_party/libkrun/src/devices',
        'every_run_sequence_releases_exactly_the_whole_free_host_pages',
    ),
    (
        'runs merge across a GPA break',
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """            Some(last) if last.0 + last.2 == host && last.1 + last.2 as u64 == gpa => {""",
        """            Some(last) if last.0 + last.2 == host => {""",
        'third_party/libkrun/src/devices',
        'every_run_sequence_releases_exactly_the_whole_free_host_pages',
    ),
    (
        'pages come out of the merge unsorted',
        'third_party/libkrun/src/devices/src/virtio/balloon/device.rs',
        """    pages.sort_unstable();
    let mut out: Vec<(usize, u64, usize)> = Vec::new();""",
        """    let mut out: Vec<(usize, u64, usize)> = Vec::new();""",
        'third_party/libkrun/src/devices',
        'every_run_sequence_releases_exactly_the_whole_free_host_pages',
    ),
    (
        'a register count sizes its own allocation',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """    let n = bounded_count(r, MAX_VCPU_REGS, "register")?;""",
        """    let n = r.u32()? as usize;""",
        'third_party/libkrun/src/vmm',
        'head_counts_are_bounded_before_they_allocate',
    ),
    (
        'a queue count sizes its own allocation',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """        let q_count = bounded_count(&mut r, MAX_QUEUES, "queue")?;""",
        """        let q_count = r.u32()? as usize;""",
        'third_party/libkrun/src/vmm',
        'head_counts_are_bounded_before_they_allocate',
    ),
    (
        'a snapshot chunk sizes the frame buffers unchecked',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """                    if chunk == 0 || chunk > CHUNK_SIZE as u64 {""",
        """                    if chunk == 0 {""",
        'third_party/libkrun/src/vmm',
        'a_chunk_past_the_writers_is_refused_before_it_sizes_a_frame',
    ),
    (
        'a snapshot region may wrap the address space',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """                    if gpa.checked_add(len).is_none() {""",
        """                    if gpa.checked_add(len).is_none() && false {""",
        'third_party/libkrun/src/vmm',
        'a_region_past_the_top_of_the_address_space_is_refused',
    ),
    (
        'the head CRC is not enforced',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """    if crc32(raw.slice(0, head_end)) != stored && !cfg!(fuzzing) {""",
        """    if crc32(raw.slice(0, head_end)) != stored && cfg!(fuzzing) {""",
        'third_party/libkrun/src/vmm',
        'snapshot_rejects_head_corruption',
    ),
    (
        'a frame CRC is not enforced',
        'third_party/libkrun/src/vmm/src/snapshot.rs',
        """                            if crc32(data) != f.crc && !cfg!(fuzzing) {""",
        """                            if crc32(data) != f.crc && cfg!(fuzzing) {""",
        'third_party/libkrun/src/vmm',
        'snapshot_rejects_frame_corruption',
    ),
    (
        'normalization flips without draining what it pressed',
        'crates/limina-input/src/ledger.rs',
        """        self.release_all_held(out);
        self.remap.normalize = on;""",
        """        self.remap.normalize = on;""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'a key press goes out unhealed',
        'crates/limina-input/src/ledger.rs',
        """        if down {
            self.sync_modifiers(flags, None, out);
        }
        self.sync_capslock(flags, out);
        self.emit_key(macos_keycode, down, out);""",
        """        self.sync_capslock(flags, out);
        self.emit_key(macos_keycode, down, out);""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'a modifier is healed in the fixed order, ahead of the ones it is pressed under',
        'crates/limina-input/src/ledger.rs',
        """        self.sync_modifiers(flags, Some(macos_keycode), out);""",
        """        self.sync_modifiers(flags, None, out);""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'Caps Lock is tapped before the modifiers heal',
        'crates/limina-input/src/ledger.rs',
        """        self.sync_modifiers(flags, Some(macos_keycode), out);
        self.sync_capslock(flags, out);""",
        """        self.sync_capslock(flags, out);
        self.sync_modifiers(flags, Some(macos_keycode), out);""",
        'crates/limina-input',
        'a_caps_lock_tap_waits_for_the_modifiers_to_heal',
    ),
    (
        'a focus loss forgets the held keys without releasing them',
        'crates/limina-input/src/ledger.rs',
        """        for &macos_keycode in self.mods.iter().chain(self.keys.iter()) {""",
        """        for &macos_keycode in self.mods.iter() {""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'the end of a capture keeps believing its modifiers',
        'crates/limina-input/src/ledger.rs',
        """        self.mods.clear();
        self.flush_aux(out);""",
        """        self.flush_aux(out);""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'an aux press is not tracked',
        'crates/limina-input/src/ledger.rs',
        """            self.aux.insert(code);""",
        """            let _ = code;""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'Option normalizes to Alt',
        'crates/limina-input/src/keymap.rs',
        """            HID_LEFT_OPTION => KEY_LEFTMETA,""",
        """            HID_LEFT_OPTION => KEY_LEFTALT,""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        "macOS's modifier setting is read back uninverted",
        'crates/limina-input/src/keymap.rs',
        """            map.physical[to] = src;""",
        """            map.physical[to] = dst;""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'Caps Lock is tapped on every event',
        'crates/limina-input/src/keymap.rs',
        """        if led_on == self.guest_on {""",
        """        if false {""",
        'crates/limina-input',
        'every_keyboard_sequence_keeps_the_guest_in_step',
    ),
    (
        'the grab is taken over a Space that is not on screen',
        'crates/limina/src/window/grab_policy.rs',
        """    let duties = fit::edge_duties(s.fullscreen_and_key && s.space_visible, s.grab_enabled);""",
        """    let duties = fit::edge_duties(s.fullscreen_and_key, s.grab_enabled);""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'a click that closes a menu takes the grab',
        'crates/limina/src/window/grab_policy.rs',
        """        if s.menu_open {
            return out;
        }""",
        """        if false {
            return out;
        }""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'a click off the guest does not stand the re-grab down',
        'crates/limina/src/window/grab_policy.rs',
        """            st.user_released = true;""",
        """            st.user_released = false;""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'leaving the picture never ends an explicit release',
        'crates/limina/src/window/grab_policy.rs',
        """    if !fit::point_in_fit(s.pos.0, s.pos.1, s.fit) && st.rearm() {""",
        """    if !fit::point_in_fit(s.pos.0, s.pos.1, s.fit) && false {""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'the dwell ignores the explicit-release latch',
        'crates/limina/src/window/grab_policy.rs',
        """    if !st.user_released
        && !s.menu_open""",
        """    if !s.menu_open""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'the dwell retakes the pointer under an open menu',
        'crates/limina/src/window/grab_policy.rs',
        """        && !s.menu_open
        && fit::may_regrab""",
        """        && fit::may_regrab""",
        'crates/limina',
        'every_free_pointer_sequence_grabs_exactly_when_the_rules_say',
    ),
    (
        'a drag against an edge releases the grab',
        'crates/limina/src/window/grab_policy.rs',
        """    if s.buttons_down {""",
        """    if false {""",
        'crates/limina',
        'every_edge_press_sequence_releases_exactly_when_the_rules_say',
    ),
    (
        'an edge press releases a hard grab',
        'crates/limina/src/window/grab_policy.rs',
        """    if !matches!(mode, GrabMode::Auto) || s.hold <= 0.0 || !s.fullscreen {""",
        """    if !matches!(mode, GrabMode::Auto | GrabMode::Hard) || s.hold <= 0.0 || !s.fullscreen {""",
        'crates/limina',
        'every_edge_press_sequence_releases_exactly_when_the_rules_say',
    ),
    (
        'a press on a new edge inherits the old charge',
        'crates/limina/src/window/grab_policy.rs',
        """    if st.edge.replace(edge) != Some(edge) {""",
        """    if st.edge.replace(edge).is_none() {""",
        'crates/limina',
        'every_edge_press_sequence_releases_exactly_when_the_rules_say',
    ),
    (
        'a side press releases onto no display',
        'crates/limina/src/window/grab_policy.rs',
        """            reachable(p).then_some(Release::Out(p))""",
        """            Some(Release::Out(p))""",
        'crates/limina',
        'every_edge_press_sequence_releases_exactly_when_the_rules_say',
    ),
    (
        'a dead edge keeps its full charge',
        'crates/limina/src/window/grab_policy.rs',
        """        None => st.charge.lapse(),""",
        """        None => {}""",
        'crates/limina',
        'every_edge_press_sequence_releases_exactly_when_the_rules_say',
    ),
    (
        'an agent is registered only after its greeting',
        'crates/limina/src/control.rs',
        """    peers.lock().unwrap().push(peer.clone());
    // A late joiner needs the CURRENT host clipboard, not just the next change.
    if peer.has_cap("clipboard")
        && let Some(offer) = clipboard.initial_offer()
    {
        let _ = peer.send(&offer, CHANNEL_CLIPBOARD);
    }""",
        """    // A late joiner needs the CURRENT host clipboard, not just the next change.
    if peer.has_cap("clipboard")
        && let Some(offer) = clipboard.initial_offer()
    {
        let _ = peer.send(&offer, CHANNEL_CLIPBOARD);
    }
    peers.lock().unwrap().push(peer.clone());""",
        'crates/limina',
        'loom:control::loom_model::an_agent_joins_as_a_host_copy_lands_and_is_offered',
    ),
    (
        'a greeting reads the pasteboard before taking the offer',
        'crates/limina/src/clipboard.rs',
        """        let mut host = self.host.lock().unwrap();
        // No change-count bump involved: read whatever is there right now.
        let text = self.pasteboard.lock().unwrap().current_text()?;""",
        """        // No change-count bump involved: read whatever is there right now.
        let text = self.pasteboard.lock().unwrap().current_text()?;
        let mut host = self.host.lock().unwrap();""",
        'crates/limina',
        'loom:control::loom_model::an_agent_joins_as_a_host_copy_lands_and_is_offered',
    ),
    (
        'an offer of unchanged text retires the serial peers hold',
        'crates/limina/src/clipboard.rs',
        """        if self.text.as_deref() != Some(text.as_str()) {""",
        """        if true {""",
        'crates/limina',
        'loom:control::loom_model::an_agent_joins_as_a_host_copy_is_offered',
    ),
    (
        'a band disarm does not claim the move first',
        'third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs',
        """        if self
            .word
            .compare_exchange(seen, hold | DISARMING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        if os.set_timeshare(port) {""",
        """        if os.set_timeshare(port) {""",
        'third_party/libkrun/src/vmm',
        'loom:macos::vcpu_sched::loom_model::the_sampler_and_the_guard_decide_at_once',
    ),
    (
        'a band disarm acts on the hold it finds, not the one it judged',
        'third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs',
        """    fn disarm<O: BandOs>(&self, os: &O, port: u32, seen: u64) -> bool {
        if seen & PHASE != IN {""",
        """    fn disarm<O: BandOs>(&self, os: &O, port: u32, _seen: u64) -> bool {
        let seen = self.load();
        if seen & PHASE != IN {""",
        'third_party/libkrun/src/vmm',
        'loom:macos::vcpu_sched::loom_model::the_sampler_and_the_guard_decide_at_once',
    ),
    (
        'a new band hold is not told apart from the last',
        'third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs',
        """        let hold = (seen & !PHASE) + (PHASE + 1);""",
        """        let hold = seen & !PHASE;""",
        'third_party/libkrun/src/vmm',
        'loom:macos::vcpu_sched::loom_model::the_sampler_and_the_guard_decide_at_once',
    ),
    (
        'a power transition wakes its waiters without recording itself',
        'third_party/libkrun/src/devices/src/legacy/power_watch.rs',
        """        *generation = generation.wrapping_add(1);""",
        """        let _ = generation.wrapping_add(1);""",
        'third_party/libkrun/src/devices',
        'loom:legacy::power_watch::loom_model::a_transition_is_never_slept_through',
    ),
    (
        'a vCPU in a WFx wait drops a Snapshot',
        'third_party/libkrun/src/vmm/src/macos/vstate.rs',
        """        (VcpuEvent::Snapshot(_), P::Parked) => A::Snapshot,""",
        """        (VcpuEvent::Snapshot(_), P::Parked) => A::Snapshot,
        (VcpuEvent::Snapshot(_), P::WfxWait) => A::Ignore,""",
        'third_party/libkrun/src/vmm',
        'macos::vstate::tests::every_park_site_answers_the_coordinator',
    ),
    (
        'a paused vCPU parks again for a Snapshot',
        'third_party/libkrun/src/vmm/src/macos/vstate.rs',
        """        (VcpuEvent::Snapshot(_), P::Parked) => A::Snapshot,""",
        """        (VcpuEvent::Snapshot(_), P::Parked) => A::SnapshotAndPark,""",
        'third_party/libkrun/src/vmm',
        'macos::vstate::tests::every_park_site_answers_the_coordinator',
    ),
    (
        'a host sleep pulses a guest whose earlier pulse is unresolved',
        'crates/limina-vmm/src/power.rs',
        """            GuestSleep::Awake if self.ours => SleepAction::DontPulse,
""",
        """""",
        'crates/limina-vmm',
        'every_host_sleep_sequence_wakes_exactly_our_suspends',
    ),
    (
        'a guest still suspending at the wake is left unwatched',
        'crates/limina-vmm/src/power.rs',
        """        if !self.ours {
            return WakeAction::LeaveAlone;""",
        """        if !self.ours || guest == GuestSleep::Suspending {
            return WakeAction::LeaveAlone;""",
        'crates/limina-vmm',
        'every_host_sleep_sequence_wakes_exactly_our_suspends',
    ),
    (
        'a watch stops on a suspend still under way',
        'crates/limina-vmm/src/power.rs',
        """            GuestSleep::Suspending => {
                watch.seen_suspending = true;
                WatchAction::Keep
            }""",
        """            GuestSleep::Suspending => {
                watch.seen_suspending = true;
                WatchAction::Stop
            }""",
        'crates/limina-vmm',
        'every_host_sleep_sequence_wakes_exactly_our_suspends',
    ),
    (
        "a host sleep claims the user's own suspend",
        'crates/limina-vmm/src/power.rs',
        """            GuestSleep::Suspending | GuestSleep::Asleep => SleepAction::DontPulse,
        }
    }

    fn on_did_wake""",
        """            GuestSleep::Suspending | GuestSleep::Asleep => {
                self.ours = true;
                SleepAction::DontPulse
            }
        }
    }

    fn on_did_wake""",
        'crates/limina-vmm',
        'every_host_sleep_sequence_wakes_exactly_our_suspends',
    ),
    (
        "a swapped-out worker's reader still moves the slots",
        'crates/limina/src/window/present.rs',
        """    if s.reader_epoch != epoch {""",
        """    if s.reader_epoch < epoch {""",
        'crates/limina',
        'every_handoff_order_shows_what_the_guest_presents',
    ),
    (
        'a swap leaves every reader current',
        'crates/limina/src/window/present.rs',
        """        self.reader_epoch += 1;
""",
        """""",
        'crates/limina',
        'every_handoff_order_shows_what_the_guest_presents',
    ),
    (
        'a released id stays in the frame cache',
        'crates/limina/src/window/present.rs',
        """        self.released.push(id);
""",
        """""",
        'crates/limina',
        'every_handoff_order_shows_what_the_guest_presents',
    ),
    (
        'a swap leaves the dead worker in the frame cache',
        'crates/limina/src/window/present.rs',
        """        self.released.extend(self.map.keys().copied());
""",
        """""",
        'crates/limina',
        'every_handoff_order_shows_what_the_guest_presents',
    ),
    (
        'a frame that misses never asks for its surface again',
        'crates/limina/src/window/guestwindow.rs',
        """        if ask {
            ask_resurface(id);""",
        """        if false && ask {
            ask_resurface(id);""",
        'crates/limina',
        'every_handoff_order_shows_what_the_guest_presents',
    ),
    (
        "a capture release leaves the policy holding",
        'crates/limina/src/window/grab_policy.rs',
        """    if captured {
        st.stop_holding();
    }
    !captured""",
        """    !captured""",
        'crates/limina',
        'every_ownership_sequence_keeps_the_grab_terms_straight',
    ),
    (
        'the tick keeps a grab whose window left the screen',
        'crates/limina/src/window/grab_policy.rs',
        """    if must_drop_grab(captured, capture_owner(facts, capture_slot)) {""",
        """    if false && must_drop_grab(captured, capture_owner(facts, capture_slot)) {""",
        'crates/limina',
        'every_ownership_sequence_keeps_the_grab_terms_straight',
    ),
    (
        'the tap keeps a policy grab outside fullscreen',
        'crates/limina/src/window/grab_policy.rs',
        """    } else if fullscreen_exit_releases(capture_tier(captured, st), &primary_facts(facts)) {""",
        """    } else if false && fullscreen_exit_releases(capture_tier(captured, st), &primary_facts(facts)) {""",
        'crates/limina',
        'every_ownership_sequence_keeps_the_grab_terms_straight',
    ),
    (
        'Cmd-Ctrl-G grabs while another app has the keyboard',
        'crates/limina/src/window/grab_policy.rs',
        """    if !hard && !is_key {
        return ComboAction::PassThrough;""",
        """    if false && !hard && !is_key {
        return ComboAction::PassThrough;""",
        'crates/limina',
        'every_ownership_sequence_keeps_the_grab_terms_straight',
    ),
    (
        'a promotion is taken as a toggle',
        'crates/limina/src/window/grab_policy.rs',
        """    if captured != hard {
        ComboAction::Promote""",
        """    if captured == hard {
        ComboAction::Promote""",
        'crates/limina',
        'every_ownership_sequence_keeps_the_grab_terms_straight',
    ),
    (
        'a control TD posts a Status event nobody asked for',
        'third_party/libkrun/src/devices/src/usb/xhci/engine.rs',
        """                if ev.status_ioc {""",
        """                if true || ev.status_ioc {""",
        'third_party/libkrun/src/devices',
        'ep0_status_without_ioc_posts_no_event',
    ),
    (
        'a GPU payload trusts a backing count past its end',
        'third_party/libkrun/src/devices/src/virtio/gpu_snapshot.rs',
        """        (n <= (self.data.len() - self.pos) / each).then_some(n)""",
        """        Some(n)""",
        'third_party/libkrun/src/devices',
        'gpu_snapshot_payload_refuses_counts_and_lengths_past_its_end',
    ),
    (
        'a GPU payload adds a length to its position unchecked',
        'third_party/libkrun/src/devices/src/virtio/gpu_snapshot.rs',
        """        let s = self.data.get(self.pos..self.pos.checked_add(n)?)?;""",
        """        let s = self.data.get(self.pos..self.pos + n)?;""",
        'third_party/libkrun/src/devices',
        'gpu_snapshot_payload_refuses_counts_and_lengths_past_its_end',
    ),
]


def run(cmd, cwd=ROOT, timeout=None, env=None):
    """Run a command, killing the whole process group if it outstays `timeout`.

    The group, not the child: `cargo test` spawns the test binary and `cargo kani` spawns `cbmc`,
    and either can outlive a killed parent. `cbmc` in particular has no memory cap of its own and
    has been seen holding gigabytes with no verdict in sight.

    A timeout is reported, never raised. It is a legitimate verdict here -- see `main`.
    """
    env = dict(env or os.environ)
    env.pop('LIMINA_HVF_TESTS', None)
    proc = subprocess.Popen(
        cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=True, env=env,
    )
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        proc.communicate()
        return SimpleNamespace(returncode=124, stdout='', stderr='', timed_out=True)
    return SimpleNamespace(returncode=proc.returncode, stdout=out, stderr=err, timed_out=False)


def command(filt, crate):
    """What runs an entry's witness, as `(argv, env)`: one Kani proof, one loom model, the
    doctests, or `cargo test` under a filter. `env` is None where the sweep's own is used."""
    if filt.startswith('kani:'):
        # `--exact`, or a harness named as a prefix of another would run both. Stubbing on,
        # because proofs stand in for what Kani cannot model (`Instant::now`, for one).
        return ['cargo', 'kani'] + TEST_ARGS.get(crate, []) + [
            '-Z', 'stubbing', '-Z', 'unstable-options', '--harness-timeout', '10m', '--exact',
            '--harness', filt[len('kani:'):]], None
    if filt.startswith('loom:'):
        env = dict(os.environ, RUSTFLAGS='--cfg loom', CARGO_TARGET_DIR=str(ROOT / 'target/loom'))
        # A crate with no library target (limina itself) keeps its models in the binary.
        target = '--lib' if (ROOT / crate / 'src/lib.rs').exists() else '--bins'
        return ['cargo', 'test', target, filt[len('loom:'):]], env
    if filt.startswith('doc:'):
        return ['cargo', 'test', '--doc', filt[len('doc:'):]], None
    return ['cargo', 'test'] + TEST_ARGS.get(crate, []) + ([filt] if filt else []), None


def separate(filt):
    """Whether an entry's witness is one `cargo test` does not run, and so needs its own
    baseline and its own clock."""
    return filt.startswith(('kani:', 'loom:', 'doc:'))


def uncommitted(rel):
    """The file's uncommitted changes, if any, asked of whichever repository holds it."""
    path = ROOT / rel
    return run(['git', 'status', '--porcelain', '--', path.name], cwd=path.parent).stdout.strip()


def main():
    patterns = sys.argv[1:]
    chosen = [s for s in SABOTAGES if not patterns or any(p in s[0] for p in patterns)]
    if not chosen:
        sys.exit('no sabotage matches %r' % patterns)

    # Only the files the sweep edits have to be clean. This tree routinely holds untracked disk
    # images and scratch, so a whole-tree check would refuse every run.
    dirty = sorted({r for _, r, *_ in chosen if uncommitted(r)})
    if dirty:
        sys.exit('uncommitted changes in files the sweep edits; commit or stash them:\n  '
                 + '\n  '.join(dirty))

    stale = [(n, r) for n, r, old, *_ in SABOTAGES if old not in (ROOT / r).read_text()]
    if stale:
        sys.exit(
            'sabotage targets no longer in the tree -- fix or retire each:\n'
            + '\n'.join('  %s\n    %s' % (n, r) for n, r in stale)
        )

    # Each crate's tests are the baseline for the entries witnessed by a test in it, and set the
    # clock those entries run against. Derived from the clean run rather than fixed, so a slow
    # machine is not called a hang and a fast one still catches a wedge quickly.
    budget = {}
    for crate in sorted({c for *_, c, f in chosen if not separate(f)}):
        began = time.monotonic()
        if run(['cargo', 'test'] + TEST_ARGS.get(crate, []), cwd=ROOT / crate).returncode != 0:
            sys.exit('the tests in %s do not pass before any sabotage; fix that first' % crate)
        budget[crate, None] = max(180.0, (time.monotonic() - began) * 8)
    # A proof or a model is its own baseline: `cargo test` never runs it, so one already failing
    # on the clean tree would read every sabotage aimed at it as caught. It is its own clock too.
    for crate, filt in sorted({(c, f) for *_, c, f in chosen if separate(f)}):
        began = time.monotonic()
        argv, env = command(filt, crate)
        if run(argv, cwd=ROOT / crate, env=env).returncode != 0:
            sys.exit('%s in %s does not pass before any sabotage; fix that first' % (filt, crate))
        budget[crate, filt] = max(180.0, (time.monotonic() - began) * 3)

    holes = []
    for name, rel, old, new, crate, filt in chosen:
        path = ROOT / rel
        original = path.read_text()
        assert old in original, 'sabotage %r no longer matches %s' % (name, rel)
        path.write_text(original.replace(old, new, 1))
        try:
            argv, env = command(filt, crate)
            clock = budget[crate, filt if separate(filt) else None]
            r = run(argv, cwd=ROOT / crate, timeout=clock, env=env)
        finally:
            path.write_text(original)
        if r.timed_out:
            print('RED       %-62s the witness hung: nothing on that path ends on its own' % name)
            continue
        # A sabotaged tree that does not build fails exactly as a catch does, and is one only if
        # the compiler refused the defect rather than the sabotage's own spelling. An error
        # inside the replacement text is the entry being uncompilable; an error anywhere else is
        # the type system refusing what the edit broke.
        if 'error: could not compile' in r.stderr or 'error[E' in r.stderr:
            start = original.index(old)
            first = original.count('\n', 0, start) + 1
            last = first + new.count('\n')
            at = re.findall(r'^\s*--> (\S+?):(\d+):\d+', r.stderr, re.M)
            here = os.path.relpath(path, ROOT / crate)
            if any(f in (rel, here) and first <= int(n) <= last for f, n in at):
                holes.append(name)
                print('BROKEN    %-62s the sabotage does not compile as written' % name)
            else:
                where = ', '.join(sorted({'%s:%s' % a for a in at})[:2]) or 'the build'
                print('RED       %-62s the build refused it: %s' % (name, where))
            continue
        if r.returncode == 0:
            holes.append(name)
            print('SURVIVED  %s' % name)
            continue
        named = re.findall(r"^    (\S+::\S+)$", r.stdout, re.M)
        if not named and filt.startswith('doc:'):
            named = re.findall(r"^    (\S+\.rs - \S+ \(line \d+\))$", r.stdout, re.M)
        if not named and filt.startswith('kani:'):
            named = ['%s: %s' % (filt, d) for d in
                     re.findall(r'Failed Checks: (.*)', r.stdout)[:1]]
        if not named:
            named = re.findall(r"^thread '(\S+::\S+)'", r.stdout + r.stderr, re.M)[:1]
        witness = ', '.join(sorted(set(named))[:2]) if named else 'the witness failed'
        more = len(set(named)) - 2
        print('RED       %-62s %s%s' % (name, witness, ' +%d more' % more if more > 0 else ''))

    print('\n%d of %d caught' % (len(chosen) - len(holes), len(chosen)))
    return 1 if holes else 0


if __name__ == '__main__':
    sys.exit(main())
