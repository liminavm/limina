# hv_vm_unmap / hv_vm_map on ranges in mixed states

Measured 2026-09-25, macOS 26.6.2 on an M4 Pro (16 KiB host pages), `./run.sh`. The question came
from modelling `ReleasedRam` (libkrun `src/hvf/src/released_ram.rs`) for the phase-3 enumeration
in `docs/design/in-crate-checkers.md`: a page can be released twice (reported free, never touched,
reported again or ballooned), so its second `release` unmaps a range that is already unmapped in
whole or in part. If HVF refused that, `release`'s rollback would drop the earlier release from
the set and leave a stage-2 hole with no bookkeeping.

| Case | Calls | Result |
|---|---|---|
| A | unmap one page twice | both `HV_SUCCESS`; the page is unmapped |
| B | unmap four pages whose second is already unmapped | `HV_SUCCESS`; all four unmapped |
| C | unmap four pages, the fourth never mapped | `HV_SUCCESS`; all four unmapped |
| D | one unmap across four separate one-page maps | `HV_SUCCESS`; all four unmapped |
| E | unmap the middle two of a four-page map, then its head | both `HV_SUCCESS`; exactly those pages unmapped |
| F | map over a live mapping, whole and partial | `HV_ERROR` (`0xfae94001`) both times; nothing changes |

**`hv_vm_unmap` is page-wise and idempotent**: it succeeds on any mix of mapped and unmapped
pages and ignores how the range was mapped. **`hv_vm_map` refuses any overlap** with a live
mapping, as `released_ram.rs` says. So a repeated release is harmless on HVF, and the released set
must still be exact, because the heal can only map back what is unmapped.

The model in `released_ram.rs`'s `every_sequence` tests follows this table: unmap succeeds on
anything (unless a failure is injected), map fails on any overlap.
