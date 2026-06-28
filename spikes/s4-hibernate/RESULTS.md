# Spike: M9.0 #1 — does stock arm64 Fedora S4-hibernate inside libkrun?

**Date:** 2026-06-28. **Question (M9.0 spike #1, `docs/design/m9-suspend-resume.md` §8):** can a stock
arm64 Fedora guest enter Linux S4 (suspend-to-disk / swsusp) inside a libkrun/HVF VM, and can a fresh
worker cold-boot and *resume* the image? **Verdict: the guest-side mechanism is correctly wired and the
guest reaches the actual memory-snapshot step — but hibernation is currently BLOCKED by two concrete
libkrun HVF-backend gaps (which we own and can patch). Resume itself is therefore not yet provable.**

## Vehicle

- Disk: a CoW clone of the **stock** `Fedora-Workstation-43.accessible.raw` (Fedora kernel
  `6.17.1-300.fc43.aarch64`, **4 KiB pages**, stock mesa). Chosen deliberately over `enhanced.raw`:
  Fedora's own kernel has `CONFIG_HIBERNATION=y`, so this isolates "does the libkrun *environment*
  support S4" from "does our 16k kernel have the PM configs" (it doesn't — `build-kernel-rpm.sh` /
  `build-test-kernel.sh` carry zero PM configs; that's an M9.1 task, not what this spike tests).
- Boot: headless `limina --net --ssh-port 2222 --ram-mib 2048 --cpus N`, silent serial firmware
  (`target/krun-efi/KRUN_EFI.silent-rebuilt.fd`), SSH control via `gssh.sh`.

## What WORKED (the guest-side setup is sound)

1. **Kernel supports S4.** `/sys/power/state` = `freeze mem disk`; `/sys/power/disk` =
   `[shutdown] reboot suspend test_resume` (shutdown mode — no firmware/ACPI S4 needed, matches the
   swsusp research); `CONFIG_HIBERNATION=y`, `CONFIG_PM_SLEEP=y`, the `95resume` dracut module present;
   **lockdown `[none]`** (no Secure-Boot block).
2. **Disk-backed swap.** Stock Fedora ships **zram-only** swap (can't hold a hibernation image — the #1
   thing it lacks). Added a 3 GiB btrfs swapfile: `btrfs subvolume create /swap` +
   `btrfs filesystem mkswapfile --size 3G /swap/swapfile` + `swapon`.
3. **resume= wiring.** `resume_offset` via `btrfs inspect-internal map-swapfile -r` = **1713408**;
   `grubby --update-kernel=ALL --args="resume=/dev/vda3 resume_offset=1713408"` (GRUB owns the cmdline
   on the EFI path — FDT bootargs are ignored, per `limina-guest-console`); `dracut -f` baked the
   resume module + `/sys/power/resume_offset` = 1713408 confirmed on the next boot.
4. **Image discovery runs.** `systemd-hibernate-resume.service` fires on every boot and checks the
   resume device for a swsusp signature (clean either/or: signature ⟹ resume, absent ⟹ normal boot).

## What BLOCKED it — two libkrun HVF gaps (we own these)

The guest got all the way into `hibernation_snapshot` (the real snapshot step) before hitting the wall,
so this is **not** a guest-config problem — it's the VMM backend.

### Blocker 1 — PSCI CPU hotplug-offline unimplemented (blocks multi-vCPU hibernate)

With `--cpus 4`, hibernation aborted at `disable_nonboot_cpus()`. Supervisor log:
```
krun_hvf: unhandled PSCI/SMC function 0x84000002; returning NOT_SUPPORTED   # PSCI CPU_OFF
krun_hvf: unhandled PSCI/SMC function 0xc4000004; returning NOT_SUPPORTED   # PSCI AFFINITY_INFO (64)
```
Hibernation offlines every secondary vCPU before the atomic snapshot — each calls PSCI `CPU_OFF`
(`0x84000002`) on itself and the boot CPU polls `AFFINITY_INFO` (`0xc4000004`). libkrun's PSCI only
implements a minimal set (VERSION / CPU_ON / SYSTEM_OFF / SYSTEM_RESET; cf. `limina-hvf-graceful`) and
returns `NOT_SUPPORTED` for these → the offline fails → hibernation aborts and thaws (the worker stays
up; the `update virtio queue in invalid state 0x8f` warnings are the device thaw).

### Blocker 2 — unhandled EL1 debug sysreg on the CPU-suspend path (blocks even single-vCPU)

With `--cpus 1` (no secondary CPUs to offline → Blocker 1 sidestepped), hibernation got further into the
snapshot, then libkrun halted the VM:
```
krun_hvf: unhandled system-register read: rt=5 reg=2622470 name=unknown sysreg; stopping the VM
krun_vmm: vCPU 0: unrecoverable run error: Unhandled guest vCPU state; stopping the VM   # worker exit 1
```
`reg=2622470` = `0x280406`. libkrun's `reg = syndrome & SYSREG_MASK` (Rt masked out). Decoded as the
ESR-ISS sysreg encoding → **`OSDLR_EL1`** (OS Double-Lock Register, ARM-ARM `op0=2 op1=0 CRn=1 CRm=3
op2=4` = `S2_0_C1_C3_4`), a self-hosted-debug register saved/restored on the arm64 CPU-suspend path
(`cpu_do_suspend`/`cpu_do_resume`). libkrun's HVF sysreg table doesn't model it (returns
`None`/"unknown sysreg") → it stops the VM. The full debug-suspend set (OSDLR_EL1, OSLAR_EL1,
MDSCR_EL1, DBGB*/DBGW*…) likely needs handling, not just this one read.

## Productization gotcha found (not a blocker): SELinux swap labeling

First hibernate attempt failed with `Call to Hibernate failed: Access denied`. Cause: the freshly
created `/swap` btrfs subvolume is `unlabeled_t`, so `systemd-logind` is SELinux-denied `search` on it:
```
audit: avc denied { search } ... comm="systemd-logind" name="swap" tcontext=...:unlabeled_t tclass=dir
```
Worked around for the spike with `setenforce 0`. **Enhanced-image productization must label the swap**
(`semanage fcontext -a -t swapfile_t '/swap(/.*)?' && restorecon -RvF /swap`, or place the swapfile at a
policy-known path), the same way `prepare-efi-image.sh` handles relabeling.

## Side-finding: virtio freeze/restore is rough in-place (S3-shaped path)

The `pm_test=devices` smoke test and every failed-hibernate **thaw** threw three identical kernel WARNs
at `include/linux/virtio_config.h:276` during `virtio_device_restore` →
`virtballoon_restore` / `virtio_vsock_vqs_init` / `virtnet_restore` (all via `virtio_mmio_restore`), and
afterwards **virtio-net did not recover** (`no route to host`; SSH wedged until a reboot). Host side
logged `update virtio queue in invalid state 0x8f`. NB this is the **in-place freeze→thaw** path against
the *same* worker (S3-shaped), **not** the M9 resume path (a *fresh* worker rebuilding virtio devices
from scratch, which `virtio_device_restore` re-negotiates against) — that path is untested here. Still,
it flags that libkrun's virtio-mmio device model needs hardening for the guest's PM status transitions.

## Not proven

The **resume handshake** (fresh worker cold-boots, finds the swsusp signature, atomically restores) —
the worker died on Blocker 2 before writing the image, so we never reached the spawn-fresh-worker step.
Gated behind fixing Blockers 1 & 2. Markers were staged to verify resume vs cold-boot (a tmpfs
`/run/pre-hib-marker`, a live `sleep` PID, the `boot_id`) — reusable once hibernate completes.

## Verdict & impact on the M9 plan

The spike **de-risks the milestone by converting "does S4 work in libkrun?" into a bounded libkrun work
list.** The guest-side path (swap, `resume=`, image discovery) is correctly wired and the guest reaches
the snapshot step; what blocks it is squarely in the dependency we own:

1. **libkrun: implement PSCI CPU hotplug-offline** — `CPU_OFF` (0x84000002) + `AFFINITY_INFO`
   (0x84000004/0xc4000004) so `disable_nonboot_cpus()` works. (Also generally useful for vCPU hotplug.)
2. **libkrun: handle the EL1 debug/suspend sysreg set** on the CPU-suspend path — OSDLR_EL1 (the one
   that crashed), and almost certainly OSLAR_EL1 / MDSCR_EL1 / the DBGB*/DBGW* breakpoint regs —
   read+write, modeled or safely stubbed, instead of "stop the VM."
3. **Image-build (enhanced tier):** label swap for SELinux; add PM configs to the 16k kernel; provision
   swap ≥ RAM + `resume=`/`resume_offset=` + dracut resume.
4. **Harden virtio-mmio freeze/restore** in libkrun (the `0x8f` invalid-state warnings + the
   virtio_config.h:276 guest WARNs), at least for the real resume path.

**This contradicts the design doc's M9.1 "libkrun patches: none required for the core" — guest-side S4
needs libkrun HVF work FIRST.** The doc + roadmap are updated accordingly.

## Reproduce

```bash
cp -c Fedora-Workstation-43.accessible.raw s4-spike.raw
target/debug/limina --firmware target/krun-efi/KRUN_EFI.silent-rebuilt.fd \
  --disk s4-spike.raw --net --ssh-port 2222 --ram-mib 2048 --cpus 1 \
  --console spikes/s4-hibernate/boot-console.log    # backgrounded; needs HVF + codesigned worker
# then over SSH (spikes/s4-hibernate/gssh.sh):
#   btrfs subvolume create /swap; btrfs filesystem mkswapfile --size 3G /swap/swapfile; swapon …
#   grubby --update-kernel=ALL --args="resume=/dev/vda3 resume_offset=$(btrfs inspect-internal map-swapfile -r /swap/swapfile)"
#   echo 'add_dracutmodules+=" resume "' >/etc/dracut.conf.d/resume.conf; dracut -f
#   setenforce 0                       # until swap is SELinux-labeled
#   systemd-run --no-block --collect systemctl hibernate
# observe: with --cpus>1 → PSCI CPU_OFF NOT_SUPPORTED; with --cpus 1 → unhandled sysreg OSDLR_EL1.
```
Evidence kept alongside: `preconditions.txt`, `swap-setup.txt`, `pm_test-smoke.txt`,
`hibernate-why-failed.txt` (the SELinux AVC), `supervisor.log` / `boot-console.log` (the two blockers).
swsusp mechanics reference: `swsusp-notes.md`.
