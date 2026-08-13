# vm-ledger-compare RESULTS — the per-pmap 2× is xnu policy, and Apple doesn't exempt Vz

**Date**: 2026-08-12 late evening. Host: macOS 26.5, M1 Max 32 GB, 16 KiB pages.
Guest for all legs: CoW clone of `Fedora-Workstation-44.accessible.raw` (stock F44,
kernel 6.19.10-300.fc44), 8 GiB RAM, 4 vCPUs, headless. Workload: drop caches, then
`dd if=/dev/vda of=/dev/null bs=1M count=6000` (6 GiB of virtio-blk reads — the
both-touch shape: VMM writes each buffer through its task mapping, guest reads it
through stage-2). Metric: VMM process `rss` (resident_size double-bills identically to
phys_footprint — proven in ../hv-ledger-marker `double` mode).

| leg | pre-drop | after 6 G dd | growth | notes |
|---|---|---|---|---|
| **QEMU 11.0 -accel hvf** | 1 801 M | 9 857 M | **+8.0 G** | healthy guest throughout |
| QEMU, re-read same 6 G | 9 829 M | 10 991 M | +1.1 G | mostly-billed already; dd anon churn |
| QEMU, fresh 3 G range | 10 991 M | 11 621 M | +0.6 G | guest recycled buffers — see below |
| **Virtualization.framework** | 1 609 M | 11 574 M | **+10.0 G** | helper = the XPC service process, NOT the probe (probe rss: 9 M); guest wedged after dd (see below) |

(For the limina leg the dogfood field data already establishes the same shape: ledger
34.2 G vs ~17 G real on a 24 G VM, and a 46.7 G peak.)

## Findings

1. **QEMU-on-HVF shows the same class of inflation** — 11.6 G resident for an 8 GiB
   guest holding ~7 G. Same API shape as libkrun (guest RAM = task mmap fed to
   `hv_vm_map`), same billing. The 2× is xnu accounting policy for any HVF consumer,
   not a limina bug.
2. **The billing saturates at ~2× of guest RAM, not 2× of IO volume.** The fresh-3 G
   leg grew rss by only 0.6 G: the guest recycled already-both-touched buffer-cache
   pages, and refilling an already-billed page adds nothing. PTEs are per guest-physical
   page, not per content. This is exactly dogfood's 46.7 G peak on a 24 G VM — full
   saturation (≈ 2×24 G minus never-both-touched pages) while the balloon was pinned
   at zero.
3. **Virtualization.framework does NOT get an exemption**: its per-VM helper process
   (`com.apple.Virtualization.VirtualMachine` XPC service) went 1.6 → 11.6 G on the same
   6 G read. Combined with `mach_memory_entry_ownership(..., NO_FOOTPRINT)` returning
   KERN_NO_ACCESS for third parties (../hv-ledger-marker `notag`), the picture is: Apple
   ships the same double-billing in its own stack rather than using the private ledger
   opt-out. A Radar is warranted on behalf of every VMM on the platform; expecting a
   macOS-side fix soon is not a plan. The mprotect settle sweep remains ours to build.
   - Caveat: the exact Vz slope is blurred by the wedge below (a spinning guest may have
     touched extra pages), but the class verdict (>1.4× of guest RAM for a 6 G read)
     doesn't depend on it.
4. **Vz side-observation**: the stock F44 guest wedged reproducibly (2/2 runs) right
   after the 6 G dd — all 4 vCPUs spinning (~400 % CPU), virtio-net dead, serial silent,
   no panic output. Not investigated further (not our stack); noted because the same
   image + workload is rock-solid under both qemu-hvf and limina.

## Implication for the fix

Nothing here changes the plan (mechanism: chunked mprotect(NONE→RW) settle sweep in
libkrun; policy in limina) — it just establishes that limina would be the FIRST VMM on
macOS whose Activity Monitor number is honest, and gives the Radar its evidence:
same-task double-billing applies to Apple's own Vz helper, and the opt-out entitlement
is private.
