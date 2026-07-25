#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Re-verify that every xHCI suspend/resume fix is load-bearing.

A guard test that passes is only evidence if it would FAIL without its fix. This reverts each fix
in the vendored libkrun tree one at a time, runs the test that is supposed to guard it, and
requires it to fail — then restores the file. Green output means every fix is still pinned by a
test; a `NOT RED` line means a test has stopped guarding what it claims to.

Run it after touching `third_party/libkrun/src/devices/src/usb/xhci/` — the L0 tests are fast, so
the whole sweep is a few seconds. The L2 guards (`usb_device_survives_inplace_s2idle`,
`managed_vm_suspends_and_resumes`) are boot tests and are NOT covered here; their RED evidence is
recorded in docs/design/usb-xhci-snapshot/README.md §4.

    python3 scripts/xhci-red-check.py
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "third_party/libkrun"
DEV = ROOT / "src/devices/src/usb/xhci/device.rs"
ENG = ROOT / "src/devices/src/usb/xhci/engine.rs"

# (label, file, fixed source, reverted source, [tests that must fail without it])
CASES = [
    (
        "CRCR stop/abort must not rebase the ring",
        DEV,
        """        if val & (CRCR_CS | CRCR_CA) != 0 {
            self.crcr_stop_write = true;""",
        """        if false {
            self.crcr_stop_write = true;""",
        [
            "crcr_abort_write_does_not_rebase_the_ring",
            "crcr_stop_write_keeps_the_ring_and_posts_no_event",
            "command_ring_abort_posts_a_command_ring_stopped_event",
        ],
    ),
    (
        "the paired CRCR high-dword write is suppressed",
        DEV,
        """    fn write_crcr_hi(&mut self, val: u32) {
        if self.crcr_stop_write {
            return;
        }""",
        """    fn write_crcr_hi(&mut self, val: u32) {
        if false {
            return;
        }""",
        ["crcr_abort_write_does_not_rebase_the_ring"],
    ),
    (
        "ERSTBA rebuilds the event ring only on a real change",
        DEV,
        """        let changed = (new & !0x3f) != (self.erstba & !0x3f);
        self.erstba = new;
        if changed {""",
        """        let changed = (new & !0x3f) != (self.erstba & !0x3f);
        self.erstba = new;
        if true || changed {""",
        ["erstba_rewrite_with_the_same_base_keeps_the_producer"],
    ),
    (
        "the run-edge port scan is idempotent",
        ENG,
        "            if self.port_populated(idx) && !self.port_connected(idx) {",
        "            if self.port_populated(idx) {",
        ["run_edge_does_not_relatch_an_already_connected_port"],
    ),
    (
        "PLC is latched when the link reaches U0",
        DEV,
        """            if is_u0 && !was_u0 {
                p |= PORTSC_PLC;
                link_reached_u0 = true;
            }""",
        """            if false {
                p |= PORTSC_PLC;
                link_reached_u0 = true;
            }""",
        ["link_state_write_to_u0_latches_plc_and_posts_a_port_event"],
    ),
    (
        "CSS/CRS self-clear",
        DEV,
        "const CMD_STORE_MASK: u32 = !(CMD_HCRST | CMD_LHCRST | CMD_CSS | CMD_CRS);",
        "const CMD_STORE_MASK: u32 = !(CMD_HCRST | CMD_LHCRST);",
        ["css_and_crs_are_self_clearing"],
    ),
    (
        "a null command ring is never walked",
        ENG,
        """        if self.cmd_ring.is_none() && self.crcr_ptr() == 0 {""",
        """        if false {""",
        ["command_doorbell_with_no_ring_is_ignored"],
    ),
    # The snapshot carry: break each field individually and require the round-trip to notice.
    # This is what keeps `XhciState` honest as `XhciDevice` grows.
    *[
        (f"snapshot carries {label}", DEV, fixed, broken, ["save_state_round_trips_a_lived_in_controller"])
        for label, fixed, broken in [
            ("usbcmd", "        self.usbcmd = s.usbcmd;", "        self.usbcmd = 0;"),
            ("usbsts", "        self.usbsts = s.usbsts & !STS_CNR;", "        self.usbsts = STS_HCH;"),
            ("dnctrl", "        self.dnctrl = s.dnctrl;", "        self.dnctrl = 0;"),
            ("crcr", "        self.crcr = s.crcr;", "        self.crcr = 0;"),
            ("dcbaap", "        self.dcbaap = s.dcbaap;", "        self.dcbaap = 0;"),
            ("config", "        self.config = s.config;", "        self.config = 0;"),
            ("iman", "        self.iman = s.iman;", "        self.iman = 0;"),
            ("imod", "        self.imod = s.imod;", "        self.imod = 0;"),
            ("erstsz", "        self.erstsz = s.erstsz;", "        self.erstsz = 0;"),
            ("erstba", "        self.erstba = s.erstba;", "        self.erstba = 0;"),
            ("erdp", "        self.erdp = s.erdp;", "        self.erdp = 0;"),
            ("next_address", "        self.next_address = s.next_address;", "        self.next_address = 1;"),
            (
                "crcr_stop_write",
                "        self.crcr_stop_write = s.crcr_stop_write;",
                "        self.crcr_stop_write = false;",
            ),
            ("work run_started", "            run_started: s.work_run_started,", "            run_started: false,"),
            ("work cmd_doorbell", "            cmd_doorbell: s.work_cmd_doorbell,", "            cmd_doorbell: false,"),
            ("work cmd_abort", "            cmd_abort: s.work_cmd_abort,", "            cmd_abort: false,"),
            (
                "work ep_doorbells",
                "            ep_doorbells: s.work_ep_doorbells.clone(),",
                "            ep_doorbells: Vec::new(),",
            ),
            (
                "work port_events",
                "            port_events: s.work_port_events.clone(),",
                "            port_events: Vec::new(),",
            ),
            ("per-port PORTSC", "                self.ports[idx] = p.portsc;", "                self.ports[idx] = PORTSC_DEFAULT;"),
            ("the slot id", "            let id = st.slot_id as usize;", "            let id = st.slot_id as usize + 1;"),
            (
                "the command walker",
                "        self.cmd_ring = s.cmd_ring.map(|(p, c)| RingWalker::new(p, c));",
                "        self.cmd_ring = None;",
            ),
            (
                "the event-ring producer",
                "        self.event_ring = s.event_ring.as_ref().map(EventRing::from_state);",
                "        self.event_ring = s.event_ring.as_ref().map(|e| EventRing::new(e.seg_base, e.seg_size));",
            ),
            ("slot port", "                port: st.port,", "                port: 0,"),
            ("slot address", "                address: st.address,", "                address: 0,"),
            ("slot state", "                state: st.state,", "                state: 0,"),
            ("slot config_value", "                config_value: st.config_value,", "                config_value: 0,"),
            (
                "the EP0 ring",
                "                ep0_ring: st.ep0_ring.map(|(p, c)| RingWalker::new(p, c)),",
                "                ep0_ring: None,",
            ),
            (
                "the EP0 ring cycle bit",
                "                ep0_ring: st.ep0_ring.map(|(p, c)| RingWalker::new(p, c)),",
                "                ep0_ring: st.ep0_ring.map(|(p, c)| RingWalker::new(p, !c)),",
            ),
            (
                "the endpoint ring pointer",
                "                                ring: RingWalker::new(e.ring.0, e.ring.1),",
                "                                ring: RingWalker::new(0, e.ring.1),",
            ),
            (
                "the endpoint ring cycle bit",
                "                                ring: RingWalker::new(e.ring.0, e.ring.1),",
                "                                ring: RingWalker::new(e.ring.0, !e.ring.1),",
            ),
            # An inversion is the easy break; hardcoding the value a fresh ring happens to start
            # with is the realistic one, and only catchable because the capture holds BOTH
            # polarities. Same for the EP0 ring below.
            (
                "the endpoint ring cycle bit (vs a hardcoded default)",
                "                                ring: RingWalker::new(e.ring.0, e.ring.1),",
                "                                ring: RingWalker::new(e.ring.0, true),",
            ),
            ("endpoint dci", "                            e.dci,", "                            e.dci ^ 1,"),
            ("endpoint dir_in", "                                dir_in: e.dir_in,", "                                dir_in: true,"),
            ("endpoint ep_type", "                                ep_type: e.ep_type,", "                                ep_type: 0,"),
            ("endpoint state", "                                state: e.state,", "                                state: 1,"),
            ("endpoint generation", "                                generation: e.generation,", "                                generation: 0,"),
        ]
    ],
    (
        "an abort cancels a doorbell queued before it",
        ENG,
        """            self.work.cmd_doorbell = false;
            self.post_command_ring_stopped(mem);""",
        """            self.post_command_ring_stopped(mem);""",
        ["an_abort_cancels_a_doorbell_queued_before_it"],
    ),
    (
        "restore never leaves CNR set",
        DEV,
        "        self.usbsts = s.usbsts & !STS_CNR;",
        "        self.usbsts = s.usbsts;",
        ["restore_never_leaves_the_controller_not_ready"],
    ),
    (
        "a changed port gadget is reconciled",
        DEV,
        "            if now == p.model_id {",
        "            if true {",
        ["restore_reconciles_a_port_whose_gadget_changed"],
    ),
]


