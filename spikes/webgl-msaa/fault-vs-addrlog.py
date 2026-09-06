#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Cross-reference the kernel's GPU fault addresses against everything KosmicKrisp
# logged the address of, so a fault can be named rather than merely placed outside
# a band. Both halves are needed and neither is free: the worker log only carries
# ranges when the arm ran with LIMINA_KK_ADDR_LOG=1, and the .ips files arrive
# hours late (sometimes never), so run this again the next day.
#
#   spikes/webgl-msaa/fault-vs-addrlog.py /tmp/webgl-msaa-<arm>/worker.log
#
# With no worker log it just prints the faults, which is still the population
# summary (alignment, spread, requestor).
import glob
import os
import re
import sys

FAULT_DIR = "/Library/Logs/DiagnosticReports"


def faults():
    out = []
    for f in sorted(glob.glob(os.path.join(FAULT_DIR, "gpuEvent-limina-vmm-*.ips"))):
        try:
            t = open(f, errors="replace").read()
        except OSError:
            continue
        addr = re.search(r'"?address"?\s*[:=]\s*(\d{6,})', t)
        req = re.search(r'requestor"?\s*[:=]\s*"?(\d+)', t)
        why = re.search(r'restart_reason_desc"?\s*[:=]\s*"([^"]+)"', t)
        out.append(
            (
                os.path.basename(f),
                int(addr.group(1)) if addr else None,
                req.group(1) if req else "-",
                why.group(1) if why else "-",
            )
        )
    return out


# bo+ / img+ / import-heap+ / import-host+ / samplertab all carry gpu=0xA..0xB.
RANGE = re.compile(r"\b(\S+?)\+? .*?gpu=0x([0-9a-f]+)\.\.0x([0-9a-f]+)")


def ranges(path):
    out = []
    for line in open(path, errors="replace"):
        if "LIMINA-ADDR" not in line and "gpu=0x" not in line:
            continue
        m = RANGE.search(line)
        if m:
            out.append((m.group(1), int(m.group(2), 16), int(m.group(3), 16)))
    return out


def main():
    rs = ranges(sys.argv[1]) if len(sys.argv) > 1 else []
    if rs:
        lo = min(r[1] for r in rs)
        hi = max(r[2] for r in rs)
        print(f"{len(rs)} logged ranges, {lo:#x}..{hi:#x} ({lo / 2**30:.1f}..{hi / 2**30:.1f} GiB)")
    else:
        print("no logged ranges (pass a worker.log from an LIMINA_KK_ADDR_LOG=1 arm)")

    for name, addr, req, why in faults():
        if addr is None:
            print(f"{name}  {why}")
            continue
        hit = [r for r in rs if r[1] <= addr < r[2]]
        where = ", ".join(f"{k} {a:#x}..{b:#x}" for k, a, b in hit) if hit else "unmatched"
        align = 64 if addr % 64 == 0 else 1
        for k in (4096, 16384, 65536):
            if addr % k == 0:
                align = k
        print(f"{name}  {addr:#014x} {addr / 2**30:8.2f} GiB  align={align:<5} req={req:<4} {where}")


main()
