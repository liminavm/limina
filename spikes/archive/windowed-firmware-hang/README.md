# Windowed GOP-firmware "BDS hang" — ROOT-CAUSED & FIXED AT SOURCE (2026-06-22)

`limina --window` with the **DEBUG GOP firmware** hangs on the TianoCore splash. **Root cause: a
firmware ASSERT, not a libkrun/virtio/timer/present bug.** EDK2's DxeCore hits a failed `ASSERT` and
`CpuDeadLoop()`s:

```
FATAL ERROR - RaiseTpl with OldTpl(0x10) > NewTpl(0x8)
ASSERT [DxeCore] .../MdeModulePkg/Core/Dxe/Event/Tpl.c(66): ((BOOLEAN)(0==1))
```

`CoreRaiseTpl(NewTpl)` requires `OldTpl <= NewTpl`; something raises to **TPL_CALLBACK (0x8)** while
already at **TPL_NOTIFY (0x10)**. Fatal only in DEBUG (`DEBUG_PROPERTY_ASSERT_DEADLOOP`); in RELEASE
`ASSERT` is a no-op — but `CoreRaiseTpl` then still sets `gEfiCurrentTpl = 0x8`, **silently lowering
the TPL** (state corruption masked, not fixed). So shipping RELEASE only *tolerated* the bug.

## The exact culprit (caller-print + addr2line, on the f44-edk2-build VM)
A one-line DEBUG print of `RETURN_ADDRESS(0)` at the failing `RaiseTpl` (see `pl011-capture-ring.patch`
to capture it without `--console` masking the race) gave:

```
LIMINA_RAISETPL_CALLER ra0=23FC8EB84 ra1=0 ra2=0
```

`0x23FC8EB84` lands in **VirtioSerialDxe.efi** (load base `0x23FC8C000`), offset `0x2B84`. `addr2line`
(slide = runtime_EP − elf_EP = the load base exactly):

```
VirtioSerialIoRead  →  OvmfPkg/VirtioSerialDxe/VirtioSerialPort.c:204
    204:  OldTpl = gBS->RaiseTPL (TPL_CALLBACK);   // disasm: mov x0,#0x8 ; blr x1 ; (ret=0x2B84)
```

**Causal chain:** VirtioSerial produces `SerialIo` → **TerminalDxe** wraps it as a console and creates
its serial-poll `TimerEvent` at **TPL_NOTIFY** (`MdeModulePkg/.../TerminalDxe/Terminal.c:381`) → when
that timer polls an *open* virtio-serial port, `VirtioSerialIoRead` (and its sibling `VirtioSerialIoWrite`,
line 161) run **at TPL_NOTIFY** and call `RaiseTPL(TPL_CALLBACK=8)` — raising to a *lower* TPL → ASSERT
→ `CpuDeadLoop`. (Generic to VirtioSerial+TerminalDxe; the windowed-GOP DEBUG firmware is just where it
became fatal-and-visible. `--console` masks it: per-byte PL011 write+flush slows the firmware past the
race window.)

## The fix (verified — both DEBUG & RELEASE now boot windowed to GNOME)
`VirtioSerialPort.c`: `RaiseTPL(TPL_CALLBACK)` → `RaiseTPL(TPL_NOTIFY)` at **both** SerialIo sites
(161 write, 204 read). TPL_NOTIFY is the correct level for shared-virtqueue access anyway (CALLBACK was
too low to even serialize against that NOTIFY poll timer), and `RaiseTPL(NOTIFY)` is legal whether
entered at CALLBACK or NOTIFY. Minimal + upstreamable. Lives in `scripts/build-krun-efi.sh` step **(1b)**.

Verified 2026-06-22 on stock F44 4 KiB, `boot-virgl-windowed.sh`:
- DEBUG-GOP + caller-print: **RED** → `LIMINA_RAISETPL_CALLER` + `CpuDeadLoop`, frozen splash (was 5/5 hang).
- DEBUG-GOP + the fix: **GREEN** → boots to GNOME, canary 0 asserts, serial reaches `fedora login:`.
- RELEASE-GOP + the fix (the shipped `KRUN_EFI.gop.fd`): **GREEN** → boots windowed to the GNOME desktop
  (IOSurface scanout pixel-verified).

## Files here
- `pl011-capture-ring.patch` — the in-memory PL011 capture ring for `third_party/libkrun` serial.rs
  (dumps `/tmp/pl011-ring.log` from a side thread; no per-byte syscall, so it does NOT alter timing the
  way `--console` does). Re-apply with `git apply` inside `third_party/libkrun` if this recurs.
- `caller-print-ring.log` — the captured boot serial showing the assert + `LIMINA_RAISETPL_CALLER`.
- `tianocore-splash-1280x800-hang.png` — `iosdump.swift` of the frozen splash (the DEBUG-no-fix RED).
- `worker-sample.txt` — `sample` of the hung `limina-vmm`: one vCPU ~99% in `hv_trap`. **NOT** a
  virtio used-ring busy-poll (the original mis-read); it is `CpuDeadLoop()`'s `for(;;) CpuPause()`.

## Falsified theories (kept as warnings)
1. virtio-gpu kick / used-ring busy-poll (the original README's diagnosis) — wrong; it was `CpuDeadLoop`.
2. HVF vtimer-mask staleness — wrong; pixel-verify showed the splash still froze, timer was healthy.
3. "the PlatformBm GOP→ConOut patch is the TPL violator" — wrong; the caller is stock VirtioSerialDxe,
   not our patch (though our GOP path is what makes the virtio-serial port open + the timer fire here).

See memory `limina-windowed-reboot-present-race`, `limina-krun-efi-build`.
