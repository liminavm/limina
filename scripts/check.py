#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Run the in-crate checkers: Kani proofs, fuzz targets, or the sabotage sweep.

    scripts/check.py kani [crate-dir ...]         every proof, or those in the named crates
    scripts/check.py fuzz [target ...] [--seconds N]
    scripts/check.py sabotage [pattern ...]

`cargo xtask check` wraps this. What each tool is for, and what it cannot do, is in
`docs/design/in-crate-checkers.md`. None of it touches HVF or needs a signed worker.

Kani runs each proof under `--harness-timeout`, which stops `cbmc` itself: a `timeout` around
`cargo kani` does not, and a `cbmc` that has lost its way holds gigabytes with no verdict in sight.
Every proof must finish well inside the limit; one that needs longer is the wrong shape for Kani
(see the design doc) rather than a reason to raise it.

Fuzzing runs each target for a fixed time with libFuzzer's own memory cap, and names the crash
file it wrote. A crash is committed as a failing unit test before it is fixed.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
KANI_HARNESS_TIMEOUT = '10m'
FUZZ_RSS_LIMIT_MB = 4096


def kani_crates():
    """The crate directories holding at least one Kani proof."""
    found = set()
    for src in sorted(ROOT.glob('crates/*/src/**/*.rs')):
        if '#[cfg(kani)]' in src.read_text():
            found.add(src.relative_to(ROOT).parts[:2])
    return [ROOT.joinpath(*p) for p in sorted(found)]


def run(argv, cwd):
    """Run a command in its own process group, so an interrupted run takes its children too."""
    print('==> %s  (in %s)' % (' '.join(argv), os.path.relpath(cwd, ROOT)), flush=True)
    env = dict(os.environ)
    env.pop('LIMINA_HVF_TESTS', None)
    proc = subprocess.Popen(argv, cwd=cwd, env=env, start_new_session=True)
    try:
        return proc.wait()
    except KeyboardInterrupt:
        os.killpg(os.getpgid(proc.pid), 9)
        raise


def kani(crates):
    dirs = [ROOT / c for c in crates] if crates else kani_crates()
    failed = []
    for d in dirs:
        argv = ['cargo', 'kani', '-Z', 'unstable-options',
                '--harness-timeout', KANI_HARNESS_TIMEOUT]
        if run(argv, d) != 0:
            failed.append(os.path.relpath(d, ROOT))
    if failed:
        print('\nkani: proofs failed or did not finish in: %s' % ', '.join(failed))
        return 1
    print('\nkani: every proof verified in %d crate(s)' % len(dirs))
    return 0


def fuzz(targets, seconds):
    fuzz_dir = ROOT / 'fuzz'
    if not targets:
        out = subprocess.run(['cargo', '+nightly', 'fuzz', 'list'], cwd=fuzz_dir,
                             capture_output=True, text=True, check=True).stdout
        targets = out.split()
    crashed = []
    for t in targets:
        argv = ['cargo', '+nightly', 'fuzz', 'run', t, '--',
                '-max_total_time=%d' % seconds, '-rss_limit_mb=%d' % FUZZ_RSS_LIMIT_MB]
        if run(argv, fuzz_dir) != 0:
            crashed.append(t)
    if crashed:
        print('\nfuzz: crashes in %s; the inputs are under fuzz/artifacts/<target>/'
              % ', '.join(crashed))
        return 1
    print('\nfuzz: %d target(s) ran %ds each with no crash' % (len(targets), seconds))
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    sub = ap.add_subparsers(dest='tool', required=True)
    k = sub.add_parser('kani', help='run the Kani proofs')
    k.add_argument('crates', nargs='*', help='crate directories, relative to the repo root')
    f = sub.add_parser('fuzz', help='run the fuzz targets for a fixed time each')
    f.add_argument('targets', nargs='*')
    f.add_argument('--seconds', type=int, default=60)
    s = sub.add_parser('sabotage', help='run the sabotage sweep')
    s.add_argument('patterns', nargs='*')
    a = ap.parse_args()

    if a.tool == 'kani':
        return kani(a.crates)
    if a.tool == 'fuzz':
        return fuzz(a.targets, a.seconds)
    return run([sys.executable, str(ROOT / 'scripts/sabotage-sweep.py')] + a.patterns, ROOT)


if __name__ == '__main__':
    sys.exit(main())
