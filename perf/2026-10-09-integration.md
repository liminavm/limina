# Integration pass: input validation, the per-VM cache key and the KK fixes

**Subject:** the stack landed in limina `b992c5a4`. It has host KK `19e3ad7ebaa` (msl type inference
and the RT-format bitcast), virglrs `a22cb8d` (venus input validation, signed pipeline-cache data),
and libkrun `8d9dfe0c` (the pipeline-cache key, kept per VM).
- **Guest:** enhanced F44, mesa `26.2.3-4.limina`, kernel `7.1.13-limina16k`; 4 vCPUs, 4 GiB,
  1280x800 @ 1.0, 60 Hz.

Drivers, rows and evidence are in `perf/integration-2026-10-09/`; nothing went to `perf/ledger.csv`.

## What this pass can and cannot see

- **One arm, read against `perf/tier0-2026-10-05/`'s tier0 rows.** It cannot attribute a
  difference to any one change. Since those rows, host KK moved 32 commits, virglrs 97, libkrun 23
  and limina 153, and the guest mesa went from 26.2.3-3 to -4.
- **The host read low all day.** The `gl-replay-llvmpipe` control read 707-711 on every
  measured point (703 on the warm-up). That is below its healthy 717-734 band and about 3% under
  10-05's 725-736. Small differences against 10-05 are therefore host, not stack, until shown
  otherwise.
- **Caches were warm.** virglrs ignores pipeline-cache data it did not sign, and the image's
  saved caches predate signing. So `w0` booted the pass's own image in place with a fixed
  `--gpu-cache-key`, and each measured point booted a clone of it with the same key. Every
  measured point's worker log has 2 caches accepted and 0 ignored. `w0`'s rows are a warm-up,
  not a measurement: its vk-replay read 1116, about half of the warm points.
- **Logs:** no point aborted or wedged. No worker log has a poisoned-context or
  `[virglrs] refused:` line.

## Results

Three points, one boot each, measured 2026-10-09, against 10-05's tier0 arm (t0 t1):

| workload | p1 p2 p3 | tier0 10-05 |
|---|---|---|
| gl-replay-llvmpipe (control) | 711 709 708 | 731 732 |
| gl-replay-venus | 51.3 51.3 51.2 | 51.0 50.9 |
| vk-replay-venus-headless | 2238 2199 2217 | 2372 2341 |
| glmark2-wayland-venus | 2559 2704 2684 | 2855 2854 |
| vkmark-default-venus | 3076-3088, 3082-3086, 3048-3183 | 3128-3138 |
| aquarium 25k (vrend) | 46 51, 46 46, 50 46 | 46 47, 47 49 |
| aquarium 30k (vrend) | 39 39, 38 39, 41 43 | 38 38, 39 40 |

- **No regression large enough to stand clear of the low control.** gl-replay-venus and both
  aquarium counts match 10-05. vkmark is about 1.5% down and vk-replay about 6% down, each the
  same order as the control's 3% drop.
- **glmark2 is the one to watch.** It reads 2559-2704 against 2854-2855, 5-10% down, and its
  three points spread 145 apart where 10-05's spread 1. That is more than the control explains.
  This pass cannot say which change moved it; an A/B against the pushed stack on one host would.