def run(tests):
    """Run `tests`; return `(compiled, passed)`.

    `compiled` matters as much as `passed`: a revert that fails to BUILD produces no passing test
    either, which would look exactly like RED while proving nothing. Cargo prints a `running N
    tests` header only once a binary actually runs, so its absence is the signal — more robust than
    grepping for `error[`, which misses plain `error: expected ...` syntax failures.
    """
    proc = subprocess.run(
        ["cargo", "test", "--features", "usb", "--"] + tests,
        cwd=ROOT / "src/devices",
        capture_output=True,
        text=True,
    )
    out = proc.stdout + proc.stderr
    if "running " not in out:
        return False, set()
    return True, {t for t in tests if f"{t} ... ok" in out}


def main():
    # Baseline first. Every named test must PASS with the tree untouched — otherwise a test that was
    # renamed away, deleted, or is already failing matches nothing, reports RED forever, and guards
    # nothing. This is the check that keeps the sweep honest as the code moves.
    named = sorted({t for case in CASES for t in case[4]})
    compiled, green = run(named)
    if not compiled:
        print("BASELINE the tree does not compile un-reverted — fix that first")
        return 1
    missing = [t for t in named if t not in green]
    if missing:
        print(f"BASELINE {len(missing)} guard test(s) do not pass un-reverted: {missing}")
        print("         (renamed? deleted? already failing? a RED verdict on these means nothing)")
        return 1
    print(f"BASELINE all {len(named)} guard tests pass un-reverted")

    failures = []
    for label, path, fixed, reverted, tests in CASES:
        src = path.read_text()
        if fixed not in src:
            print(f"SKIP     {label}: anchor no longer present — update this script")
            failures.append(label)
            continue
        path.write_text(src.replace(fixed, reverted, 1))
        try:
            compiled, still_green = run(tests)
        finally:
            path.write_text(src)
        if not compiled:
            print(f"NO BUILD {label}: the reverted source does not compile — this row proves nothing")
            failures.append(label)
        elif still_green:
            print(f"NOT RED  {label}: {sorted(still_green)} passed WITHOUT the fix")
            failures.append(label)
        else:
            print(f"RED      {label}")

    print()
    if failures:
        print(f"{len(failures)} fix(es) are not pinned by a test: {failures}")
        return 1
    print(f"all {len(CASES)} fixes are pinned by a failing-without-them test")
    return 0


sys.exit(main())
