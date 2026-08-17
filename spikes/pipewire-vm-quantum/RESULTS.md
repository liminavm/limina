<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Gustavo Noronha Silva
-->

# Guest audio is destroyed for small-buffer clients: PipeWire cannot tell it is in a VM

**CONFIRMED 2026-08-16, both tiers affected, fix not yet chosen.**

An audio client that asks for a small buffer drags the entire PipeWire graph — the ALSA
sink included — down to its quantum, and the virtio-snd sink then starves continuously.
Playback becomes unlistenable, not merely glitchy.

## The chain

1. PipeWire ships a guard for exactly this hazard in `/usr/share/pipewire/pipewire.conf`:

   ```
   context.properties.rules = [
       {   matches = [ { cpu.vm.name = !null } ]
           actions = { update-props = {
               # These overrides are only applied when running in a vm.
               default.clock.min-quantum = 1024
   ```

2. `cpu.vm.name` is set by spa's VM detection, which reads `/sys/class/dmi/id/sys_vendor`
   and `/sys/class/dmi/id/product_name` and matches a fixed vendor table. Verified against
   the shipped `libspa-support.so` (pipewire 1.6.8, F44): `QEMU`, `VMware`, `Bochs`,
   `Parallels`, `BHYVE`, `microsoft`, `oracle`.

3. Our guest presents `sys_vendor=Libkrun`, `product_name=libkrun Virtual Machine`,
   `bios_vendor=libkrun`. Nothing matches, so `cpu.vm.name` is **unset** — confirmed absent
   from the running core's properties, which carry only `cpu.max-align`.

4. The rule never fires. `clock.min-quantum` stays at the bare-metal default **32**.

5. A client requesting `256/44100` therefore pulls the graph to `QUANT=256`. The sink runs
   5.33 ms cycles against a virtio-snd device whose period is 480 frames (10 ms) and
   starves. Xruns climb continuously on both the client and the sink node; the device
   buffer drains to 7648 of 7680 frames.

`systemd-detect-virt` reports only `vm-other` for the same reason, so anything else keying
off VM identity (systemd, dracut, GNOME, mesa) is equally blind. **This is a systemic
identity gap, not an audio bug.**

## Reproducer

Enhanced-tier F44 guest, any host. `org.wesnoth.Wesnoth` from the `fedora` OCI remote
(the Flatpak, *not* the `wesnoth` RPM — see below), launched in the seated session:

```
flatpak install --user -y fedora org.wesnoth.Wesnoth
```

Listen to the title music: destroyed. Measure with `pw-top -b -n 25` — `QUANT=256` on both
the client and `alsa_output.platform-a016000.virtio_mmio.stereo-fallback`, `ERR` climbing on
both.

**The RPM `wesnoth` does not reproduce it**: it requests `1024/44100` and identifies as
`SDL Application`, so the graph stays at 1024. The Flatpak requests `256/44100`, identifies
as `wesnoth`, and reaches pipewire-pulse through `/run/flatpak/pulse/native`. Any
small-buffer client will do — the Flatpak is incidental, the 256-frame request is not.

## Proof

Lifting the floor at runtime, with the failing client still playing, restores the sink to
`QUANT=1024`, stops both xrun counters dead, and makes the music clean:

```
pw-metadata -n settings 0 clock.min-quantum 1024
```

## Workaround (per-guest, either tier)

```
mkdir -p ~/.config/pipewire/pipewire.conf.d
cat > ~/.config/pipewire/pipewire.conf.d/10-vm-quantum.conf <<'EOF'
context.properties = { default.clock.min-quantum = 1024 }
EOF
systemctl --user restart pipewire
```

## Ruled out

Each was matched to the failing guest and listened to; all clean. None is the cause.

- alsa-lib/alsa-ucm/alsa-utils `1.2.15.3` vs `1.2.16.1`, wireplumber `0.5.13` vs `0.5.14`.
- Device `api.alsa.period-size` 512 vs 480 — forced to 480, clean. The **graph quantum** is
  the variable, not the device period; a guest freely renegotiates between 480 and 512
  within one session, so a single reading of either proves nothing.
- vCPU count (6 vs 10) and RAM (8G vs 24G).
- Dynamic memory. `--memory 1024..24576 --reclaim moderate` with the balloon inflating
  0.75 -> 17.4 GiB produced 3 xruns at peak inflation and none after — a real mechanism at
  negligible magnitude, worth remembering but not this bug.
- Accumulated user state: 2021-era `~/.config/pulse` tdbs, `media-session.d` leftovers, a
  WirePlumber 0.4 `main.lua.d` config, and `default.configured.audio.sink` pointing at a
  device absent from the VM. Transplanted wholesale onto a clean guest: clean.

## Fix options (undecided)

1. **Present a recognized DMI identity** from libkrun's SMBIOS. The only option that fixes
   the **stock tier**, where the guest has no limina components to carry a drop-in — and the
   stock tier is precisely where destroyed game audio is least acceptable. Costs honesty in
   `sys_vendor`/`product_name`.
2. **Upstream `libkrun` into spa's table and `systemd-detect-virt`.** Correct, and it fixes
   the systemic gap rather than one symptom, but slow and does nothing for guests running
   today's software.
3. **Ship a pipewire drop-in in the enhanced tier.** Cheap and immediate, but enhanced-only,
   so it violates nothing while fixing nothing for a stock guest.

(1) and (2) are complementary and probably both wanted. (3) alone is not sufficient.
