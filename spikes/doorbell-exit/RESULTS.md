# A doorbell trap costs ~1 µs on HVF, and the vCPU thread can do the Metal work itself

Question (from `spikes/metal-heap-addressing/SKETCH.md` §4): instead of a ring polled by a
host thread, could the guest driver ring a device doorbell page and let the trap run the host
work on the vCPU's own thread? Venus's ring costs 1.5 µs only while a host core spins, and
0.6–1.4 ms once that thread has parked (`spikes/metal-heap-addressing/RESULTS.md`). What does
the trap path cost?

The vehicle is a bare-metal guest (`payload.S`): EL1, MMU on, guest RAM Normal write-back,
device page Device-nGnRnE. It times each round trip with `CNTVCT_EL0`, which HVF does not
trap. A doorbell access is a stage-2 data abort and goes straight to the VMM whatever the
guest EL, so it is the trap a Linux userspace driver would take through a mapped doorbell
page. The host (`doorbell.m`) serves each exit on the thread that called `hv_vcpu_run`.

Each exit does one of four kinds of work before the guest resumes:
- **nothing**: the trap alone.
- **metal**: create a texture view and read its `gpuResourceID`, a real Metal query.
- **handoff**: pass the request to a worker thread parked on a condvar and wait for its reply,
  which is the shape of a dedicated device thread that does not spin.
- **spin**: pass it to a worker spinning on an atomic.

Gaps are guest busy-work between calls.

## Measured

2026-10-07, M1 Max, macOS 26.6.2, `./build.sh`. Three peer VMs were running (load average
3.4), so timings are indicative. Medians and tails from `runs/r1.txt`; `runs/r2.txt` repeats
it with the same picture. Every scenario's exit count matched, and so did the load doorbell's
returned value.

| exit | work | gap 0: p50 / p99 | gap 200 µs: p50 / p99 | gap 5 ms: p50 / p99 |
|---|---|---|---|---|
| store | nothing | 0.96 / 1.12 µs | 1.00 / 4.96 µs | 1.46 / 7.96 µs |
| load | nothing | 0.96 / 1.25 µs | 1.00 / 4.12 µs | 2.46 / 9.08 µs |
| hvc | nothing | 0.83 / 1.08 µs | 0.83 / 1.46 µs | 1.04 / 6.46 µs |
| load | metal (view + id) | 2.08 / 2.38 µs | 2.08 / 9.58 µs | 5.46 / 35.6 µs |
| load | handoff (parked worker) | 3.88 / 23.9 µs | 4.29 / 14.4 µs | 8.04 / 966 µs |
| load | spin (spinning worker) | 1.04 / 1.38 µs | 1.08 / 4.00 µs | 1.62 / 10.6 µs |

Medians in r2: trap 1.0–1.1 µs hot and 2–3 µs after 5 ms; metal 2.2–2.4 µs hot and 6.9 µs after
5 ms; handoff 11–12 µs. The timer-read floor is under 0.05 µs.

## What it says

1. **A trap round trip is ~1 µs, and stays within a few µs when the guest has been idle.**
   Against venus's 47–334 µs after 200 µs of idle, and 0.6–1.4 ms once parked, that is 40–1000×
   cheaper. No host core spins for it. It does not degrade the way a parked ring does, because
   the vCPU thread is the one that waits, and it is already running when the guest traps.
2. **A real Metal query done inline costs ~2.1 µs end to end.** That is about 1 µs of trap plus
   the Metal work. Synchronous calls stop being a design hazard: at ~2 µs each, the browser's
   27 round trips per frame would cost ~60 µs of a 20.8 ms frame. This holds for calls the
   design keeps synchronous anyway, such as texture size/align per new descriptor and pipeline
   cache misses.
3. **Handing off to a parked thread costs more than the trap and has a long tail.** It is
   4–12 µs median and up to ~1 ms p99 after idle, the wake-up cost venus pays at larger scale. A
   spinning worker matches inline only by burning a core. **Do the work on the vCPU thread**
   unless it is long. Long work, such as encoding a large command stream, is better queued to a
   worker while the vCPU returns to the guest; the doorbell is then only the "work is there"
   signal.
4. **HVC is ~0.15 µs cheaper than a doorbell access.** It is reachable only from the guest
   kernel, so it suits kernel-mediated calls. For userspace, the mapped doorbell page avoids the
   syscall entirely.

## Not covered

- **libkrun's own exit path.** `hvf/src/lib.rs` `run` does more per exit than this host loop:
  it checks for a pending interrupt, syncs the virtual timer, and dispatches MMIO through the
  device bus. That overhead adds to these numbers and is the next thing to measure, with a
  doorbell device in libkrun and a Linux-userspace caller.
- **Concurrency.** A vCPU thread doing Metal work blocks that vCPU for the duration, and several
  vCPUs may trap into one context at once. The replayer needs per-context ordering, by locking
  or by routing a context to one vCPU at a time.
- **The cross-process hop.** The replayer will be its own process
  (`spikes/metal-heap-addressing/SKETCH.md` §4). The trap then wakes the replayer instead of
  doing the Metal work itself. The worker resumes the guest without waiting for bulk work, and
  only the rare synchronous queries pay the hop. Its cost is not measured yet.
