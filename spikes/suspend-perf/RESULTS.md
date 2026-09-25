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
- restore read: `fs::read` 0.60 s warm / 0.85 s cold → mmap 0.33 s / 0.36 s, apply unchanged
  (~0.65 s) even cold, so the page-in hides behind the decompress.

### What is left

1. The production write (1.9 s) is still ~2x the bench's null-sink compute; the file write is
   the difference. F_NOCACHE or preallocation are candidates; measure first.
2. The production apply (0.9–1.1 s) is ~1.5x the bench's (0.65 s): first-touch faults on
   HVF-mapped guest memory are the suspect, unmeasured.
3. The ~0.3 s left in restore `read` is mostly the serial decompress of the 598 MB GPU
   section; the save compresses it serially too.
4. Replay (~0.45 s) and quiesce (~0.2 s) are untouched.

`cycle.sh` runs the click-free suspend/resume cycles; `sample-resume.sh` is the resume-sampling
watcher used for the baseline profile.
