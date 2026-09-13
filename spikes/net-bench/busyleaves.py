#!/usr/bin/env python3
"""On-CPU time per thread from a macOS `sample` file, by leaf function.

`sample` records every thread on every tick, blocked or not, so a thread's total is always the
run length. A node's self count (its count less its children's) is time spent *in* that frame;
dropping the frames that block (the wait set below) leaves what the thread spent on a core.
`hv_trap` is kept: it is a vCPU thread running the guest.

Usage: busyleaves.py <file.sample> [thread-name-substring ...] [--top N]
"""
import re
import sys
from collections import Counter, defaultdict

WAITS = {
    "__psynch_cvwait", "semaphore_wait_trap", "semaphore_timedwait_trap", "kevent", "kevent64",
    "kevent_id", "__workq_kernreturn", "poll", "__semwait_signal", "mach_msg2_trap",
    "__ulock_wait", "__ulock_wait2", "__psynch_mutexwait", "__select", "__sigsuspend",
    "__wait4", "__pselect",
}
# A blocking read is a wait too, but a read that returns data is work; `sample` cannot tell
# them apart, so report it on its own line rather than guess.
AMBIGUOUS = {"read", "__recvfrom", "__recvmsg", "__read_nocancel"}

LINE = re.compile(r"^(?P<pre>[ +!:|]*)(?P<n>\d+) (?P<fn>.+?)(?:  \(in (?P<lib>[^)]+)\).*)?$")
THREAD = re.compile(r"^\s+\d+ (Thread_\d+.*)$")


def short(fn):
    fn = re.sub(r"_R[A-Za-z0-9_]+?(\d+[a-z_]+)\d*E?$", r"\1", fn) if fn.startswith("_R") else fn
    return fn[:110]


def parse(path):
    threads = {}
    cur = None
    stack = []  # (depth, count, key)
    lines = open(path, errors="replace").read().split("\n")
    in_graph = False
    for ln in lines:
        if ln.startswith("Call graph:"):
            in_graph = True
            continue
        if not in_graph:
            continue
        if ln.startswith("Total number in stack") or ln.startswith("Sort by top of stack"):
            break
        m = THREAD.match(ln)
        if m and not ln.lstrip().startswith(("+", "!", ":", "|")) and ln.startswith("    ") and not ln.startswith("     "):
            cur = m.group(1)
            threads[cur] = Counter()
            stack = []
            continue
        m = LINE.match(ln)
        if not m or cur is None:
            continue
        depth = len(m.group("pre"))
        n = int(m.group("n"))
        fn = m.group("fn").strip()
        while stack and stack[-1][0] >= depth:
            stack.pop()
        if stack:
            threads[cur][("child", id(stack[-1]))] += 0  # keep structure implicit
            stack[-1][2]["kids"] += n
        node = {"fn": fn, "n": n, "kids": 0}
        stack.append((depth, n, node))
        threads[cur].setdefault("_nodes", [])
        threads[cur]["_nodes"].append(node)
    out = {}
    for t, c in threads.items():
        self_by_fn = Counter()
        for node in c.get("_nodes", []):
            s = node["n"] - node["kids"]
            if s > 0:
                self_by_fn[short(node["fn"])] += s
        out[t] = self_by_fn
    return out


def main():
    args = sys.argv[1:]
    top = 12
    if "--top" in args:
        i = args.index("--top")
        top = int(args[i + 1])
        del args[i:i + 2]
    path, filters = args[0], args[1:]
    text = open(path, errors="replace").read()
    m = re.search(r"(\d+) samples? .*?every (\d+) millisecond", text) or re.search(r"every (\d+) millisecond", text)
    threads = parse(path)
    rows = []
    for t, c in threads.items():
        total = sum(c.values())
        waits = sum(v for k, v in c.items() if k in WAITS)
        amb = sum(v for k, v in c.items() if k in AMBIGUOUS)
        busy = total - waits - amb
        rows.append((busy, amb, total, t, c))
    rows.sort(reverse=True)
    for busy, amb, total, t, c in rows:
        if filters and not any(f in t for f in filters):
            continue
        if not filters and busy + amb < total * 0.02:
            continue
        print("%-70s busy %5.1f%%  read/recv %5.1f%%" % (t[:70], 100 * busy / total, 100 * amb / total))
        for fn, v in c.most_common():
            if fn in WAITS:
                continue
            if v < total * 0.005:
                break
            print("    %6.1f%%  %s" % (100 * v / total, fn))
            top_left = top
        print()


if __name__ == "__main__":
    main()
