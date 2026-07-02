# Host memory overhead of a limina VM — measured + sourced (2026-07-02)

**Question (user):** a 12 GiB VM was observed costing up to ~20 GB host-side. Where does the
overhead come from, how much is intrinsic, what can be improved?

**TL;DR:**
- The number the user saw is `phys_footprint` (Activity Monitor "Memory") of `limina-vmm`. It
  counts: every guest page **ever touched and not yet reclaimed** + GPU memory (IOAccelerator/
  IOSurface billed to the worker) + compressed pages + page tables. `ps` RSS is a *different*,
  misleading number (it still counts already-reclaimed `MADV_FREE_REUSABLE` pages and can read
  *higher* than footprint, but misses compressed + IOKit).
- Guest RAM is lazily mapped (`MAP_ANON|MAP_NORESERVE`, no prefault; `hv_vm_map` wires nothing);
  the host cost is the guest's **touched-page high-water mark minus reclaim**.
- Reclaim exists and works TODAY, even on static `--ram-mib` VMs: free-page reporting →
  16 KiB-safe coalescing → `MADV_FREE_REUSABLE` (M6/FRQ; the balloon device is attached
  unconditionally). Measured live: guest frees 8 GiB → worker footprint 12.4 → 6.5 GB in ~90 s.
- The two terms FRQ can never return: **guest page cache** (never "free", so never reported —
  the dominant real-world ratchet) and **graphics working sets** (bounded by guest behavior and
  returned on free since the 0022/0041 unref fix, but pinned meanwhile — and guest mesa caches
  pin freed GPU memory up to `host_ram/8` per screen because KK advertises *host* RAM as the
  venus heap).
- ~20 GB for a 12 GiB VM is credible as: 12 GiB fully-touched guest RAM (incl. accumulated page
  cache) + 1–3 GB graphics under a WebGL-heavy desktop + ~0.5–0.7 GB fixed (binaries, malloc,
  supervisor, gvproxy) + compressed/pagetable remainder. Parallels calls 10–20% over the
  configured size "expected"; 20 GB/12 GiB (≈65%) says the *policy* (nothing driving memory
  back), not the mechanism, was the gap — plus the pre-fix venus region leak inflating things
  on long-lived sessions.
- **With `--memory 2048..12288` the same VM idles at 2.7 GB host, absorbs a 4 GiB page-cache
  workload at 2.6 GB, gives the guest 6 GiB back in seconds when it really needs it (9.2 GB),
  and returns to 2.7 GB five minutes after the demand ends.** The fix for the user-visible
  overhead is defaulting managed VMs to a memory range, not new mechanism.

## Measured (Run A: static `--ram-mib 12288`, enhanced 16k F44 guest, windowed venus desktop)

Worker `phys_footprint` (vmmap; = Activity Monitor):

| phase                                   | footprint | notes |
|-----------------------------------------|-----------|-------|
| A0 idle after boot (settled)            | 4.3 GB    | guest touched ~3.8 GiB booting; RSS reads 5.1 GiB |
| A1 guest allocs+touches 8 GiB (held)    | 12.4 GB   | ≈ full guest RAM touched; host compressor already active (RSS 7.1) |
| A1 +90 s after guest frees              | **6.5 GB**| **FRQ reclaim on a STATIC VM** — no balloon range needed |
| A2 fill 4 GiB guest page cache          | 8.4 GB    | page cache = host-resident anon memory |
| A2 +90 s after `drop_caches`            | 4.9 GB    | freed cache pages get reported + reclaimed |
| A3 glmark2 (score 1177)                 | 4.9 GB    | light GPU client, negligible footprint |
| A4 Firefox + WebGL aquarium (3 min in)  | 5.8 GB    | IOAccelerator 294→694 MB; ledger overhead +0.8 GB |
| A4 +90 s after Firefox closes           | 5.2 GB    | GPU memory returns fully (post-0022/0041); residue = guest cache + KK pipeline caches |

Ledger detail per phase in `footprint-*.txt` / `vmmap-summary-*.txt`; narrative log in
`memlog.txt`; snapshot tool `snap.sh` (wraps `spikes/venus-draw-probe/memsnap.sh`).

Measurement gotchas (cost us an hour; keep):
- **`footprint(1)`'s total does NOT include the hv-mapped guest RAM** (its ledger walk misses
  it) — it reports only the *overhead* slice (~1.3–2.2 GB in our phases). vmmap's
  "Physical footprint:" line and Activity Monitor DO include guest RAM. Divergent by design.
