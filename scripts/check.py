#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Run the in-crate checkers: Kani proofs, fuzz targets, or the sabotage sweep.

    scripts/check.py kani [crate-dir ...]         every proof, or those in the named crates
    scripts/check.py loom [crate-dir ...]         every loom model, or those in the named crates
    scripts/check.py fuzz [target ...] [--seconds N]
    scripts/check.py miri [crate-dir ...] [--stall N]
    scripts/check.py sabotage [pattern ...]

`cargo xtask check` wraps this. What each tool is for, and what it cannot do, is in
`docs/design/in-crate-checkers.md`. None of it touches HVF or needs a signed worker.

Kani runs each proof under `--harness-timeout`, which stops `cbmc` itself: a `timeout` around
`cargo kani` does not, and a `cbmc` that has lost its way holds gigabytes with no verdict in sight.
Every proof must finish well inside the limit; one that needs longer is the wrong shape for Kani
(see the design doc) rather than a reason to raise it.

loom models live in `loom_model` modules under `cfg(all(test, loom))`. They build with `--cfg loom`
in their own target directory, so switching between loom and ordinary builds rebuilds neither.

Fuzzing runs each target, limina's and the libkrun fork's, for a fixed time with libFuzzer's own
memory cap and a per-input timeout, and names where the crash inputs were written. A crash is committed as a failing unit test before it is fixed.

