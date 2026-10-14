# A restored L1 guest that goes idle and never wakes

`l1_restore_vtimer_offset` fails intermittently at "the restored guest never ran": the restored
worker logs `resumed from snapshot` for both vCPUs, and the guest then writes nothing to its
console, not one heartbeat, for the test's 20 s.

## Measured (dev Mac, macOS 26.6.2, M1 Max, 2026-10-14)

- Rate: 0/3 alone on a quiet host; with eight `yes` processes loading the CPUs, 3/6, 1/6, 1/1,
  1/7 and 1/4 at libkrun `9c3cb1e8`, and 1/6 at libkrun `bf5fb254` (limina `97af3e53`, the tree
  the test landed in). The stall predates the batch that first saw it.
- Both vCPUs resume at the same PC, the kernel's idle WFI, and stay inside `hv_vcpu_run` in
  HVF's own `VcpuStateManager::wait_for_interrupt` (`sample` of the stuck worker).
- At restore the timer state is unremarkable: `masked=false`, `CNTV_CTL` enabled (or vCPU 1
  stopped, `0x4`, in one run), and both deadlines already 0.4–2.9 s in the past, so the timers
  should fire as soon as the vCPUs run.
- Kicked out every 2 s while stuck, both vCPUs are still at the idle PC with HVF's offset equal to
  libkrun's record (20 s, the test's first pause). vCPU 0's deadline was ~1430 s ahead and
  falling; vCPU 1's moved forward a little faster than real time. The guest is idle with its
  heartbeat task never woken, not spinning and not stopped.

## Not the cause

- A restored vtimer mask: the failing runs restored `masked=false`.
- A stale offset record (`bf5fb254`'s bug): HVF's offset and the record agree throughout.
- The UART: Linux writes kernel console output by polling the PL011, without its interrupts.

## Open leads

- Where the heartbeat task's wake went: its 5 ms hrtimer was queued at the snapshot and overdue
  at the restore, so its expiry, or the IPI that should have followed it, was lost. The
  in-kernel GIC's restore (`hv_gic_set_state` + the per-vCPU ICC registers) and any SGI in
  flight at the snapshot are the first places to look.
- Why load matters: it widens the window between the snapshot's quiesce kick and each vCPU
  actually parking.

## Reproduce

Apply `libkrun-diag.patch` in `third_party/libkrun` and `test-diag.patch` at the limina root,
load the CPUs (`for c in 1 2 3 4 5 6 7 8; do gtimeout 1800 yes > /dev/null & done`), and repeat
`NEXTEST_SUCCESS_OUTPUT=immediate scripts/test-boot.sh debug --test l1_restore_vtimer_offset`
until one fails. A failure prints the console tail, the supervisor log with each vCPU's timer
state at restore and every 2 s after, and a `sample` of the worker (`DIAG_SAMPLE=<file>`).
