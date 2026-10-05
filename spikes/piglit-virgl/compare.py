#!/usr/bin/env python3
"""compare.py <stock results.json.bz2> <enhanced results.json.bz2> [rig.tsv]

Status counts per arm, then every test that is not pass/skip on either arm, with the status on
the other arm and, given the upstream rig's non-pass list (tab-separated: main status, fix status,
test name), the status there. A test the rig list does not name passed or skipped on the rig.
"""
import bz2
import collections
import json
import sys


def load(path):
    with bz2.open(path) as f:
        return {k: t["result"] for k, t in json.load(f)["tests"].items()}


stock, enh = load(sys.argv[1]), load(sys.argv[2])
rig = {}
if len(sys.argv) > 3:
    for line in open(sys.argv[3]):
        cols = line.rstrip("\n").split("\t")
        rig[cols[2]] = cols[0]

for name, arm in (("stock", stock), ("enhanced", enh)):
    counts = collections.Counter(arm.values())
    print(f"{name}: {len(arm)} tests, " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))

ok = ("pass", "skip", None)
print("\nstock\tenhanced\trig\ttest")
for test in sorted(set(stock) | set(enh)):
    s, e = stock.get(test), enh.get(test)
    if s in ok and e in ok:
        continue
    print(f"{s}\t{e}\t{rig.get(test, 'pass/skip' if rig else '-')}\t{test}")