Miri runs each crate's unit tests one at a time. A test that reaches a foreign call ends the whole
run, so the sweep reruns past it with `--skip` and says, per crate, what ran and why each of the
others did not: a foreign call, undefined behaviour, a failure, or a stall (no progress for
`--stall` seconds). Only undefined behaviour and failures fail the sweep. The enumeration walks
are skipped up front: they run millions of sequences, which under Miri is hours.
"""

import argparse
import os
import re
import select
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
KANI_HARNESS_TIMEOUT = '10m'
FUZZ_RSS_LIMIT_MB = 4096
# Seconds one input may take before libFuzzer calls it a hang: every target here answers an input in
# milliseconds, so a hang is a loop the guest's bytes can drive, not a slow input.
FUZZ_TIMEOUT_S = 10
# Arguments a crate's proofs need beyond `cargo kani`. Without them a proof can be compiled out,
# and Kani reports the crate verified having checked nothing in it.
KANI_ARGS = {
    'third_party/libkrun/src/devices': ['--features', 'usb'],
}
# Tests Miri is not asked to run, by substring: the enumeration walks (`every_sequence` modules and
# the like), whose millions of sequences are hours in the interpreter.
MIRI_SKIP = ('every_', 'handoff_sequence', 'ownership_sequence')
# The interpreter's clock and file system are the host's: tests that read either still run, and
# isolation would stop them at the first `Instant::now` with a file behind it.
MIRI_FLAGS = '-Zmiri-disable-isolation'
# limina's fuzz workspace, and the libkrun fork's.
FUZZ_DIRS = ('fuzz', 'third_party/libkrun/fuzz')


def crates_with(marker):
    """The crate directories whose sources hold `marker`: limina's own, and the libkrun fork's
    (on its `limina` branch, beside the code they check)."""
    found = set()
    for pattern in ('crates/*/src/**/*.rs', 'third_party/libkrun/src/*/src/**/*.rs'):
        for src in sorted(ROOT.glob(pattern)):
            if marker in src.read_text(errors='replace'):
                crate = src.parent
                while not (crate / 'Cargo.toml').exists():
                    crate = crate.parent
                found.add(crate)
    return sorted(found)


def run(argv, cwd, env_extra=None):
    """Run a command in its own process group, so an interrupted run takes its children too."""
    print('==> %s  (in %s)' % (' '.join(argv), os.path.relpath(cwd, ROOT)), flush=True)
    env = dict(os.environ, **(env_extra or {}))
    env.pop('LIMINA_HVF_TESTS', None)
    proc = subprocess.Popen(argv, cwd=cwd, env=env, start_new_session=True)
    try:
        return proc.wait()
    except KeyboardInterrupt:
        os.killpg(os.getpgid(proc.pid), 9)
        raise


def kani(crates):
    dirs = [ROOT / c for c in crates] if crates else crates_with('#[cfg(kani)]')
    failed = []
    for d in dirs:
        argv = ['cargo', 'kani'] + KANI_ARGS.get(os.path.relpath(d, ROOT), []) + [
            '-Z', 'stubbing', '-Z', 'unstable-options', '--harness-timeout', KANI_HARNESS_TIMEOUT]
        if run(argv, d) != 0:
            failed.append(os.path.relpath(d, ROOT))
    # Kani re-resolves libkrun's lockfile against its own workspace; keep the fork's tree clean.
    subprocess.run(['git', 'checkout', '--quiet', 'Cargo.lock'], cwd=ROOT / 'third_party/libkrun',
                   stderr=subprocess.DEVNULL)
    if failed:
        print('\nkani: proofs failed or did not finish in: %s' % ', '.join(failed))
        return 1
    print('\nkani: every proof verified in %d crate(s)' % len(dirs))
    return 0


def loom(crates):
    dirs = [ROOT / c for c in crates] if crates else crates_with('#[cfg(all(test, loom))]')
    failed = []
    env_extra = {'RUSTFLAGS': '--cfg loom', 'CARGO_TARGET_DIR': str(ROOT / 'target/loom')}
    for d in dirs:
        # A crate with no library target (limina itself) keeps its models in the binary.
        target = '--lib' if (d / 'src/lib.rs').exists() else '--bins'
        argv = ['cargo', 'test', '--release', target, 'loom_model']
        if run(argv, d, env_extra) != 0:
            failed.append(os.path.relpath(d, ROOT))
    subprocess.run(['git', 'checkout', '--quiet', 'Cargo.lock'], cwd=ROOT / 'third_party/libkrun',
                   stderr=subprocess.DEVNULL)
    if failed:
        print('\nloom: models failed in: %s' % ', '.join(failed))
        return 1
    print('\nloom: every model passed in %d crate(s)' % len(dirs))
    return 0


def fuzz_targets():
    """Every fuzz target, as `(name, workspace directory)`."""
    found = []
    for d in FUZZ_DIRS:
        out = subprocess.run(['cargo', '+nightly', 'fuzz', 'list'], cwd=ROOT / d,
                             capture_output=True, text=True, check=True).stdout
        found += [(t, ROOT / d) for t in out.split()]
    return found


def fuzz(targets, seconds):
    known = fuzz_targets()
    chosen = [(t, d) for t, d in known if not targets or t in targets]
    unknown = set(targets) - {t for t, _ in known}
    if unknown:
        print('fuzz: no such target: %s' % ', '.join(sorted(unknown)))
        return 1
    crashed = []
    for t, d in chosen:
        argv = ['cargo', '+nightly', 'fuzz', 'run', t, '--',
                '-max_total_time=%d' % seconds, '-rss_limit_mb=%d' % FUZZ_RSS_LIMIT_MB,
                '-timeout=%d' % FUZZ_TIMEOUT_S]
        if run(argv, d) != 0:
            crashed.append('%s (inputs under %s)'
                           % (t, os.path.relpath(d / 'artifacts' / t, ROOT)))
    subprocess.run(['git', 'checkout', '--quiet', 'Cargo.lock'], cwd=ROOT / 'third_party/libkrun',
                   stderr=subprocess.DEVNULL)
    if crashed:
        print('\nfuzz: crashes in %s' % ', '.join(crashed))
        return 1
    print('\nfuzz: %d target(s) ran %ds each with no crash' % (len(chosen), seconds))
    return 0


def miri_env():
    # Lints capped: Miri is on nightly, whose new deprecations a dependency that denies warnings
    # (imago) turns into errors, and linting is not what this run is for. The repo venv on PATH,
    # as `cargo xtask` puts it: virglrs's build script runs its generators through `python3`.
    env = dict(os.environ, MIRIFLAGS=MIRI_FLAGS, CARGO_TARGET_DIR=str(ROOT / 'target/miri'),
               RUSTFLAGS='--cap-lints=warn')
    venv = ROOT / 'third_party/venv-mesa/bin'
    if venv.is_dir():
        env['PATH'] = '%s:%s' % (venv, env.get('PATH', ''))
    env.pop('LIMINA_HVF_TESTS', None)
    return env


def miri_tests(d):
    """Every unit test in `d`, by its exact name."""
    target = '--lib' if (d / 'src/lib.rs').exists() else '--bins'
    out = subprocess.run(['cargo', '+nightly', 'miri', 'test', target, '--', '--list'],
                         cwd=d, env=miri_env(), capture_output=True, text=True).stdout
    return re.findall(r'^(\S+): test$', out, re.M)


def miri_run(d, skips, stall):
    """One `cargo miri test` over `d`'s unit tests, one at a time, past `skips`. Returns the
    exit code (None for a stall), the test running when it ended, and the output."""
    target = '--lib' if (d / 'src/lib.rs').exists() else '--bins'
    argv = ['cargo', '+nightly', 'miri', 'test', target, '--', '--test-threads=1', '--exact']
    for skip in sorted(skips):
        argv += ['--skip', skip]
    proc = subprocess.Popen(argv, cwd=d, env=miri_env(), stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, start_new_session=True)
    out = b''
    last = time.monotonic()
    code = None
    while True:
        ready, _, _ = select.select([proc.stdout], [], [], 5)
        if ready:
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                code = proc.wait()
                break
            out += chunk
            last = time.monotonic()
        # Compiling is slow and silent too; only a test in flight can stall.
        elif re.search(rb'test \S+ \.\.\. $', out) and time.monotonic() - last > stall:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            proc.wait()
            break
    text = out.decode(errors='replace')
    # With one thread the harness names each test before it runs it, and completes the line after.
    started = re.findall(r'^test (\S+) \.\.\. ', text, re.M)
    finished = set(re.findall(r'^test (\S+) \.\.\. (?:ok|FAILED|ignored)', text, re.M))
    running = next((t for t in reversed(started) if t not in finished), None)
    return code, running, text


def miri(crates, stall):
    dirs = [ROOT / c for c in crates] if crates else sorted(
        p.parent for p in ROOT.glob('crates/*/Cargo.toml'))
    bad = []
    for d in dirs:
        rel = os.path.relpath(d, ROOT)
        print('==> miri: %s' % rel, flush=True)
        tests = miri_tests(d)
        # Exact names throughout, and each rerun skips what already ran: a substring skip would
        # take tests whose names merely contain it, and rerunning the passes is quadratic.
        walks = {t for t in tests if any(w in t for w in MIRI_SKIP)}
        skips = set(walks)
        stops = {'foreign call': [], 'undefined behaviour': [], 'failed': [], 'stalled': []}
        passed = set()
        while True:
            code, running, text = miri_run(d, skips, stall)
            ran = set(re.findall(r'^test (\S+) \.\.\. ok', text, re.M))
            passed |= ran
            failed = re.findall(r'^test (\S+) \.\.\. FAILED', text, re.M)
            skips |= ran | set(failed)
            stops['failed'] += failed
            if code == 0 and running is None:
                break
            if code is not None and running is None and not failed:
                if 'error: could not compile' in text or 'error[E' in text:
                    print(text[-4000:])
                    stops['failed'].append('(the crate does not build under Miri)')
                else:
                    print(text[-4000:])
                    stops['failed'].append('(the run ended with no test in flight)')
                break
            if running is None:
                # The run finished, with ordinary test failures.
                break
            if code is None:
                kind = 'stalled'
            elif 'Undefined Behavior' in text:
                kind = 'undefined behaviour'
                print(text[text.index('Undefined Behavior') - 200:][:3000])
            elif 'unsupported operation' in text or 'can\'t call foreign function' in text:
                kind = 'foreign call'
            else:
                kind = 'failed'
                print(text[-3000:])
            stops[kind].append(running)
            skips.add(running)
        print('miri: %s: %d of %d passed, %d walks not run' % (rel, len(passed), len(tests),
                                                               len(walks)))
        for kind, tests in stops.items():
            if tests:
                print('  %s (%d): %s' % (kind, len(tests), ', '.join(tests)))
        if stops['undefined behaviour'] or stops['failed']:
            bad.append(rel)
    if bad:
        print('\nmiri: undefined behaviour or failures in: %s' % ', '.join(bad))
        return 1
    print('\nmiri: nothing undefined in %d crate(s)' % len(dirs))
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    sub = ap.add_subparsers(dest='tool', required=True)
    k = sub.add_parser('kani', help='run the Kani proofs')
    k.add_argument('crates', nargs='*', help='crate directories, relative to the repo root')
    lo = sub.add_parser('loom', help='run the loom models')
    lo.add_argument('crates', nargs='*', help='crate directories, relative to the repo root')
    f = sub.add_parser('fuzz', help='run the fuzz targets for a fixed time each')
    f.add_argument('targets', nargs='*')
    f.add_argument('--seconds', type=int, default=60)
    m = sub.add_parser('miri', help='run the unit tests under Miri, past each foreign call')
    m.add_argument('crates', nargs='*', help='crate directories, relative to the repo root')
    m.add_argument('--stall', type=int, default=300,
                   help='seconds a test may go without output before it is called stalled')
    s = sub.add_parser('sabotage', help='run the sabotage sweep')
    s.add_argument('patterns', nargs='*')
    a = ap.parse_args()

    if a.tool == 'kani':
        return kani(a.crates)
    if a.tool == 'loom':
        return loom(a.crates)
    if a.tool == 'fuzz':
        return fuzz(a.targets, a.seconds)
    if a.tool == 'miri':
        return miri(a.crates, a.stall)
    return run([sys.executable, str(ROOT / 'scripts/sabotage-sweep.py')] + a.patterns, ROOT)


if __name__ == '__main__':
    sys.exit(main())