- The 12 GiB guest region is INVISIBLE in vmmap's region list (no GiB-scale region appears;
  region table TOTAL ≈ 1.6 GB resident while phys_footprint = 4.3 GB) — guest RAM hides in
  the gap between the two. Don't try to find it per-region; use the footprint delta.
- `ps` RSS counts not-yet-scavenged reusable pages: after a big reclaim RSS ≫ footprint.

## Measured (Run B: `--memory 2048..12288`, PSI autoballoon + limina-agent)

| phase | footprint | balloon `actual` | notes |
|-------|-----------|------------------|-------|
| B0 idle settled                    | **2.7 GB** | 10 GiB (FULL) | policy inflated to max within the settle window; guest at its 2 GiB floor. RSS reads 8.9 GiB — the RSS-vs-footprint trap in full display |
| B1 write+read 4 GiB (page cache)   | 2.6 GB | 10 GiB | **the ratchet is gone**: cache evicted as fast as it formed (guest cache peaked 569 MiB); Run A hit 8.4 GB on the same workload |
| B2 guest allocs 6 GiB of ZEROS     | 2.7 GB | 10 GiB | guest zram swallowed it (6.4 GiB → 220 MB compressed) — balloon + zram compose: compressible guest memory costs the host ~nothing |
| B3 guest allocs 6 GiB of urandom   | 9.2 GB | **0** | incompressible demand: balloon fully released (policy PSI release + DEFLATE_ON_OOM), guest got its memory, no OOM kill (568 MiB spilled to zram in the transition) |
| B3 +5 min after the free           | **2.7 GB** | 10 GiB | full round trip; cumulative reclaimed 39.9 GB over the run |

Dynamic memory works end-to-end exactly as designed: the host pays the guest's true working
set, page cache cannot accumulate, and real demand gets the memory back within seconds.

## Measured (Run C: the user's scenario — GPU-heavy tabs + stress-ng, static 12288)

Six WebGL tabs (aquarium ×2 @10k/30k fish, dynamic-cubemap, field, blob, electricflower),
then `swapoff` + `stress-ng --vm 4 --vm-bytes 7400M --vm-keep` (7.4 GB total — note stress-ng
SPLITS --vm-bytes across workers) pinning the guest to ~full.

| phase | footprint | notes |
|-------|-----------|-------|
| C0 idle                       | 5.0 GB  | (dnf install of stress-ng churned cache) |
| C1 six WebGL tabs settled     | 7.0 GB  | IOAccelerator 1.0 GB dirty, IOSurface 146 MB/47 surfaces, venus shmem pool 1.4 GB resident |
| C2 tabs + stress-ng (guest full) | **15.0 GB** | the "assigned 12 GiB → what does the host pay" answer |
| C3 stress-ng ends +2 min      | 8.6 GB  | FRQ returned ~6.4 GB |
| C4 Firefox closed +2 min      | 7.3 GB  | IOAccelerator → 284 MB, IOSurface → 20 MB — graphics returns |

