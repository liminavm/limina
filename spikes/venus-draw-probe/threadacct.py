#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Per-thread CPU accounting for a `sample <proc> 10 -file X` capture of limina-vmm:
# splits each thread's ticks into GUEST (hv_trap = vCPU executing guest code),
# host-busy, and idle (blocking syscall leaves), then aggregates by thread-name
# group in core-equivalents. The sum cross-checks against `ps -o %cpu`.
# Usage: sample limina-vmm 10 -file /tmp/x.sample && threadacct.py /tmp/x.sample
import re, sys
txt = open(sys.argv[1]).read()
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 10.0
# isolate the call-graph region (before "Total number in stack" / binary images)
end = txt.find('Sort by top of stack')
if end > 0: txt = txt[:end]
lines = txt.splitlines()
# thread sections
starts = [i for i,l in enumerate(lines) if re.match(r'^    \d+ Thread_\d+', l)]
starts.append(len(lines))
WAIT = ['__psynch_cvwait','__ulock_wait','__semwait_signal','kevent','__recvmsg',
        '__select','semaphore_wait_trap','__workq_kernreturn','__sigsuspend',
        '__accept','__poll','mach_msg2_trap']
READ = ['__read','read$NOCANCEL']
GUEST = ['hv_vcpu_run']
rows = []
ticks = 0
for a,b in zip(starts, starts[1:]):
    head = lines[a]
    m = re.match(r'^    (\d+) Thread_(\d+)(?::? ?(.*))?$', head)
    total = int(m.group(1)); name = (m.group(3) or '').strip() or f'tid-{m.group(2)}'
    ticks = max(ticks, total)
    idle = guest = 0
    body = lines[a+1:b]
    for l in body:
        lm = re.match(r'^[\s+!:|*]*(\d+) (\S+)\s+\(in ([^)]+)\)', l)
        if not lm: continue
        cnt, sym, lib = int(lm.group(1)), lm.group(2), lm.group(3)
        if sym in WAIT: idle += cnt
        elif sym in READ and 'libsystem_kernel' in lib: idle += cnt
        elif sym in GUEST: guest += cnt
    busy = total - idle - guest
    rows.append((name, total, idle, guest, busy))
print(f"ticks per thread: {ticks} over {dur}s")
agg = {}
for name,total,idle,guest,busy in rows:
    key = re.sub(r'-?\d+$','',name) or name
    g = agg.setdefault(key, [0,0,0,0])
    g[0]+=total; g[1]+=idle; g[2]+=guest; g[3]+=busy
print(f"{'thread group':28s} {'n':>3s} {'guest-cores':>11s} {'host-busy-cores':>15s}")
n_by_key = {}
for name,*_ in rows:
    key = re.sub(r'-?\d+$','',name) or name
    n_by_key[key] = n_by_key.get(key,0)+1
tot_g = tot_b = 0
for key,(t,i,g,bsy) in sorted(agg.items(), key=lambda kv: -(kv[1][2]+kv[1][3])):
    gc, bc = g/ticks, bsy/ticks
    tot_g += gc; tot_b += bc
    if gc+bc < 0.005: continue
    print(f"{key:28s} {n_by_key[key]:3d} {gc:11.2f} {bc:15.2f}")
print(f"{'TOTAL':28s} {'':3s} {tot_g:11.2f} {tot_b:15.2f}  (sum = {tot_g+tot_b:.2f} cores)")
