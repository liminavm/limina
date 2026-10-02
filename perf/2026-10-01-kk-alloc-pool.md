# A/B: the KosmicKrisp command-allocator pool

**Subject:** limina-kk's device-wide command-allocator pool (mesa `237a51460f7`), against the same
branch without it. Without the pool, upstream KosmicKrisp's never-reset allocators ratchet: the
memory side is measured in `spikes/kk-alloc-pool/RESULTS.md`. This pass measures what the pool
costs in frame rate, and it is what set the pool's default budget to 16 MiB.

**Builds:** host mesa `fab44a91147` for the base arm (the pool commit's parent, in
`/Volumes/mesa-cs/build-kk-ratchet`). The pool arms ran `5d248de2541` (`/Volumes/mesa-cs/build-kk`),
which differs from `237a51460f7` only in the default budget, so every pool arm names its budget.
virglrs `79fccb5`, libkrun `53522ebd`, limina `c26902c1`/`0a5179a7`. The rows, evidence and drivers
are in `perf/kk-alloc-pool-2026-10-01/`; nothing went to `perf/ledger.csv`.

## What this pass can and cannot see

- **One guest:** enhanced F44. Each point ran aquarium 25k/30k twice, perf-ledger once, and vkmark
  three times. The aquarium is the vrend reading (zink-on-KK, one long-lived KK device). The rest
  are venus, where every client gets its own KK device.
- **The arms really differed.** Both ran with `LIMINA_KK_ALLOC_STATS=20000`, and only the pool
  build prints pool stats lines: every base point counted 0, and every pool point 115-150
  (`evidence/*/arm-check.txt`).
- **The budget points ran after the bracketed sweep,** without a base point after them. The
  `gl-replay-llvmpipe` control reads 724-732 in every point, so the host did not drift.
- No poisoned-context marker appeared in any worker log.

## Results

The points ran in the order b0 p0 b1 p1 b2 at the 4 MiB budget, then the 16 and 64 MiB points. One
boot each, measured 2026-10-01.

| workload | b0 base | p0 4 MiB | b1 base | p1 4 MiB | b2 base | 16 MiB | 64 MiB |
|---|---|---|---|---|---|---|---|
| gl-replay-llvmpipe (control) | 731 | 728 | 728 | 724 | 732 | 729 | 730 |
| gl-replay-venus | 46.6 | 46.7 | 46.6 | 46.7 | 46.5 | 46.3 | 46.5 |
| vk-replay-venus-headless | 1837 | 730 | 1916 | 869 | 1962 | 1864 | 1886 |
| glmark2-wayland-venus | 2669 | 1658 | 2647 | 1656 | 2654 | 2626 | 2602 |
| vkmark-default-venus | 2581 2555 2529 | 1851 1864 1857 | 2548 2562 2531 | 1847 1852 1848 | 2564 2540 2533 | 2522 2525 2520 | 2532 2530 2530 |
| aquarium 25k (vrend) | 50 45 | 49 45 | 50 44 | 45 44 | 50 45 | 45 44 | 42 50 |
| aquarium 30k (vrend) | 37 38 | 40 38 | 41 39 | 38 40 | 42 38 | 38 39 | 43 43 |

**At 4 MiB the pool costs venus heavily; at 16 MiB it does not.**

- At 4 MiB: vkmark loses 27% (2529-2581 → 1847-1864), glmark2 37%, and vk-replay 55-60%. Both
  pool points agree to within 1%.
- The cause is churn, read off the pool stats in `evidence/p0-pool/worker-enh.log`. A venus
  client's allocator settles near 13 MiB, so at a 4 MiB budget it crossed the budget every ~13
  recordings, and was released and replaced by a fresh allocator about 35 000 times per client
  run.
- At 16 MiB a venus client keeps one allocator for its whole life. vkmark (2520-2525),
  glmark2 (2626) and vk-replay (1864) land at or just under the bottom of the base range. That is
  a gap of 1-2% from one unbracketed point, so it is a lead at most, not a measured cost.
- **The aquarium shows nothing at any budget.** 25k reads 44-50 and 30k reads 37-43 across all
  seven points. The vrend device's churn at 4 MiB (about 35 replacements a second) does not reach
  the frame rate.
- `gl-replay-venus` is flat everywhere, at 46.3-46.7.

**What 16 MiB costs in memory:** the vrend device's allocators hold about 200 MiB instead of
about 60 MiB, still flat across launches. Upstream's reach 1.5 GiB after eight launches and keep
climbing (`spikes/kk-alloc-pool/RESULTS.md`). 64 MiB buys nothing the 16 MiB point does not, and
holds 270-385 MiB.
