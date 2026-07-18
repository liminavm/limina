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

### ✅ FIX + VALIDATION — krun-efi patched, s2idle round-trip wakes (2026-07-18)
The blocker was purely the firmware. **Root cause: `ArmVirtPkg/Library/ArmVirtPL031FdtClientLib`'s
constructor explicitly sets the pl031 DT node `status="disabled"`** ("UEFI takes ownership of the RTC
hardware... disable it in the device tree to prevent the OS from attaching its device driver as
well"). **Fix (`scripts/build-krun-efi.sh` patch step 1c): flip that `"disabled"` → `"okay"`** so the
guest's `rtc-pl031` binds (UEFI keeps using the same PL031 for GetTime — concurrent reads don't
conflict). Rebuilt `KRUN_EFI.gop.fd`; re-probed a fresh clone:

- `rtc1 -> rtc-pl031 a002000.rtc; wakealarm: YES`; `/proc/device-tree/rtc@a002000/status` = `okay`;
  `a002000.rtc` is now an amba device; `/proc/interrupts` shows `rtc-pl031` on GIC SPI 33 (Edge).
- **`rtcwake -m freeze -s 10 -d rtc1` round-trip:**
  - **ENTRY works** — guest freezes, virtio drivers quiesce (`update virtio queue in invalid state
    0x8f`), NO S4-style PSCI/OSDLR crash.
  - **WAKE works** — the libkrun 0054 alarm arms with correct timing (`match=…273 now=…262 -> 11.1s`),
    fires on schedule 11 s later (`fire_alarm imsc=1 intc=true irq_line=Some(33)`), and the **vCPU
    resumes from s2idle**: proven by the guest immediately executing the `RTCIMSC=0` MMIO store (only a
    running vCPU can), i.e. its rtc IRQ handler ran.
  - ⚠️ **Full resume/thaw is incomplete** — post-wake the guest network never recovers (SSH resets),
    and no further guest activity is seen. This is the **virtio freeze/thaw hardening** the design
    already flags for M9.2 (spike #1's `invalid queue state 0x8f`), NOT a wake failure.

## Conclusion / where this leaves M9

**Spike F wakeup-half: ANSWERED — s2idle is reachable in libkrun and the PL031 alarm wakes it.** The
mechanism is now complete on the real (EFI) boot path: libkrun 0054 (RTC alarm) + the krun-efi patch
(PL031 enabled) → `rtcwake -m freeze` enters s2idle and the vCPU wakes on schedule.

**Next (M9.2-shaped):** make the resume *complete* — virtio freeze/thaw hardening so the guest (esp.
virtio-net) comes back cleanly after wake. Still open independently: our **16k enhanced kernel has no
PM configs** (`CONFIG_SUSPEND`), needed before the enhanced tier can s2idle; and spike F **part 2**
(does the virtio-gpu resubmit hook the sleep callbacks) needs the Dongwon-Kim series we don't carry.

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
