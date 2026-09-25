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

## Candidate levers (not yet tried)

1. Hardware CRC32 (ARMv8 `crc32` instructions, e.g. the `crc32fast` crate): fixes the byte-wise
   CRC's cost on both save and restore.
2. Compress/zero-check straight from the guest mapping (drop the `read_slice` copy).
3. Revisit the 8-worker cap (the pool is CPU-bound).
4. Restore: mmap / streamed read so IO overlaps apply, instead of `fs::read` of the whole file.
5. The ~1 s vCPU re-online step before the bracket.
6. The missing splash on flat `--disk` runs (perceived latency, not real latency).

`sample-resume.sh` is the resume-sampling watcher used here.
