#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Tabulate a deadline.swift run: per commit phase, how many rounds made the target vsync.

A round made it when the replaced surface was released within the first refresh after the target
(about 5 ms after it; a miss releases one refresh later). Usage: analyse.py <run.txt>
"""
import sys,re
from collections import defaultdict
r=[dict(re.findall(r'(\w+)=([-\d.]+)',l)) for l in open(sys.argv[1]) if l.startswith('round')]
P=float(r[0]['period_ms'])
by=defaultdict(list)
for x in r:
    d=float(x['d_ms']); u=float(x['u_ms']); c=float(x['c_ms'])
    # made = released within the first period after target
    by[d].append((u, abs(c+d)))
print('n',len(r),'P',P)
print(' d_ms  made/n   u_ms values   (late-commit flag)')
for d in sorted(by):
    v=by[d]; made=sum(1 for u,_ in v if u< P*0.5+5)
    print(f'{d:5.1f}  {made}/{len(v)}  ', ' '.join(f'{u:.1f}' for u,_ in sorted(v)), ' slip>0.2ms:',sum(1 for _,s in v if s>0.2))
