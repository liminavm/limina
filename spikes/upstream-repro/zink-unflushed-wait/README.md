# zink: fix lost-wakeup deadlock in the multi-context unflushed-batch wait

**Bug.** When a context must wait for another context's batch that has not been flushed yet,
`zink_batch_usage_unflushed_wait()` checks `u->unflushed` without `u->mtx`, then takes the mutex
and calls `cnd_wait(&u->flush)` once, without re-checking. `submit_queue()` clears
`bs->usage.unflushed` and broadcasts `bs->usage.flush` without the mutex. A flush that completes
between the waiter's check and its `cnd_wait` is a lost wakeup: the waiter sleeps on a batch
state whose broadcast already happened, and nothing broadcasts on it again unless that batch state
is reused and flushed. Separately, the `trywait` branch passes `{0, 10000}` to `cnd_timedwait`,
which takes an absolute `TIME_UTC` deadline: that is 1970 + 10 µs, so the try-wait never waits.

**Reproducer.** `zink-unflushed-wait.c` (GLES 3, surfaceless EGL, two shared contexts on two
threads): A records `glCopyBufferSubData` into a buffer, hands off to B, then `glFlush`es; B
`glMapBufferRange(GL_MAP_READ_BIT)`s the same buffer, which waits on A's unflushed batch. A
watchdog reports `HANG` when B makes no progress for 10 s. Driver-independent by code reading;
measured on anv.

    cc -O2 -o zink-unflushed-wait zink-unflushed-wait.c -lEGL -lGLESv2 -lpthread
    MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-unflushed-wait 120

A plain run hangs only occasionally (1 in 7 here). Under gdb — no breakpoints, the ptrace
overhead alone shifts the timing — it hangs in most runs, and with `HANG_TRAP=1` it stops there so
the waiter's state can be printed (Mesa with debug info):

    HANG_TRAP=1 MESA_LOADER_DRIVER_OVERRIDE=zink gdb -q -batch -x hang.gdb --args ./zink-unflushed-wait 120

`hang.gdb` + `dump-waiter.py` print the `zink_batch_usage` the blocked thread sleeps on. Every hang
below printed `unflushed=false`: the waiter is asleep on a predicate that is already satisfied,
which is the lost wakeup by definition.

The window can also be widened deterministically: a breakpoint (with a large ignore count, so gdb
only stops and resumes) on the `mtx_lock(&u->mtx)` of the multi-context branch — line 1210 at
b39d173ca93 — holds the waiter between its unlocked check and the lock while the flush thread
completes, the same window a preemption opens.

## Results

Host: Fedora 44, Intel Iris Plus G7 (Ice Lake), zink from the Mesa under test over the
system anv (Fedora mesa 26.1.8). 8 CPUs, process unpinned unless noted. "tc" = threaded context
(`GALLIUM_THREAD=0` turns it off).

| Mesa (zink) | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | plain, tc | 1/7 hung: 1/3 at 60 s (after 494814 iterations), 0/4 at 120 s (~2.5 M iterations each) |
| `main` | under gdb (no breakpoints), tc, 120 s | 4/6 hung (after 14–121 s), all `unflushed=false` |
| `main` | under gdb (no breakpoints), no tc, 120 s | 3/3 hung (after 19–112 s), all `unflushed=false` |
| `main` | `taskset -c 0` (with/without tc, with/without a CPU burner on that core), 60 s | 0/12 hung |
| `main` | gdb breakpoint on the multi-context `mtx_lock` (line 1210), 5–20 s | 7/7 hung on the first hit (6 without tc, 1 with), `unflushed=false` in the 3 runs that dumped it |
| `main` + fix (series tip e09e44d2d0d) | plain, tc, 120 s | 0/4 hung (1.8–2.5 M iterations each) |
| `main` + fix | under gdb (no breakpoints), tc / no tc, 120 s | 0/6 and 0/3 hung (0.6–2.5 M iterations each) |
| `main` + fix | gdb breakpoint on the equivalent `mtx_lock` (line 1228), no tc, 20 s | 0/3 hung, 102k–112k breakpoint hits each |
| `main` | gdb breakpoint on the multi-context `mtx_lock` (line 1210), no tc, 20 s — re-run | 3/3 hung on the first hit |
| `main` + fix as sent (tip 1eea896f4d5: `util/timespec.h`, loop bounded by `submit_count`) | breakpoint on the equivalent `mtx_lock` (line 1217), no tc, 20 s | 0/3 hung, 95k–109k hits each; plain 60 s runs 0/2 |

Measured 2026-10-05.

Pinning everything to one CPU made it *less* likely, not more. The likely reason (not measured):
the flush runs on zink's flush queue thread, and the lost wakeup needs it to complete while the
waiter is between its check and the lock, which two CPUs allow without any preemption.

## MR description (draft)

> **zink: fix lost-wakeup deadlock in the multi-context unflushed-batch wait**
>
> When one context waits on another context's unflushed batch, the waiter checks `unflushed`
> without the usage mutex and then does a single `cnd_wait` without re-checking it, while
> `submit_queue()` clears `unflushed` and broadcasts without the mutex. A flush that completes in
> between is a lost wakeup and the waiter sleeps forever. The try-wait variant also passes
> `{0, 10000}` to `cnd_timedwait`, whose deadline is absolute (1970 + 10 µs), so it never waited.
>
> Reproducer attached: two shared GLES contexts on two threads, one records a
> glCopyBufferSubData and flushes, the other maps the buffer for reading. On zink/anv (Ice Lake),
> main hung in 7 of 9 two-minute runs under gdb (no breakpoints) and in 1 of 7 plain runs; every
> dumped hang had the blocked thread in `cnd_wait` on a usage whose `unflushed` was already false.
> With this change: 0 of 13 (9 under gdb, 4 plain; 0.6–2.5 M iterations per run). A breakpoint on
> the waiter's `mtx_lock` makes it deterministic: main hangs on the first hit, the fixed code
> survives ~100k hits.
>
> The fix clears `unflushed` and broadcasts under the mutex, and waits in a loop on the predicate;
> the try-wait computes a real deadline.

## As sent

The commit — message, `Fixes:`, trimmed comments — is on branch `upstream/guest-2026-10` of
`liminavm/mesa`. It also uses `timespec_add_nsec()` from `util/timespec.h` for the try-wait
deadline (not `u_cnd_monotonic`, which would change the type of `zink_batch_usage::flush` for a
10 µs wait), and bounds the wait loop by the caller's `submit_count`: a batch state reused after
the awaited submission sets `unflushed` again for its next batch, which the bare
`while (u->unflushed)` would keep sleeping on. That second case was never observed; it is closed by
construction.
