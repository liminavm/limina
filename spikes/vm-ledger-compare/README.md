# vm-ledger-compare — do QEMU-on-HVF and Virtualization.framework double-bill too?

Control experiment for the per-pmap 2× finding (`spikes/hv-ledger-marker`, `double` mode):
on macOS, a guest page touched by both the VMM process (task pmap) and the guest (HV
stage-2 pmap) bills phys_footprint AND resident_size twice. Every disk-fed guest page is
both-touched by construction, so Activity Monitor shows ~2× the guest page cache for a
limina VM.

Question here: is that xnu accounting applied to *any* HVF consumer, and does Apple's own
Virtualization.framework get an exemption (e.g. via the entitlement-gated ledger-tag
machinery — `mach_memory_entry_ownership(..., NO_FOOTPRINT)` returns KERN_NO_ACCESS for
us)?

- **QEMU** (`qemu-system-aarch64 -accel hvf`, Homebrew): same API shape as libkrun —
  guest RAM is a task mmap fed to `hv_vm_map`, virtio-blk preads land disk data through
  the task mapping. Expected: same 2×. Confirms "xnu policy, not a limina bug".
- **Virtualization.framework** (`vz/probe.swift`, minimal headless Vz VM): if it does
  NOT show 2×, Apple exempts its own stack — prime Radar ammunition.

## Method

Same guest for every VMM: a CoW clone of `Fedora-Workstation-44.accessible.raw` (stock
F44, ssh creds, no limina components), 8 GiB RAM, 4 vCPUs, headless.

Workload: ssh in, `sync; echo 3 > drop_caches`, then `dd if=/dev/vda of=/dev/null bs=1M
count=6000` — builds ~6 GiB of guest page cache purely from virtio-blk reads (the
both-touch shape). Sample the VMM process before/after:

- `ps -o rss=` (resident_size — the probe showed it double-bills identically), and
  `sudo footprint` when available (phys_footprint, Activity Monitor's number).
- guest `/proc/meminfo` Cached at the same instants.

Verdict arithmetic: VMM growth ≈ 2× guest-cache growth ⇒ double-billing; ≈ 1× ⇒ exempt.

## Scripts

- `run-qemu.sh <clone.raw>` — boot the clone under qemu-hvf (ssh on 127.0.0.1:2299).
- `vz/build.sh` — build + ad-hoc-sign the Vz probe (`com.apple.security.virtualization`).
- `vz/vz-probe <clone.raw>` — boot under Vz NAT (find the guest IP in
  `/var/db/dhcpd_leases`, ssh directly).
- `workload.sh <ssh-target> <vmm-pid> <label>` — drop caches, dd, sample, append to
  `RESULTS.md`-ready CSV lines.

Run limina itself over the same clone + workload for the in-family baseline if a fresh
number is wanted (the dogfood field data already establishes 2×).
