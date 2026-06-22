# Windowed GOP-firmware "BDS hang" — ROOT-CAUSED & FIXED (2026-06-22)

`limina --window` with the **DEBUG GOP firmware** (the `cf147ed` windowed default) hangs on the TianoCore
splash. **Root cause: a firmware ASSERT, not a libkrun/virtio/timer bug.** EDK2's DxeCore hits a failed
`ASSERT` and `CpuDeadLoop()`s:

```
FATAL ERROR - RaiseTpl with OldTpl(0x10) > NewTpl(0x8)
ASSERT [DxeCore] /build/edk2/MdeModulePkg/Core/Dxe/Event/Tpl.c(66): ((BOOLEAN)(0==1))
```

`CoreRaiseTpl(NewTpl)` asserts `OldTpl <= NewTpl`; something raises to **TPL_CALLBACK (0x8)** while already
at **TPL_NOTIFY (0x10)** — a latent EDK2 TPL re-entrancy (timer/event interleaving) exposed by the windowed
present timing. It is **fatal only in DEBUG** (`DEBUG_PROPERTY_ASSERT_DEADLOOP`); RELEASE compiles `ASSERT`
to a no-op and tolerates it, like all production EDK2/QEMU firmware. (That is also why the silent firmware
"worked": it is RELEASE — not because "no GOP avoids early virtio-gpu".)

## The fix (verified 3/3 boots, was 0/5 hangs)
Ship a **RELEASE GOP firmware** — keeps the graphical boot console AND boots windowed-no-console to a
seated GNOME desktop:
```
TARGET=RELEASE GOP=1 scripts/build-krun-efi.sh        # -> target/krun-efi/KRUN_EFI.gop.fd (bootable)
spikes/virgl-zink-kk/firmware-hang-probe.sh 5 120     # RED on DEBUG (5/5 hang), GREEN on RELEASE (5/5 boot)
```
`scripts/build-krun-efi.sh` now defaults to RELEASE; `TARGET=DEBUG` builds a separate
`KRUN_EFI.gop.debug.fd` (still hangs windowed unless booted with `--console`, whose per-byte PL011 flush
slows the firmware past the race).

## Evidence in this dir — note the corrected interpretation
- `tianocore-splash-1280x800-hang.png` — `iosdump.swift` of the live scanout: the TianoCore logo frozen on
  black at 1280x800. Valid: the firmware never reaches the kernel mode-set because it dead-looped.
- `worker-sample.txt` — `sample` of `limina-vmm`: one vCPU pegged ~99% in `hv_trap`, other vCPUs parked,
  all host workers idle. **Originally mis-read as "the firmware busy-polls a virtio used-ring".** It is
  actually **`CpuDeadLoop()`** (an infinite `for(;;) CpuPause()` after the failed ASSERT) — confirmed by
  `addr2line` on `DxeCore.dll` (gdb base 0x47B0A000 from the serial `add-symbol-file` line): the stuck PC
  (0x47b14d94 / 0x47b26778) → `CpuDeadLoop` / `CpuPause`.

## How it was found (oracles that worked)
- **In-memory PL011 capture ring** in serial.rs: records every UARTDR byte regardless of `out` and dumps
  to `/tmp/pl011-ring.log` from a bg thread — no per-byte syscall, so it does NOT alter timing the way
  `--console`'s write+flush does (`--console` masks the race by slowing the firmware). This caught the
  ASSERT text. Re-add it if this recurs.
- **Forced-exit watchdog** let `run()` read the stuck guest PC; `addr2line` mapped it to `CpuDeadLoop`.
- **`hv_gic_set_spi` log**: the macos IrqChip is the in-kernel **HvfGicV3**, not software `GicV3`.

Two earlier theories — a virtio kick race (0006 territory; the *previous* content of this README) and an
HVF vtimer-mask bug — were both **falsified**. The PlatformBm GOP→ConOut patch our GOP firmware carries is
a prime suspect for the exact TPL violation (open follow-up: identify the `CoreRaiseTpl` caller in a Linux
build VM). See memory `limina-windowed-reboot-present-race`, `limina-krun-efi-build` (0006/0007).
