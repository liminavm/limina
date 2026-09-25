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


def command(filt):
    """What runs an entry's witness, as `(argv, env)`: one Kani proof, one loom model, the
    doctests, or `cargo test` under a filter. `env` is None where the sweep's own is used."""
    if filt.startswith('kani:'):
        # `--exact`, or a harness named as a prefix of another would run both. Stubbing on,
        # because proofs stand in for what Kani cannot model (`Instant::now`, for one).
        return ['cargo', 'kani', '-Z', 'stubbing', '-Z', 'unstable-options',
                '--harness-timeout', '10m', '--exact', '--harness', filt[len('kani:'):]], None
    if filt.startswith('loom:'):
        env = dict(os.environ, RUSTFLAGS='--cfg loom', CARGO_TARGET_DIR=str(ROOT / 'target/loom'))
        return ['cargo', 'test', '--lib', filt[len('loom:'):]], env
    if filt.startswith('doc:'):
        return ['cargo', 'test', '--doc', filt[len('doc:'):]], None
    return ['cargo', 'test'] + ([filt] if filt else []), None


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
        if run(['cargo', 'test'], cwd=ROOT / crate).returncode != 0:
            sys.exit('the tests in %s do not pass before any sabotage; fix that first' % crate)
        budget[crate, None] = max(180.0, (time.monotonic() - began) * 8)
    # A proof or a model is its own baseline: `cargo test` never runs it, so one already failing
    # on the clean tree would read every sabotage aimed at it as caught. It is its own clock too.
    for crate, filt in sorted({(c, f) for *_, c, f in chosen if separate(f)}):
        began = time.monotonic()
        argv, env = command(filt)
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
            argv, env = command(filt)
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
