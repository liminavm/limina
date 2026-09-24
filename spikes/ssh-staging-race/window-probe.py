#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""window-probe — measure the gap between "sshd answers a banner" and "ssh can run a command".

The harness's readiness oracle (`Guest::wait_for_ssh_banner`, and
`scripts/wait-guest-ssh.sh`, which uses the same rule) is: connect to gvproxy's forward,
read 8 bytes, accept anything starting with `SSH-`. Every networked test then issues its
first `ssh_exec` immediately afterwards. venus_clear_rect failed there once in a 3-wide
suite (2026-09-23) with exit 255 and an EMPTY stderr -- ssh's signature for "the TCP
connect was accepted and the connection then died before the key exchange finished", as
opposed to "connection refused", which ERROR still prints.

Waiting for that flake again is expensive: 63 boots of the real test at the real width
did not reproduce it. So measure the WINDOW instead of waiting to land in it. From the
moment the supervisor announces the forward, probe in a tight loop, twice per round:

  T_b -- the first round whose banner read succeeds  (what the harness waits for)
  T_s -- the first round whose `ssh true` succeeds   (what the harness then needs)

T_s - T_b is the width of the window in which the oracle says "ready" and the very next
command fails. Every boot yields a number, so a rare failure becomes a measurement. The
harness polls every 500 ms, so it fires its first ssh somewhere in [T_b, T_b + 0.5s]: a
window near zero is safe, and one comfortably past 500 ms should fail nearly every time.

Usage: spikes/ssh-staging-race/window-probe.py [--boots N] [--load N] [--interval S]
       --load N boots N extra guests first and leaves them running for the whole probe,
       so the measured guest competes for the host exactly as it does in the suite.
"""

import argparse
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
KERNEL = os.path.join(REPO, "target/test-guest/kernel/Image-16k")
IMAGE = os.path.join(REPO, "Fedora-Workstation-44.stock.test.raw")
# Verbatim from GuestConfig::enhanced_fedora_from_env (crates/limina-test/src/lib.rs).
CMDLINE = ("root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 "
           "console=ttyAMA0 systemd.zram=0")
RUNDIR = "/tmp/limina-sshwindow"
SSH_OPTS = ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "BatchMode=yes", "-o", "ConnectTimeout=4", "-o", "LogLevel=INFO"]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def clone_disk(tag):
    dst = os.path.join(RUNDIR, f"{tag}.raw")
    if os.path.exists(dst):
        os.unlink(dst)
    # APFS CoW clone: instant and free, the same trick the harness uses per guest.
    subprocess.run(["cp", "-c", IMAGE, dst], check=True)
    return dst


def boot(tag):
    """Spawn a supervisor and return (proc, port, logpath). Does not wait for the guest."""
    disk = clone_disk(tag)
    port = free_port()
    log = open(os.path.join(RUNDIR, f"{tag}.log"), "w")
    env = dict(os.environ, TMPDIR=RUNDIR)
    proc = subprocess.Popen(
        [os.path.join(REPO, "target/debug/limina"),
         "--kernel", KERNEL, "--disk", disk, "--cmdline", CMDLINE,
         "--net", "--ssh-port", str(port), "--cpus", "4", "--ram-mib", "4096"],
        stdout=log, stderr=subprocess.STDOUT, env=env)
    return proc, port, log.name


def banner_ok(port, timeout=1.5):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=timeout) as c:
            c.settimeout(timeout)
            return c.recv(64).startswith(b"SSH-")
    except OSError:
        return False


def ssh_ok(port):
    p = subprocess.run(["ssh", "-p", str(port)] + SSH_OPTS + ["claude@127.0.0.1", "true"],
                       capture_output=True, timeout=30)
    return p.returncode, p.stderr.decode(errors="replace").strip()


def probe(port, proc, interval, limit=240.0):
    """Return (t_banner, t_session, trace) measured from the call, or Nones on timeout."""
    t0 = time.monotonic()
    t_b = t_s = None
    trace = []
    while time.monotonic() - t0 < limit:
        t = time.monotonic() - t0
        b = banner_ok(port)
        rc, err = ssh_ok(port) if b else (None, "")
        if b and t_b is None:
            t_b = t
        if rc == 0 and t_s is None:
            t_s = t
        if b:
            # Drop ssh's own known-hosts warning (LogLevel=INFO prints it every time, and
            # UserKnownHostsFile=/dev/null makes it unavoidable); keep everything else, because
            # the line that explains the refusal is the GUEST's, above ssh's own last word.
            keep = [ln for ln in err.splitlines() if "Permanently added" not in ln]
            trace.append((t, rc, " | ".join(keep)))
        if t_s is not None:
            break
        if proc.poll() is not None:
            break
        time.sleep(interval)
    return t_b, t_s, trace


def shutdown(procs):
    for p, _, _ in procs:
        if p.poll() is None:
            p.terminate()
    deadline = time.monotonic() + 45
    for p, _, _ in procs:
        while p.poll() is None and time.monotonic() < deadline:
            time.sleep(0.2)
        if p.poll() is None:
            p.kill()
            p.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--boots", type=int, default=5)
    ap.add_argument("--load", type=int, default=2, help="extra guests kept booted alongside")
    ap.add_argument("--interval", type=float, default=0.1)
    args = ap.parse_args()

    for path, what in ((KERNEL, "16k test kernel"), (IMAGE, "stock.test image"),
                       (os.path.join(REPO, "target/debug/limina"), "limina")):
        if not os.path.exists(path):
            sys.exit(f"missing {what}: {path}")
    os.makedirs(RUNDIR, exist_ok=True)

    load = []
    try:
        for i in range(args.load):
            load.append(boot(f"load{i}"))
        if load:
            # Let the neighbours finish booting, so the measured guest competes with
            # running guests rather than with a burst of concurrent boots.
            print(f"warming {len(load)} co-resident guest(s)...", flush=True)
            for _, port, _ in load:
                end = time.monotonic() + 180
                while time.monotonic() < end and not banner_ok(port):
                    time.sleep(1)

        print(f"{'boot':>4}  {'T_banner':>9}  {'T_session':>9}  {'window':>7}  first-ssh-error")
        widths = []
        for i in range(args.boots):
            proc, port, log = boot(f"probe{i}")
            t_b, t_s, trace = probe(port, proc, args.interval)
            first_err = next((e for _, rc, e in trace if rc not in (0, None) and e), "")
            if t_b is None or t_s is None:
                print(f"{i:>4}  {'-' if t_b is None else f'{t_b:9.2f}'}  "
                      f"{'TIMEOUT':>9}  {'-':>7}  {first_err}  (log {log})")
            else:
                w = t_s - t_b
                widths.append(w)
                print(f"{i:>4}  {t_b:9.2f}  {t_s:9.2f}  {w:7.2f}  {first_err}", flush=True)
            shutdown([(proc, port, log)])
        if widths:
            print(f"\nwindow (T_session - T_banner) over {len(widths)} boots: "
                  f"min {min(widths):.2f}s  max {max(widths):.2f}s  "
                  f"mean {sum(widths)/len(widths):.2f}s")
            print("the harness polls the banner every 0.5s and ssh's immediately after, so a "
                  "window wider than that is a failure it cannot avoid.")
    finally:
        shutdown(load)
        shutil.rmtree(RUNDIR, ignore_errors=True)


if __name__ == "__main__":
    main()
