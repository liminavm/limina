# vz27-custom-virtio — can Virtualization.framework's macOS 27 custom devices carry limina?

The question: macOS 27 adds `VZCustomVirtioDevice` (our own virtio PCI device inside a VZ VM, with
virtio shared-memory regions and save/restore hooks). Does that remove the reasons limina runs its
own VMM on raw HVF (`docs/research/02-macos-hvf.md` §Option D)? Four things decide it, and this
spike measures them: notification latency, mapping GPU memory into the guest, reclaiming guest
memory through the framework's mapping, and save/restore of a custom device.

## Setup

- `host/vzprobe.m` — VZ host. Devices: Apple's virtio console #0 (log, `hvc0`), Apple's virtio
  console #1 behind a pipe that echoes XOR 0x20 ("apple"), a `VZCustomVirtioDevice` speaking
  virtio-console (ID 3, no MULTIPORT, identity echo, one 64 MiB shared-memory region, ID 1,
  `supportsSaveRestore`), Apple's vsock (5000 = command channel, 5001 = echo), Apple's
  traditional balloon. `vzprobe26` is the same source with a macOS 26 floor (no custom device).
- `guest/` — static-musl PID 1 in a throwaway initramfs (`mkinitramfs.py`), on the test kernels
  `target/test-guest/kernel/Image` (4 KiB) and `Image-16k` (6.12, virtio-pci built in).
- `run-libkrun-baseline.sh` — the same initramfs under `limina-vmm` (PL011 kernel console,
  libkrun virtio-console echoed through two FIFOs by `echo-helpers.py`, vsock 5001 to a UNIX
  socket echo). No command channel there, so only the echo tests run.
- Build: `./build.sh` (macOS 27 SDK). Run: `vzprobe <Image> out/initramfs.cpio [cmdline]`;
  `VZPROBE_RETURN_ON_PAUSE=1` returns held descriptors on pause.

Measured 2026-10-07 on the dogfood Mac (M4 Pro, macOS 27.0.1, from `/tmp`, 2 vCPU / 2 GiB guests)
unless marked dev Mac (M1 Max, macOS 26.6.2). The dogfood Mac was also running two other VMs
throughout, which is the likely source of the p99 noise. Latency = guest-measured 1-byte round
trip, n = 20000 after warm-up, raw tty.

## 1. Notification latency

| Path (M4 Pro) | p50 µs | p99 µs |
|---|---|---|
| VZ custom device (virtio-console, our delegate echoes), 4 runs, 4 KiB | 60.6 – 68.0 | 118 – 134 |
| VZ custom device, 16 KiB guest | 60.9 | 134 |
| VZ Apple console → pipe → our thread → pipe | 36.3 – 40.1 | 46 – 92 |
| VZ Apple vsock → our echo thread | 36.3 – 38.0 | 62 – 77 |
| libkrun virtio-console → FIFO → python echo → FIFO (Limina.app worker), 3 runs | 50.6 – 53.2 | 79 – 112 |
| libkrun vsock → UNIX socket → python echo, 3 runs | 37.0 – 38.5 | 45 – 91 |

Dev Mac (M1 Max, 26.6.2): VZ Apple console 59.3, VZ vsock 57.2, libkrun console 72.1.

- A custom-device round trip costs **~25 µs more than Apple's own console** on the same VM, though
  both cross into our process once each way. The extra is the custom-device machinery (delegate
  dispatch on its queue, per-element buffers, the interrupt back).
- Against libkrun's virtio-console it is **~12 – 15 µs slower at p50** (61 – 68 vs 51 – 53), and
  libkrun's figure carries a python echo the VZ figures do not, so the real gap is somewhat larger.
- Apple's built-in devices and libkrun's are comparable; the libkrun side carries the extra
  python hop, so no finer ranking follows from these numbers.
- Bulk through the custom console: 28 – 38 MiB/s (4 KiB guest), 52 MiB/s (16 KiB); the guest
  driver posts page-sized rx buffers, so this is a per-element cost, not a bandwidth ceiling.
- Every device / region call asserts it runs on the device queue (`dispatch_assert_queue` trap
  from `-[VZVirtioSharedMemoryRegion mapMemory:...]` called off-queue).

Reading: a transport that kicks per call pays ≥ 60 µs per synchronous round trip. A transport
whose rings live in the shared-memory region and are polled (venus's ring thread already works
this way) avoids notifications on the hot path; the kick cost then lands only on wake-ups.

The API's only guest→host signal is a virtqueue notification: there is no MMIO trap and no
doorbell-page hook. A userspace doorbell whose trap runs work on the vCPU thread (bare HVF:
~1 µs per trap, `spikes/doorbell-exit/` on the `metal-native-context` branch) has no equivalent
under VZ; the floor there is the ~30 µs one-way kick measured here.

