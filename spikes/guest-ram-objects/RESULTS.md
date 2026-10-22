# guest-ram-objects: guest RAM as owned, ledger-tagged memory objects

**Date**: 2026-10-22. Host: M1 Max, 32 GB, 16 KiB pages, Darwin 27.0.0. Two other VMs were running
(a 12 GiB build VM and a 4 GiB test VM), so the host-wide compressor column is noisy; the per-task
columns are not affected.
**Question** (`docs/design/guest-ram-objects.md` §4.1): does guest RAM made of owned,
ledger-tagged memory objects bill a page once when both the worker and the guest touch it, and does
replacing an object free its resident and compressed pages?

**Method**: `probe.c` + `payload.S` (the guest payload of `spikes/hv-ledger-marker`): a 3 GiB HVF VM
with one vCPU. The host memsets a 1 GiB range H, the guest then dirties one u64 in every 16 KiB page
of it, then `MADV_FREE_REUSABLE` on H while it is still stage-2 mapped, then release H
(`hv_vm_unmap`, then: `MADV_FREE_REUSABLE` for anon as production does, a fresh tagged object over
each block otherwise), then the guest re-touches H through a heal. `task_vm_info` is read around
every step. Legs: `anon` (production's `MAP_ANON|MAP_PRIVATE`), `hvalloc` (`hv_vm_allocate`),
`tagged:<KiB>` (a no-access reservation filled with `MAP_MEM_NAMED_CREATE|MAP_MEM_LEDGER_TAGGED`
entries mapped `VM_FLAGS_OVERWRITE`), `purgeable:<KiB>` (the same plus `MAP_MEM_PURGABLE`).
Run: `./build.sh`, then `./probe <leg> [compressed]`, sandbox off.

## Results

| leg | host + guest touch the same 1 GiB | REUSABLE while mapped | release of 1 GiB | content after |
|---|---|---|---|---|
| anon | footprint **+1024 M twice: 2x** | footprint −2048 M, resident unchanged | one full share stays resident (1030 M), reusable but not freed; 8.7 ms | old bytes |
| hvalloc | footprint +1024 M, then **+0.5 M: 1x** | nothing | footprint −1025 M, **resident −2048 M**; 14 ms | zeros |
| tagged:2048 | **1x** | nothing | as hvalloc; 14 ms | zeros |
| tagged:16 | **1x** | nothing | as hvalloc; **256 ms** | zeros |
| purgeable:2048 | **1x** | nothing | as hvalloc; 13 ms | zeros |

- **Single billing holds through HVF's stage-2 mapping.** On every owned-object leg the guest's
  touch of host-touched pages adds nothing to `phys_footprint`. `internal` still counts both
  mappings (2049 M), so the single billing is alternate accounting, as xnu's source predicts.
- **`MADV_FREE_REUSABLE` does nothing** on owned objects, as predicted: the release primitive is
  replacing the object.
- **Replacing a block frees its pages at once**: resident falls by both mappings' worth and stays
  down a second later; the new block reads as zeros, so zero-on-release is unnecessary.
- **One `hv_vm_map` spans many map entries**: 3 GiB in 1.1 ms as 2 MiB blocks (1,536 entries) and in
  58 ms as 16 KiB blocks (196,608 entries). Allocation: 7 ms vs 415 ms.
- **16 KiB blocks are too expensive**: allocation, mapping and release cost 30–60x what 2 MiB
  blocks do, and 24 GiB would need 1.5 M map entries. Use 2 MiB blocks.
- anon's REUSABLE on a stage-2-mapped range now debits the footprint at once while the pages stay
  resident. `spikes/hv-ledger-marker` (macOS 26.5) measured no debit at all for the same step.
  The OS changed here; whatever relied on the old behaviour should be re-measured.

**At 24 GiB** (`./probe <leg> resident 24`; only the 1 GiB range is touched):

| leg | allocate | one `hv_vm_map` | release 1 GiB |
|---|---|---|---|
| anon | 0 | 0.08 ms | 8 ms |
| hvalloc (128 MiB entries) | 1 ms | 0.2 ms | 13 ms |
| tagged:2048 (12,288 entries) | 39 ms | 3.5 ms | 13 ms |
| tagged:16 (1,572,864 entries) | 3.2 s | 491 ms | 244 ms |

2 MiB blocks cost a 24 GiB VM 43 ms at boot; 16 KiB blocks cost 3.7 s and a quarter second per
released GiB.

**Compressed cell, partial.** `MADV_PAGEOUT` is refused from userspace (`ENOTSUP`), and 40 GiB of
ballast compressed only ~150 MB of H: the kernel compressed the other VMs' idle memory first. On
that sample (tagged:2048) the release dropped the task's compressed share from 149.6 M to 1.3 M at
once. The host-wide compressor fell by 74 M, which the other VMs' traffic makes unreadable. Owed: the
same leg on an idle host, or with H made older than everything else, before retiring REUSABLE.

## Verdict

No blocker for direction A: owned objects bill once, release by replacement frees resident pages
immediately, and 2 MiB is the block size. The compressed-page half is consistent but not yet
measured cleanly.
