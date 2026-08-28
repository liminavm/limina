#!/usr/bin/env python3
"""Read a powermetrics log against the battery blocks it spans.

    pm-align.py <powermetrics.log> <block.csv> [block.csv ...]

The pack's own draw is display-dominated, which buries a few hundred milliwatts of vCPU policy in
noise. Package power is two orders more sensitive, but powermetrics has no idea what a block is —
so each block's window is taken from its sample timestamps and the package samples inside it are
averaged. A block the log does not span is reported as such rather than averaged over whatever
it does cover.
"""
import re, sys, datetime as dt

def pm_samples(path):
    out, when = [], None
    for line in open(path, errors='ignore'):
        m = re.match(r'\*\*\* Sampled system activity \(\w+ (\w+ +\d+ [\d:]+) \d{4}', line)
        if m:
            when = m.group(1).split()[-1]
        m = re.match(r'Combined Power \(CPU \+ GPU \+ ANE\): (\d+) mW', line)
        if m and when:
            out.append((when, int(m.group(1))))
        m = re.match(r'(CPU|GPU) Power: (\d+) mW', line)
        if m and when:
            out.append((when + '/' + m.group(1), int(m.group(2))))
    return out

def window(csv):
    ts = [l.split(',')[0] for l in open(csv).read().splitlines()[1:] if ',' in l]
    return (ts[0], ts[-1]) if ts else None

pm = pm_samples(sys.argv[1])
for csv in sys.argv[2:]:
    w = window(csv)
    if not w:
        print(f"{csv}: no samples"); continue
    lo, hi = w
    combined = [v for t, v in pm if '/' not in t and lo <= t <= hi]
    cpu = [v for t, v in pm if t.endswith('/CPU') and lo <= t.split('/')[0] <= hi]
    gpu = [v for t, v in pm if t.endswith('/GPU') and lo <= t.split('/')[0] <= hi]
    name = csv.split('/')[-1].replace('.csv', '')
    if len(combined) < 10:
        print(f"{name} {lo}-{hi}: not spanned by the log ({len(combined)} samples)")
        continue
    print(f"{name} {lo}-{hi}  n={len(combined):3d}  "
          f"package {sum(combined)/len(combined):7.0f} mW  "
          f"cpu {sum(cpu)/len(cpu):7.0f} mW  gpu {sum(gpu)/len(gpu):6.0f} mW")
