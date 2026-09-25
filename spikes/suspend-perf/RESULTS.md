# Suspend/resume timing on a lived-in 10 GiB guest (2026-09-25)

Baseline measurement before optimizing. Vehicle: `Fedora-Workstation-44.enhanced.test.raw` clone,
EFI + venus, 6 vCPUs, `--ram-mib 10240`, **release** `limina` + `limina-vmm` (HEAD 6fb303fc),
M4 Pro (14 cores). Firefox with 15 tabs (YouTube, Maps, Reddit, news sites, GitHub...) →
guest `used` 6.9 GiB + 2.4 GiB page cache, 1.9 GiB free. Suspend = SIGTSTP to the supervisor (the
menu path: vCPU re-online, bracket, s2idle, save); resume = play click on the parked window.

## Suspend (3 generations)

| phase | gen 1 | gen 2 | gen 3 |
|---|---|---|---|
| vCPU re-online (#41) → bracket SIGTSTP | ~1 s ¹ | ~1 s ¹ | ~1 s ¹ |
| guest s2idle quiesce | 0.27 s | 0.32 s | 0.21 s |
| GPU fence drain + capture (598 MB section, 569 MiB classic contents) | 0.6 s | 0.5 s | 0.3 s |
| RAM stream write (10242 MiB →) | 3.7 s (2913 MiB) | 3.8 s (3583 MiB) | 3.7 s (3582 MiB) |
| **trigger → worker exit** | **~5.6 s** | **~5.6 s** | **~5.2 s** |

¹ supervisor log lines are second-granular; the `control` line to the worker's `SIGTSTP received`
line spans 1–1.6 s. Worth a sub-second timestamp before optimizing it.

Only 201–392 of 2561 4 MiB chunks were all-zero: a lived-in guest leaves the zero-hole
optimization little to skip.

## Resume (click → first presented frame)

| phase | gen 1 | gen 2 |
|---|---|---|
| click → worker up → snapshot `fs::read` + validate | ~1.0 s + 1.0 s | ~1 s + 1.1 s |
| RAM apply (10242 MiB) | 2.5 s | 3.0 s |
| GPU payload staging + replay (DRIVER_OK held) | 0.16 + 0.47 s | ~0.54 s |
| **first frame presented** | **4.5 s** | **5.5 s** |

Also: `restore splash unreadable at …limina-suspend.splash.png`. The last-scanout splash was not
written for this flat `--disk` run, so the window showed nothing in the gap.

## Profile of the RAM write (`sample`, 1 ms, from quiesce to exit)

The 8 `write_streaming` pool threads (`ram_workers()` = min(ncpu−2, 8)) were **100% busy**, with
each thread's time split into three roughly equal parts:

- `lz4_flex::block::compress_internal`: ~34%
- `GuestMemory::read_slice` → `memmove`: ~31%. Every 4 MiB chunk is copied out of guest RAM
  into `inbuf` before the zero check and compression, which could read the mapping in place.
- closure self-time: ~34%. This is the inlined **byte-at-a-time table `crc32`**
  (`snapshot.rs:220`) over every lz4 frame, plus `is_all_zero`.

The writer thread was idle in `recv` 79% of the time, and `write()` itself took only 442/2426
samples, so **disk I/O is not the bottleneck; CPU in the pool is**. The input rate is 10 GiB / 3.7 s
≈ 2.8 GB/s, about 1% of the M4 Pro's memory bandwidth, so the premise behind the cap at 8
("memory-bandwidth-bound past ~8 workers") doesn't hold at this rate.

The resume profile only caught the tail of the apply, because `sample` attaches late. Even so, the
apply pool threads spend most of their time in closure self-time: the same byte-wise `crc32`
verifying every frame before `decompress_into`. The main thread `fs::read`s the whole 3–3.6 GB file
before any apply starts, which accounts for the ~1 s "read + validated" line.

## After (same guest and disk, 2026-09-25): suspend ~2.6 s, resume 2.2 s to first frame

Measured with `cycle.sh` (below): launch → auto-restore → ssh login → SIGTSTP → park, five
cycles over two builds. The baseline above ran libkrun one commit behind its manifest pin (the
vendored tree had drifted); `cargo xtask vendor` fixed that before these numbers.

| phase | before | after | change |
|---|---|---|---|
| vCPU confirm wait (#41) | ~1–1.6 s | 0 (skipped: all online, no shrink outstanding) | limina `control.rs` |
| guest s2idle quiesce | 0.2–0.3 s | 0.17–0.28 s | — |
| GPU capture | 0.3–0.6 s | 0.3–0.4 s | — |
| RAM write | 3.7 s | **1.9 s** | libkrun: hardware CRC32, pool uncapped |
| restore read + validate | 1.0–1.1 s | **0.5 s** | libkrun: mmap instead of `fs::read` |
| restore RAM apply | 2.5–3.0 s | **0.9–1.1 s** (2.0 s once, cold file) | libkrun: hardware CRC32 |
| GPU replay | 0.47–0.54 s | 0.41–0.48 s | — |
| restore: first frame | 4.5–5.5 s (from click, parked) | **2.2 s** (from window open) | + splash now shown |

The restore splash was never saved on a guest-driven suspend: s2idle entry disables the scanout,
and the window's `scanoutgone` handling cleared the surface id the save needed. Fixed, and the
saved PNG was pixel-checked to be the desktop as suspended.

### bench_real_snapshot (libkrun `snapshot::tests`, ignored; the gen-3 snapshot as fixture)

`LIMINA_SNAPSHOT_BENCH=<file> [LIMINA_SNAPSHOT_BENCH_SINK=null] cargo test --release -p krun-vmm
--lib snapshot::tests::bench_real_snapshot -- --ignored --nocapture` in `third_party/libkrun`.
Host noise (a 10-vCPU dogfood VM alongside) is large; compare minimums.

- crc32 on one core: 0.5 GB/s (table) → 11 GB/s (`crc32fast`), with the checksum unchanged.
- apply_ram: 1.5 s → 0.8 s (CRC). write_streaming to a real file: min 2.3 s → 1.2 s (CRC).
- write_streaming to /dev/null by pool width: 8 workers 1.13 s, 11 workers 0.92 s, 14 workers 0.87 s.
- Rejected, **measured no gain**: compressing/decompressing in place from the guest mapping
  (dropping the `read_slice` bounce copy): 1.10 vs 1.12 s save, 0.62 vs 0.64 s apply. Once the
  CRC was fast, the pool became lz4-bound and the memcpy stopped mattering.
- ~~restore read: `fs::read` → mmap, apply unchanged even cold~~ **Wrong, and reverted.** The bench ran
  a single-thread CRC pass over the whole file *before* `apply_ram`, which faulted the mapping in,
  so the apply never paid the page-in. See "Correction" below.

## Correction and second pass (2026-09-25, later)

Two of the "after" numbers above were taken **on battery in Low Power Mode** (the dogfood Mac was
unplugged at 22%), which roughly halves the snapshot pool's speed: the null-sink save measured
1.8 s on battery vs 0.9 s on AC for the same code. **Compare timings only within one power state,
and check `pmset -g batt` before a run.** On AC the production save is 1.1 s (1.5 s once), not 1.9 s.

The save and apply now log a phase split (libkrun 22f7fb31), and the bench prints the same:

- **Save, AC:** head encode 0.2 s (serial: the GPU section's lz4) + max(pool ~0.8 s, writer ~0.8 s
  in `write`, 4.4 GB/s). The writer rarely holds the pool up; the earlier lead "the file write is
  the difference" came from comparing runs across power states.
- **Restore, AC, corrected bench** (CRC pass moved after the apply), read + apply of the 3.5 GB file:

  | read strategy | cold | warm |
  |---|---|---|
  | `fs::read` | **1.48 s** | 1.27 s |
  | mmap (shipped in fabcc2e9) | 1.87 s | 0.97 s |
  | mmap + `MADV_WILLNEED` | 2.29 s | 1.04 s |
  | parallel 8 MiB `pread`s | 1.51 s | 1.15 s |

  A mapped file is faulted in by the apply pool with small synchronous reads: +0.4 s on the cold
  read a real resume usually is. `WILLNEED` reads the whole file synchronously at ~2.3 GB/s.
  Parallel preads read at the same rate as `fs::read`. **Reverted to `fs::read`** (5d368dea).
  A fresh APFS clone (`cp -c`) of the file reads at SSD speed (0.55 s vs 0.19 s cached), so a clone
  is an honest cold run without `purge`.

### Streamed restore (libkrun 3da68ffe) and the state it leaves

A thread streams the file into memory and publishes how much has landed; the head parse and the
frame walk each wait only for the bytes they need, and the walk hands frames to the apply pool as
they arrive, so the IO overlaps the GPU section's decompress and the RAM apply.

- Bench, read + apply: cold **1.48 → 0.95 s**, warm **1.27 → 0.90 s**. The pool's decode time cold
  (~3.7 s summed) now matches warm: the IO is hidden.
- Production, 3 cycles on AC with a quiet host (libkrun c33afce4, virglrs 828a05bd): head 0.3 s,
  apply 0.6–0.7 s, replay 0.29–0.33 s, **first frame 1.3–1.8 s after the window opens**; suspend
  write 1.1–1.3 s (head encode 0.2 s + writer ~0.9 s ≈ pool ~0.85 s).
- A run while a game was loading the host (swap 6.4 GB used) took 18 s to suspend and 9–10 s to
  resume: the pool is CPU-bound and has no priority over what else the user is doing.

### What is left

1. Save: head encode 0.2 s is serial (the GPU section's single lz4 block); the writer and the pool
   are now balanced at ~0.85 s each.
2. Restore: storing into fresh guest RAM (first-touch faults) is about half the apply pool's time.
3. Quiesce (~0.2 s) and replay (~0.3 s) are untouched.
4. Under host contention both paths degrade badly; nothing here is prioritised.

`cycle.sh` runs the click-free suspend/resume cycles; `sample-resume.sh` is the resume-sampling
watcher used for the baseline profile.