**The gap, decomposed (C2: 15.0 GB total, guest RAM ≈ 12.0 GiB fully touched → gap ≈ 3.0 GB,
matching footprint(1)'s guest-RAM-excluding ledger of 3003 MB):**

| contributor | size | what it is |
|-------------|------|------------|
| untagged VM_ALLOCATE            | 1.47 GB | dominated by the venus shmem pool: guest-visible CS/reply/ring buffers + mtl_shm blob carriers (vmmap types it "shared memory", 20.6 G virtual / ~1.4 G hot; macOS pushed it to compressor/swap under the C2 pressure) |
| IOAccelerator (graphics), dirty | 1.10 GB | KK Metal heaps — the six tabs' WebGL textures/buffers/render targets |
| MALLOC_SMALL + LARGE            | 0.19 GB | worker heap |
| IOSurface                       | 0.15 GB | 48 window/scanout buffers |
| owned-unmapped (Metal internal) | 0.07 GB | |
| page tables + binaries + misc   | ~0.05 GB | |
| **total overhead**              | **≈3.0 GB** | + outside the worker: supervisor ~130 MB (debug build), gvproxy 30 MB |

So with the guest genuinely full, host cost = assigned + ~25% — all of it graphics + venus
transport + heap, and all of it *returns* when the workload ends (C3/C4). The user's original
~20 GB reading additionally contained the pre-fix venus context leak (each dead context kept
its ring+reply shmem mapped and its pages dirty — a long browsing session accumulates
thousands of contexts), fixed 2026-07-02 (patches/virglrenderer/0022 + libkrun/0041).

## Measured (Run D: what the full-inflation clawback costs the guest page cache)

Question: the idle policy inflates to the floor, permanently evicting the guest page cache —
how much does that actually hurt, and does it justify a host-pressure heuristic instead of
"always claw back when idle"? Suite: `cache-bench.sh` run identically in the squeezed state
(dynamic `2048..12288`, balloon at full 10 GiB) and the cache-friendly state (static 12288),
host UBC deliberately warm in both so the delta isolates the guest cache. 3 GiB incompressible
test file.

| test | squeezed | with cache (static) | penalty |
|------|----------|---------------------|---------|
| seq re-read 3 GiB           | 5.4 GB/s | 23.5 GB/s | 4.3× — but 5.4 GB/s via virtio+host-UBC is still fast; sequential is RESCUED by the host cache |
| 4k buffered randread (fio, invalidate=0) | 13.3k IOPS / 75 µs | **852k IOPS / 1.0 µs** | **64×** — the workload class that hurts: rpmdb/sqlite, git, app relaunch, code search |
| `du -s /usr` warm           | 0.93 s | 0.23 s | 4× (dentry/inode cache) |
| `rpm -qa` warm              | 0.48 s | 0.45 s | ~1× (small hot set survives even at the floor — ~400 MB cache remains) |
| claim 4 GiB anon            | 2.69 s | 2.28 s | +0.4 s balloon give-back latency — negligible |

(fio gotcha for posterity: fio defaults to `invalidate=1` — it wipes the page cache for the
test file before running, silently hiding the cache benefit; the honest cached number needs
`--invalidate=0`.)

**Reading.** The host UBC (we run buffered, `direct_io: false`) fully rescues *sequential*
re-reads and softens everything else — the guest cache's irreplaceable value is the ~1 µs
random hit, which the squeeze turns into a 75 µs virtio round trip, 64× slower. Fedora keeps
the hottest ~400 MB of slab/cache alive even at the 2 GiB floor, so small hot sets (rpm -qa)
survive; anything bigger (repo greps, sqlite scans, relaunching a fat app) pays 4–64×. The
benefit side is ~2–4 GB of host RAM at idle (2.7 vs 4.3–5.0 GB measured).

**Verdict: "always claw back when idle" is too aggressive as the default.** It buys host RAM
the host may not even need, at a real interactive cost. The data supports a two-input policy:
- **FRQ stays always-on** (it returns only truly-free pages — zero cache cost, and it alone
  handled the 8 GiB free→reclaim in Run A).
- **Balloon squeeze becomes host-pressure-driven**: at host pressure *normal*, inflate at most
  to `used + cache allowance` (leave the guest ~1–2 GiB of cache — restores the ~1 µs hits for
  typical hot sets) or don't inflate at all; at *warn*, current behavior; at *critical*, full
  squeeze to the floor. macOS exposes the signal (dispatch memory-pressure source /
  `kern.memorystatus_vm_pressure_level`) — this was already lever #2, now with numbers behind it.

## Where the memory goes (sourced; workflow `mem-overhead-sources`, 4 agents, all claims path:line-cited)

1. **Guest RAM (the big term).** One anon `MAP_NORESERVE` mmap sized at max
   (`vm-memory MmapRegion::new`; `builder.rs:1699`), `hv_vm_map`'d wholesale — HVF does not
   wire pages (proven in `spikes/balloon-madvise/RESULTS.md`). Cost = touched high-water −
   reclaimed. On macOS 26.5 the ONLY working reclaim primitive is `MADV_FREE_REUSABLE`
   (`MADV_DONTNEED`/`MADV_FREE` return 0 and free nothing) — QEMU-HVF's balloon (MADV_DONTNEED)
   is therefore likely footprint-ineffective on macOS; ours is not.
2. **Reclaim paths.** (a) FRQ: guest page_reporting hands ≥pageblock free runs; the
   `ReclaimCoalescer` REUSABLEs each 16 KiB host page only when all four 4 KiB guest sub-pages
   were reported (exact 1:1 on the 16k enhanced kernel) (`balloon/device.rs:66-137, 269-324`).
   Unconditional — works for static VMs (validated in Run A). (b) Balloon inflate, same
   REUSABLE path (`device.rs:326-454`); deflate deliberately skips REUSE (re-fault re-bills).
   The PSI policy driving (b) exists ONLY with `--memory MIN..MAX` (`main.rs:626-636`) and
   needs limina-agent mempressure reports.
3. **What FRQ cannot see: page cache.** Cache pages are never free → never reported → host
   keeps them until the *guest* evicts (its own pressure, or balloon pressure). This is the
   ratchet that walks every VM toward its configured size over days of use (and what peers'
   users observe on Vz: "grows 1 GB per GB copied, never returns", docker/for-mac#6120).
4. **Graphics (additive, on top of guest RAM).** All venus device memory = KK `MTLHeap`s in
   the worker (no VRAM); scanout/window buffers = IOSurface (15.6 MiB @2560×1600) + a dead
   same-size shm carrier each; venus ring/CS/reply shmem ~10–20 MiB per venus process;
   browser+WebGL ≈ 0.5–1.5 GB steady-state (measured: +0.9 GB). Bounded by guest lifetime
   (returns on close post-0022/0041) BUT guest zink's bo cache pins freed host memory up to
   `total_mem/8` where total_mem = KK's report = **host physical RAM** (32 GB → 4 GiB ceiling
   per screen) — an honest-heap-size clamp is a cheap lever. vrend/GL tier doubles texture
   memory (guest-RAM backing + host copy); venus does not.
5. **Fixed.** ~0.5–0.7 GB: worker text/malloc (~0.3 GB ledger at idle), supervisor (~130 MB
   debug / less release), gvproxy 30 MB, firmware 2 MiB, page tables ~5 MB. The 8 GiB virtio-gpu
   shm window and 436 GiB VSZ are address space only.

## Peer context (web-researched)

Parallels documents 10–20% over configured size as normal, and points users at `footprint -p`
for the known Activity-Monitor double-counting bug (KB 128437). No Vz-based peer (UTM, Lima,
Tart, Docker) can return memory at all — Vz exposes only a traditional balloon with no
public host-side reclaim story; Docker's answer is stopping the VM (Resource Saver); OrbStack
ships undisclosed dynamic memory. **limina's mechanism (FRQ + MADV_FREE_REUSABLE) is ahead of
every peer; the gap is policy** — nothing drives a static VM's target, and page cache needs
pressure to leave.

## Improvement levers (ranked)

1. **Default managed VMs to a memory range** (vm.toml `memory = "MIN..MAX"`) or instantiate the
   balloon policy for static VMs too, with a conservative floor. Mechanism shipped; policy-only.
   Largest win: idle VMs converge to working set instead of high-water mark.
2. **Host-pressure input to the policy** (macOS memory-pressure dispatch source in the
   supervisor): inflate when the *Mac* needs memory, not only when the guest is idle. This is
   the Parallels-like behavior users expect.
3. **Clamp the venus-advertised heap** (KK memory-properties report or vkr passthrough) to
   ~min(host/2, guest RAM): drops guest zink's bo-cache ceiling 4 GiB → ~1–1.5 GiB and makes
   guest apps budget sanely. Small patch, needs a WebGL perf A/B.
4. **Drop the dead mtl_shm carrier** on IOSurface-backed window buffers (export as
   OPAQUE_HANDLE with map_ptr = IOSurface base — #28 mechanism already exists): ~2 regions +
   15.6 MiB vsize per window buffer.
5. **Lazy sw2d present machinery** (3-ring + staging + canvas ≈ 78 MiB @2560×1600 allocated on
   every venus mode-set, never written on the zero-copy path): allocate on first sw2d present.
6. **Wire the balloon stats queue** (drained and ignored today) → guest free/cache/available
   visible to the host policy without the agent → smarter targets on stock guests.
7. Guest-side (enhanced tier): agent-driven cache trimming / MGLRU tuning before inflate;
   later virtio-pmem/DAX to kill double page-cache for disk images (big, deferred).
8. Post-M9: Docker-style idle suspend for near-zero idle cost (blunt but peer-proven).

## Verdict on "intrinsic"

The *fixed* overhead is small (~0.5 GB). The big numbers are not intrinsic:
- touched-then-freed guest RAM → already returns via FRQ (works, measured);
- page cache → returnable with balloon pressure (Run B / lever 1–2);
- graphics working set → returns on free since 0022/0041; cache-pinning fixable (lever 3);
- compressed/pagetable → follows the above down.
What IS intrinsic: the guest's *actual working set* + ~0.5 GB fixed + whatever GPU memory live
guest windows genuinely need. A desktop 12 GiB VM that idles at ~3–4 GB host and peaks near
12–14 GB under load is the realistic target with levers 1–3.

## Artifacts

`memlog.txt` (narrative), `footprint-<tag>.txt` + `vmmap-summary-<tag>.txt` per phase,
`snap.sh`. Workflow agent reports (full JSON incl. citations):
`~/.claude/.../workflows/wf_1d88845e-991` journal; key facts inlined above.