## 2. Mapping host memory into the guest (shared-memory region)

`maximumAllowedSharedMemoryRegionCount` = 1 (per device). Each kind: 16 MiB mapped at offset 0 of
the 64 MiB region; host writes the first half, guest verifies and writes the second half, host
verifies with the CPU and with a GPU blit, then the host GPU fills the whole buffer and the guest
verifies.

| Host memory | map ms | unmap ms | guest sees host | host CPU sees guest | GPU reads guest | guest sees GPU fill |
|---|---|---|---|---|---|---|
| anonymous `mmap` | 0.10 | 0.14 | ✔ | ✔ | — | — |
| `MTLBuffer` shared | 0.06 | 0.34 | ✔ | ✔ | ✔ | ✔ |
| `MTLBuffer` bytesNoCopy (anon) | 0.06 | 0.21 | ✔ | ✔ | ✔ | ✔ |
| `MTLHeap` shared placement buffer | 0.06 | 0.22 | ✔ | ✔ | ✔ | ✔ |
| `IOSurface` base address | 0.06 | 0.23 | ✔ | ✔ | — | — |

Zero bad words / bytes in every cell, on both the 4 KiB and the 16 KiB guest. All pointers were
16 KiB aligned. The guest mapped the BAR through sysfs (`resourceN`, uncached): reads 6.4 –
7.9 GiB/s, writes 3.0 – 6.1 GiB/s.

Not covered: a **cached** guest mapping (what a real virtio-gpu host-visible blob uses) and its
coherence with GPU writes; regions larger than 64 MiB.

## 3. Reclaiming guest memory through the framework's mapping

Guest touches and mlocks 512 MiB, sends its PFN runs (16 KiB-aligned); host takes
`guestMemoryMappingAtPhysicalAddress:` for them and madvises.

- Apple's VM service process (`com.apple.Virtualization.VirtualMachine`) carries guest RAM in its
  footprint: 136 – 152 MiB at boot → 649 – 668 MiB after the guest touches 512 MiB. Our process
  stays at 18 – 160 MiB.
- `MADV_FREE_REUSABLE`, `MADV_DONTNEED`, `MADV_FREE` on our mapping: all return 0, **the VM
  service footprint does not move** (668 → 668), and the guest reads every page back intact.
  The mapping is a window, not a reclaim lever.
- Apple's own balloon, target 2 GiB → 1 GiB: the guest inflates (MemFree −1 GiB), **the VM
  service footprint does not move within 8 s** (668 → 668). Whether it is released later or
  only under host pressure is not measured.

## 4. Save / restore with a custom device

- A configuration with the custom device (`supportsSaveRestore = YES`) validates for
  save/restore. Save: 390 – 460 ms, 29 – 32 MB file (2 GiB guest, mostly untouched). Restore +
  resume into a fresh `VZVirtualMachine`: 290 – 400 ms. The device's save blob comes back
  verbatim in `customVirtioDeviceShouldRestore:saveState:`.
- Descriptors the device holds when the state is saved (the 256 posted rx buffers) are **never
  re-presented** after restore.
- **After restore the custom device's I/O does not reach the guest**, even with every held
  descriptor returned (empty) before the save: the restored device sees new rx buffers and tx
  notifications, writes the echo into an rx buffer and returns it, and the guest reads nothing
  (1 s timeout). The device's interrupt count in `/proc/interrupts` does go up when the buffer
  is returned, so the interrupt arrives and the data does not. The same happens when the device
  carried no traffic before the save (`vzprobe.noprobe`), so it is not specific to queues in use.
  Apple's console and vsock work after the same restore. Cause not isolated; a used-ring index
  that does not survive the restore fits the observations but is unproven.

## 5. Other observations

- The 16 KiB-page guest kernel boots and runs every test under VZ.
- VZ starts the VM in 73 – 117 ms.
- Not run: input. The API has no callback for driver writes to device config space (only the
  host-side `updateDeviceSpecificConfiguration:`), and virtio-input's probe is "driver writes
  select/subsel, device answers", so virtio-input cannot be built as a custom device. Feeding
  VZ's own USB keyboard/pointer without `VZVirtualMachineView` is untested.

## Verdict

The shared-memory region does what a venus/blob transport needs, with Metal and IOSurface memory,
GPU-coherent both ways. Notifications cost ~65 µs per round trip (~12 µs over libkrun's console,
~25 µs over Apple's own devices), which rules out kick-per-call designs but not polled rings in
shared memory. The framework's guest-memory mapping cannot reclaim memory, and Apple's balloon did
not visibly release any within 8 s. Save/restore of a custom device did not work after restore
on 27.0.1 (cause not isolated), and that is what the GPU-snapshot path would need. Input needs
Apple's view or its own devices. A ~1 µs vCPU-thread doorbell has no counterpart at all.
