# balloon-ab-compile: pre-fix vs post-fix under a real compile mix

**Date**: 2026-08-11. Host: macOS 26.5, M1 Max 32 GB (quiet), 16 KiB pages.
**Question**: what does the balloon stage-2 unmap fix (libkrun `1763801..d1b0a5a`) cost on a
real workload, and does the pre-fix billing pathology replicate at scale?

## Setup

One primed image (clone of `f44-kbuild.raw`: linux tree + `make defconfig`, mesa 26.1.6 +
full builddeps, synoik source + prefetched cargo deps), cloned per side so both runs are
byte-identical. Workload: `linux defconfig -j8` → `meson/ninja mesa (virgl,llvmpipe,virtio)`
→ `cargo build --release synoik`, sequential, timed in-guest. VM: 8 vCPUs,
`--memory 2G..12G --balloon-free-page-reporting`, headless EFI, NAT.

- **A (pre-fix)**: limina `3727860` + libkrun `d29e394` (REUSABLE-only release, no unmap,
  no fault healing), built in a worktree.
- **B (post-fix)**: limina `9eece9f`-era HEAD + libkrun `b6b2c99` (unmap + REUSABLE, stage-2
  fault heal, counters).

Instrumentation: host ledger sampler (10 s: internal/reusable/phys_footprint balances +
system compressor), in-guest meminfo logger. Caveat discovered at analysis: the kbuild
guest is STOCK tier — no limina-agent, so no PSI reports, so the **autoballoon policy never
ticked** on either side (and the balloon-trace journal stayed empty — it only writes on
policy ticks). Both runs therefore exercised the kernel-autonomous FRQ path only; fair A/B,
but no inflate/deflate cycling and no per-run heal counter capture.

## Timings — the fix is free

| Phase | A (pre-fix) | B (post-fix) |
|---|---|---|
| linux defconfig | 225 s | 226 s |
| mesa | 61 s | 61 s |
| synoik | 197 s | 196 s |
| total | 494 s | 493 s |

Identical within 0.2 % — the unmap/remap syscalls and stage-2 fault heals cost nothing
measurable on a compile mix. (The M6-era fear that faulting would tax refills does not
materialize at this workload's churn.)

## Ledger behavior — the pre-fix limbo replicates in miniature

Guest end state was identical on both sides (MemTotal 11.64 G, ~4.6 G live, ~10.1 G
touched-non-free). Host ledgers diverged exactly as the hv-ledger-marker model predicts:

|  | A (pre-fix) | B (post-fix) |
|---|---|---|
| internal end | 9.7 G | 14.1 G |
| **reusable end** | **6.8 G, monotonic 0.1→6.8, never scavenged** | **≈ 0 the whole run** |
| phys_footprint end (peak) | 9.7 G (10.6) | 14.1 G (16.3) |

- **B never has limbo**: every release settles instantly (unmap first), `reusable` stays
  ~0. Its higher `internal` is honest billing of what the guest actually holds non-free
  (page cache ratchet — expected without a policy squeezing, FRQ only reports *free*
  pages).
- **A accumulates**: 6.8 G parked in `reusable` (marked but never scavenged on a
  pressure-free host) and an unquantifiable limbo share inside `internal` — the same
  deferred-settlement behavior the marker probe isolated, now visible in a real workload's
  ledger. int+reus totals 16.5 G vs B's 14.3 G against identical guest states.

## Stray faults — 6 on B, 0 on A: a real fix-era race, with a data-loss fallthrough

B logged 6 `released-ram: stage-2 translation fault ... no released range covering it`
errors during the run (2 in the first minute). A logged none (nothing is ever unmapped
pre-fix). Mechanism: a vCPU faults on a released range while another vCPU's heal (holding
the released-set lock) heals it; the loser finds the set empty → `FaultOutcome::NotHandled`
→ **falls through to the MMIO decode path, which consumes the instruction**
(`pending_advance_pc`) and emulates the RAM access against no device — the guest's
load/store is silently swallowed. The builds succeeded regardless (lost accesses hit
freshly-healed page-cache pages whose content was garbage-by-contract anyway in these
instances), but this is guest-visible corruption waiting to happen.

**Fix (implemented as the follow-up to this spike)**: a translation fault on a guest-RAM PA
that is not in the released set must RETRY without advancing the PC (the racing heal has
already installed the mapping), with a per-PA cap so a genuine bookkeeping hole still dies
loudly instead of silently.

Dogfood exposure at time of writing: zero strays (its balloon sits in dead-band, no heal
traffic → no race window).

## What this run could NOT test

- **The dogfood `internal_compressed` ratchet** (15.3 G phantom on the field VM the same
  morning): the quiet host never engaged the compressor (system counter flat), so the
  strand-vs-double-charge discriminator got no local data. Owed: a pressured rerun (held
  host ballast during the mix) watching `ic_bal/cred/deb` vs system compressor growth.
- **Policy-driven oscillation**: no agent in the stock guest. For policy-in-the-loop local
  runs, install limina-agent into the image (or use an enhanced-tier clone).

Data: `ab-{a,b}-sampler.csv` (host ledger, 10 s), `ab-{a,b}-meminfo.csv` (guest, 5 s).
