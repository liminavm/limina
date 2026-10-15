# HVF's per-vCPU pending interrupt lines on macOS 27

**Question:** does `hv_vcpu_get/set_pending_interrupt` still work on macOS 27? A dogfood guest
suspend failed there with `hv_vcpu_get_pending_interrupt(0) failed: 0xfae9400f` on every vCPU of
a VM without nested virtualization, which the snapshot code only expected with EL2.

**Vehicle:** `probe.c` — one VM, optionally the in-kernel GIC (libkrun's layout), one vCPU, then
get / set false / set true / get on both lines. `build.sh` builds, signs and runs both ways;
`build.sh build-only` for copying to another host.

## Measured 2026-10-15

| host | GIC | get | set false | set true |
|---|---|---|---|---|
| macOS 26.6.2, M1 Max | in-kernel | `HV_SUCCESS` | `HV_SUCCESS` | `HV_SUCCESS` (reads back 1) |
| macOS 26.6.2, M1 Max | none | `HV_SUCCESS` | `HV_SUCCESS` | `HV_SUCCESS` (reads back 1) |
| macOS 27.0.1, M4 Pro | in-kernel | `HV_UNSUPPORTED` | `HV_UNSUPPORTED` | `HV_UNSUPPORTED` |
| macOS 27.0.1, M4 Pro | none | `HV_SUCCESS` | `HV_SUCCESS` | `HV_SUCCESS` (reads back 1) |
| macOS 27.0.1, M1 Max | in-kernel | `HV_UNSUPPORTED` | `HV_UNSUPPORTED` | `HV_UNSUPPORTED` |
| macOS 27.0.1, M1 Max | none | `HV_SUCCESS` | `HV_SUCCESS` | `HV_SUCCESS` (reads back 1) |

Same on both lines (IRQ, FIQ), and the same on both chips: it is the OS, not the silicon. `spikes/m9-hvf-state-roundtrip` dies on 27.0.1 at the same call
(`roundtrip.c:447`) and is GREEN on 26.6.2. With its four pending-line calls made non-fatal it is
GREEN on 27.0.1 too, with the same rejections as on 26 (the EL2 reads, `ICC_RPR_EL1` on restore):
the pending lines are the only part of a vCPU + GIC snapshot that macOS 27 broke.

## Conclusion

On macOS 27 a VM with the in-kernel GIC has no host-driven IRQ/FIQ lines at all: reading and
writing them are both refused, whatever the vCPU's EL2 state. Interrupts reach such a vCPU
through the GIC, whose state a snapshot saves separately, and libkrun drives these lines only for
its userspace GIC. So a snapshot must treat a refused line as not pending, and a restore must not
write a line that is not pending — on 27 even writing `false` fails, which would fail every
restore.
