# An out-of-process Metal replayer: shared arenas work, and a hop costs ~3 µs

These are the two questions `spikes/metal-heap-addressing/SKETCH.md` §4 raises once the Metal
replayer is its own process:
1. How does arena memory become visible to the replayer (Metal), the worker (`hv_vm_map`) and
   the guest, and what happens to it when the replayer dies?
2. What does a cross-process wake-up round trip cost?

`ipc.m` is one binary with two roles. The parent is the **replayer** and owns Metal. It spawns
itself as the **worker**, which owns a VM and a bare-metal guest (`payload.S`). The guest reads
and writes the shared memory through stage 2, so "the guest sees it" is checked by guest
accesses, not inferred from `hv_vm_map` returning success. The replayer's port reaches the
worker as an inherited registered port (`mach_ports_register`).

Measured 2026-10-07 on an M1 Max, macOS 26.6.2, `./build.sh`. The host was loaded: one to three
peer VMs, load average 10–20. Correctness results do not depend on load; the latency tails are
inflated by it.
- `runs/r1.txt`, `runs/r2.txt`: parts A and B.
- `runs/r3.txt`: adds part C.

## A. Arena memory across the two processes

**A1. Replayer-owned heap, exported to the worker.** The replayer makes a 256 MiB placement heap
with KK's options plus a buffer aliasing all of it. `mach_make_memory_entry_64` on the alias's
contents succeeds with no special flags, and the entry covers the whole heap.
- The worker `mach_vm_map`s the entry, and `hv_vm_map`s that mapping into its VM.
- Words the GPU wrote: correct in the worker's CPU mapping (25,165,824 of 25,165,824) and in 64
  of 64 guest reads.
- Words the worker's CPU filled, and a word the guest wrote: correct when the replayer's GPU
  and CPU read them (0 bad).
- A texture placed in the heap: all 65,536 of its tagged texels appear in the worker's mapping.
  Texture memory is the same shared pages.
- **The replayer releases the heap.** The worker's mapping and the guest still hold the old
  data, and none of the replayer's newly allocated, GPU-filled heaps show through. The worker's
  mapping keeps the pages alive.

**C. The replayer is killed with SIGKILL** while the worker and its guest map a heap it owns.
1 s and 4 s later, all 67,108,864 words still read correctly in the worker, and 64 of 64 in the
guest. **Replayer-owned arenas survive the replayer's death** as long as the worker maps them.
The Metal objects are gone, though, so a new replayer cannot rebuild a placement heap over that
memory. It can only wrap it with `newBufferWithBytesNoCopy` (A2), and its textures are lost,
which is a lost device.

**A2. Worker-owned memory, wrapped by the replayer.** The worker creates a named memory entry
(`MAP_MEM_NAMED_CREATE`), maps it, `hv_vm_map`s it, and sends it. The replayer maps it, and
`newBufferWithBytesNoCopy` accepts the whole 256 MiB.
- GPU check of what the worker wrote: 0 bad.
- What the GPU wrote: correct in the worker's CPU mapping and in 64 of 64 guest reads.
- After the replayer drops the buffer and unmaps the memory, the worker's data is intact.

## B. Wake-up round trips, worker → replayer → worker

The worker calls and waits for the replayer's reply. Each run has 20,000 calls back to back,
2,000 calls with a 200 µs busy gap, and 200 calls with a 5 ms gap. Values are p50 / p99 in µs,
from `runs/r2.txt`, with r1 in parentheses where it differs much.

| primitive | cross-process gap 0 | gap 200 µs | gap 5 ms | in-process gap 0 | in-process gap 5 ms |
|---|---|---|---|---|---|
| Mach semaphore pair | 2.8 / 7.4 | 3.5 / 12 | 5.9 / 15 | 2.5 / 6.1 | 5.1 / 19 |
| Mach message RPC (send + receive in one `mach_msg`) | 3.0 / 7.3 | 3.8 / 20 | 9.1 / 24 | 2.6 / 6.8 | 5.2 / 26 |
| `os_sync_wait_on_address`, shared | 5.4 / 17 (5.1 / 47) | 5.8 / 22 (14 / 67) | 10 / 527 (25 / 390) | 2.8 / 28 | 5.3 / 26 |
| spin on shared memory | 0.17 / 0.21 | 0.08 / 0.38 | 0.08 / 92 | 0.42 / 0.46 | 0.08 / 3.1 |

- **The process boundary itself costs well under 1 µs** for semaphores and Mach messages. Cross
  vs in-process: 2.8 vs 2.5 µs hot, 5.9 vs 5.1 µs after 5 ms. What costs is waking a parked
  thread, in either case.
- **Mach semaphores and Mach RPC are the choice**: about 3 µs hot, 4 µs with work in between,
  6–9 µs after 5 ms idle, and p99 within tens of µs. `os_sync_wait_on_address` across processes
  is about twice as slow, with tails into hundreds of µs. A spinning thread is ~0.1–0.4 µs but
  burns a core.
- These beat the in-process condvar handoff in `spikes/doorbell-exit/` (4–12 µs).

## What it says

1. **§1's memory model stands with the replayer out of process.** Replayer-owned placement
   heaps, buffers and twiddled textures, export to the worker as Mach memory entries with no
   special flags. The worker maps them into the guest, and every direction is coherent.
2. **Arena memory outlives a replayer crash in either ownership model.** The worker's mapping
   keeps the pages, so the guest never loses its mapped memory. A restarted replayer can wrap
   that memory for buffers again. Texture contents and every Metal object are lost, which is the
   lost-device contract. The sketch's "dies with the replayer" was wrong for the memory and right
   for the objects.
3. **A query-lane round trip is about 4–10 µs**: a ~1 µs doorbell trap plus a 3–9 µs Mach
   semaphore or RPC hop, against 47 µs–1.4 ms on venus's ring today. The bulk lane costs the
   worker one signal and lets the guest go on.
4. **Use Mach semaphores (or Mach RPC where a message carries data), not
   `os_sync_wait_on_address`,** for the cross-process wake.

## Not covered

- A real launchd-started replayer with its sandbox profile. The spike's spawn inherits a
  registered port; production would hand ports over the way `crates/limina-launch` does.
- The bulk lane's one-way signal cost, and how a replayer that keeps draining while busy
  amortises it.
- Snapshot/restore of replayer-owned arenas, and the balloon's exclusion of them.
