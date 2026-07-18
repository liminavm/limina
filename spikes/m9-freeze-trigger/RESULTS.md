# Spike F — is suspend-to-idle reachable + wakeable inside libkrun?

**Gate before M9.2** (see `docs/design/m9-freeze-trigger.md` §5). Two coupled unknowns:
(1) can a libkrun guest enter `freeze` (s2idle) and be woken, without tripping an HVF gap the way
S4 did; (2) does the virtio-gpu resubmit hook the sleep callbacks. This spike attacks (1) — the
wakeup-source half — because that is what blocks everything else.

Vehicle: `spikes/m9-freeze-trigger/f44-s2idle.raw` = CoW clone of
`Fedora-Workstation-44.accessible.raw` (stock kernel `6.19.10-300.fc44`, has `CONFIG_SUSPEND` +
`rtc-pl031`). Booted headless EFI + `--net`, probed read-only over SSH. Date: 2026-07-18.

## Findings

### ✅ s2idle IS available in the guest
- `/sys/power/state` → `freeze mem disk`; `/sys/power/mem_sleep` → `[s2idle]`. So the `freeze`
  state exists and `mem` maps to s2idle. (Entry/exit not yet exercised — blocked on a wakeup, below.)

### ✅ libkrun's PL031 had no alarm — FIXED (libkrun 0054, this session)
- Firecracker's PL031 stored the Match Register but never fired, and on macOS `register_mmio_rtc`
  dropped the interrupt eventfd + ignored the `IrqChip`. So an armed alarm never delivered → no
  `rtcwake` wakeup. Fixed: a timer thread + event-manager `Subscriber` that raises the SPI on match;
  `register_mmio_rtc` now wires intc + irq line + subscriber; macOS RTC FDT node is edge-triggered
  (the in-kernel GIC's `set_irq` only asserts a one-shot pulse). Unit-tested (`test_rtc_alarm_fires`),
  boot-neutral.

### ❌ …but the PL031 alarm is UNREACHABLE on the EFI boot path — the real blocker
On the EFI/GRUB boot path (how **all** real images boot, stock and enhanced):
- The guest's **rtc0 is `rtc-efi`** (EFI runtime-services RTC), which has **no `wakealarm`**:
  `rtcwake -m freeze -s 5` → `rtcwake: set rtc wake alarm failed: Invalid argument`, so it bails
  *before* freezing (no hang, but no suspend either).
- The **PL031 never binds**: its DT node `rtc@a002000` is present but **`status = "disabled"`**, so
  `of_platform` skips it (no amba device, no `rtc1`, no IRQ in `/proc/interrupts`). The UART
  (`a001000`) and GPIO (`a003000`) primecell siblings bind fine.
- The PL031 MMIO itself is healthy (read directly via `/dev/mem`: PID `31 10 14 00`, CID
  `0d f0 05 b1` = the `0xB105F00D` primecell signature, valid `RTCDR`). So the device is there and
  correct — it's just **disabled in the DT that reaches the OS**.
- libkrun's `create_rtc_node` emits **no** `status` property (→ "okay" on the direct-kernel `--kernel`
  path), so the `disabled` is applied on the **EFI path by the EDK2/krun-efi firmware** (it shadows
  the DT RTC with its own EFI runtime RTC, an ArmVirtPkg-style pattern). We own that firmware
  (`scripts/build-krun-efi.sh`).

## Conclusion / where this leaves M9

The wakeup-source half of spike F is **blocked on the EFI firmware disabling the PL031**, not on
libkrun's device layer (now fixed) or on s2idle being unavailable (it isn't). To get a working
`rtcwake`/s2idle wakeup on the real boot path we need **one** of:

1. **Patch krun-efi/EDK2 to leave the PL031 DT node enabled** (stop shadowing it with the EFI RTC),
   so `rtc-pl031` binds and its `wakealarm` rides the libkrun 0054 alarm. Smallest, mechanism-in-
   firmware, we own it. **← recommended next.**
2. Implement EFI `SetWakeupTime` in libkrun's EFI RTC runtime so `rtc-efi` gains a wakealarm. Larger.
3. Use a non-RTC PM wakeup source (a virtio IRQ marked wakeup) — sidesteps RTC but diverges from the
   standard `rtcwake` path the freeze bracket assumes.

Also still open (independent of the wakeup): our **16k enhanced kernel has no PM configs**
(`CONFIG_SUSPEND` etc.) — the enhanced tier needs them added before it can s2idle at all. And spike
F **part 2** (does the virtio-gpu resubmit hook the sleep callbacks) can't run until we carry the
Dongwon-Kim series.

## Repro
```
cp -c Fedora-Workstation-44.accessible.raw spikes/m9-freeze-trigger/f44-s2idle.raw
target/debug/limina --firmware target/krun-efi/KRUN_EFI.gop.fd \
    --disk spikes/m9-freeze-trigger/f44-s2idle.raw --cpus 2 --ram-mib 4096 --net
ssh -p <PORT> claude@127.0.0.1   # PORT from the "SSH forward ready" log line
  cat /sys/power/state /sys/power/mem_sleep
  cat /sys/class/rtc/rtc0/name              # rtc-efi (no wakealarm)
  cat /proc/device-tree/rtc@a002000/status  # disabled
  sudo rtcwake -m freeze -s 5 -v            # "set rtc wake alarm failed: Invalid argument"
```
