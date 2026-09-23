# Stock boots that never reach sshd: the guest i2c-virtio use-after-free

**Question.** In a suite run, some stock Fedora 44 boots never reached sshd. This spike asked
whether the snd worker thread caused that. It did not. The cause is a use-after-free in the stock
guest's `i2c-virtio` driver, and the virtio-i2c SBS battery device is what makes it reachable. The
host mitigations below lower how often it fires. Fixing it for good takes a guest driver fix.

## Vehicle

`boot-loop.sh <outdir> [iterations] [extra limina args]` boots `Fedora-Workstation-44.stock.test.raw`
(on an APFS clone) over and over, with `KRUN_EFI.gop.fd`, `--net`, 4 vCPUs, 4 GiB and the console
captured. A boot counts as stalled if sshd does not answer within 150 s. The loop then stops, leaves
that VM running and `sample`s its worker. `boot-19-console.log` and `boot-34-console.log` are the
consoles from the two stalls it caught.

## Findings

- **The stall is a guest kernel panic.** Both consoles end in `Oops ... Fatal exception in
  interrupt`, with the fault in `complete` called from `virtio_i2c_msg_done` → `vring_interrupt`.
  Each fired on CPU 0, over a task that is not the one doing the i2c transfer (`swapper/0`,
  `systemd-journal`). Both happened at the initrd switch-root, when systemd SIGKILLs what is left of
  the initrd's udev while that udev is reading the SBS battery's sysfs.
- **Mechanism.** The driver's wait for a transfer is interruptible (upstream `a663b3c47ab1`). When
  a signal interrupts it, the driver frees the requests while they are still on the virtqueue. The
  completion then arrives on whichever vCPU takes the interrupt, and its callback calls `complete()`
  on freed memory. Checked 2026-09-23 against the 6.19.10 driver: unchanged in mainline (7.3-rc4) and
  linux-next. Three fixes have been posted and none is merged: retain the transfer with a kref (v5),
  reset the virtqueue before freeing, and wait uninterruptibly (which brings back the hang that
  `a663b3c47ab1` fixed).
- **The battery device is required.** No stalls in 60 boots with no battery. With the battery
  attached there were 2 stalls in 53 boots. Before the snd worker thread there were 0 in 60, which
  is not significantly different from 2 in 53.
- **What sets the window.** The request stays in flight from the kick until the interrupt is
  handled. Before the mitigation that span covered waking the shared event loop after idle (about
  1 ms, from the wake-probe) plus the IOKit battery read. The IOKit read was the smaller part:
  51 µs at p50.

## Host mitigation

- **libkrun `VirtioDevice::notify_inline`.** virtio-i2c handles the QueueNotify write on the vCPU
  that made it, so the used ring is written and the interrupt raised before that write returns to
  the guest. The window shrinks to how long the interrupt takes to reach its vCPU. It is not closed:
  when the interrupt is routed to a different vCPU than the killed reader, that reader can still
  reach its wait first.
- **Cached battery snapshot** (`crates/limina-vmm/src/krun/battery.rs`). A thread re-reads IOKit
  every 2 s. The provider only copies the snapshot, so no IOKit call runs on a vCPU.

Measured 2026-09-23, with both mitigations: 0 stalls in 100 boots. At the earlier rate of 2 in 53,
a clean run of 100 would happen by chance about 2% of the time. The guest still reads the host's
battery correctly through the inline path (sysfs capacity and status match `pmset -g batt`).

The real fix is on the guest side: carry a patch in our kernel fork for the enhanced tier, and wait
for Fedora to ship a fixed kernel for the stock tier.
