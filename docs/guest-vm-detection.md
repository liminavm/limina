<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Gustavo Noronha Silva
-->

# Software that misbehaves because it does not know HVF, libkrun or Limina

Guest and host software routinely asks "am I in a VM, and which one?" and answers it by
matching a **hardcoded list of names**. Anything not on the list is treated as bare metal —
silently, with no warning and no fallback. limina is on nobody's list, so we inherit
bare-metal defaults everywhere this pattern appears.

This is the User-Agent problem: the check is a string lookup against a closed set, so the
failure mode for a newcomer is not "unknown, be conservative" but "definitely not a VM".

**This file is the running inventory.** Add an entry the moment a suspicion appears, even
unconfirmed — a wrong guess costs a line, a missed one costs a day. A later pass will patch
and report upstream in bulk.

## How the detection actually works

Two mechanisms matter, and the difference decides whether a fix is possible at all:

- **Name tables (fragile).** Read `/sys/class/dmi/id/{product_name,sys_vendor,board_vendor,
  bios_vendor}` and **prefix**-match against a fixed vendor list. `product_name` is read
  first. Because it is a prefix match, a compatibility token only works **first** in the
  string — `"Limina (like Parallels)"` matches nothing, `"KVM Virtual Machine (Limina)"`
  matches. Used by systemd (`dmi_vendor_table` in `src/basic/virt.c`) and PipeWire
  (`spa/plugins/support/cpu.c`, exposed as `cpu.vm.name`).
- **Generic signals (robust).** The SMBIOS BIOS Characteristics Extension Byte 2 bit 4 says
  "this is a virtual machine" without naming anyone. systemd honors it and reports
  `vm-other`; **spa does not**, which is the whole bug below.

We present, since libkrun `10fdf29`:

```
sys_vendor   = Limina
product_name = KVM Virtual Machine (Limina, libkrun)
bios_vendor  = libkrun
```

The leading `KVM` is deliberate and load-bearing: it is what every name table matches on.
`KVM` is accurate (libkrun is a KVM VMM on Linux; HVF is the same abstraction on macOS) and
is the only accurate option with **no guest-daemon baggage** — see the name-choice table
below. Do not reorder that string without re-reading this file.

### Choosing a name: what else keys on it

| Name | Consequence in a Fedora guest |
|---|---|
| `vmware` | Starts `vmtoolsd`, `vgauthd`, `run-vmblock\x2dfuse.mount`. open-vm-tools is **installed and enabled** on our F44 images, gated purely on `ConditionVirtualization=vmware`. **Avoid.** |
| `microsoft` | Starts `hypervkvpd`, `hypervvssd` the same way. **Avoid.** |
| `oracle`, `vmware` | WirePlumber has extra ALSA rules for these (`~^(vmware)\|(oracle)$`) — but they only match `alsa_*.pci.*` nodes, so they would not reach our virtio-mmio node anyway. |
| `kvm`, `qemu`, `parallels`, `bochs` | Activate nothing. `qemu-guest-agent` is **not** name-gated — it is udev-triggered on a `virtio-ports` device named `org.qemu.guest_agent.0`, which **we expose on every spawn** (`crates/limina-vmm/src/krun/console.rs`), so the stock agent runs and the host talks to it (`crates/limina/src/qga/`). |

Nothing in a Fedora guest keys on `parallels` to infer a macOS host. That signal does not
exist; do not choose a name hoping for it.

## Confirmed

### PipeWire — `default.clock.min-quantum` stays at the bare-metal 32

**Impact: severe. Both tiers. Fixed by the DMI change.**

PipeWire raises `default.clock.min-quantum` to 1024 only when `cpu.vm.name` is set. spa has
no generic fallback — only the name table — so before the fix the floor stayed at 32, any
client requesting a small buffer dragged the whole graph (**including the ALSA sink**) to its
quantum, and playback starved continuously. A Flatpak requesting 256 frames at 44.1 kHz made
audio unlistenable, with xruns climbing on both the client and the sink node.

Full diagnosis, reproducer and measurements: `spikes/pipewire-vm-quantum/RESULTS.md`.

**Still owed upstream:** teach spa to honor the SMBIOS VM bit, so every VMM off the list is
fixed rather than just us. Adding `libkrun` to spa's and systemd's tables is the lesser
version. Neither is filed yet.

### WirePlumber — VM ALSA defaults never reach our sound device

**Impact: unknown, unfixed. Not a name problem — a node-name problem.**

`/usr/share/wireplumber/wireplumber.conf.d/alsa-vm.conf` applies VM-appropriate
`api.alsa.period-size = 1024` and `headroom = 2048`, but only to nodes matching
`alsa_input.pci.*` / `alsa_output.pci.*`. Ours is
`alsa_output.platform-a016000.virtio_mmio.stereo-fallback`, so it matches under **no**
hypervisor name. The guest consequently negotiates its period without those guardrails and
observably drifts between 480 and 512 frames within a single session.

Fixing this needs an upstream rule covering virtio-mmio sound nodes, or a guest-side drop-in.
A drop-in only helps the enhanced tier.

## Suspected — not yet investigated

Nothing here is confirmed. Each is a place the same pattern is likely to bite.

- **systemd** — we now report `kvm` rather than `vm-other`, which changes which units run.
  Worth an audit of `ConditionVirtualization=` across the images to check nothing undesirable
  became active, and that nothing desirable became inactive.
- **dracut / initramfs generation** — host-only vs generic initramfs decisions consult virt
  detection; may affect module inclusion.
- **Mesa / drirc** — driver workaround databases key on device and platform identity.
- **GNOME / gnome-settings-daemon** — power, panel and animation defaults sometimes differ
  under virtualization.
- **Anything reading `sys_vendor` for branding** — we now say `Limina`, which is a *new*
  string to the world and may itself trip lists elsewhere.
- **Host side (macOS/HVF)** — the mirror image of this problem: tools that special-case
  hypervisors on the host, or that assume Hypervisor.framework behaves like KVM.

## Adding an entry

State the software, the impact, whether it is confirmed or suspected, the mechanism (name
table vs generic signal vs something else), and whether a fix exists and where. Link a spike
if one exists rather than restating its measurements.
