# vsock bulk-transfer profile

What a large transfer over libkrun's vsock costs, where the cost lands, and why host→guest is the
expensive direction.

## Vehicle

- `Fedora-Workstation-44.enhanced` clone at payload r27, 6 vCPUs, 8 GiB, EFI + venus, M1 Max host.
- Release `limina-vmm` at limina `21ff92f3` (libkrun `1315acdb`), booted with
  `LIMINA_VMM_BIN=target/release/limina-vmm` — a debug worker's unoptimised device code would
  dominate the profile.
- Path: guest `iperf3 -c 127.0.0.1:5201` → guest `socat TCP-LISTEN:5201 VSOCK-CONNECT:2:7000` →
  libkrun muxer → `LIMINA_VSOCK_BENCH=7000:/tmp/limina-vsockbench.sock` → host
  `socat UNIX-LISTEN TCP:127.0.0.1:5201` → host `iperf3 -s`.
- The bench socket lives in `/tmp` because macOS caps a UNIX socket path at 104 bytes; socat
  truncates a longer one silently and binds the wrong path.

`vsockbench.sh` runs one profiled 30 s transfer (worker `%cpu`, `ps -M`, guest `top`, a 10 s
`sample`); `irqbench.sh` counts the guest's vsock interrupts over a 10 s run each way.

## Measured 2026-09-13

| run | throughput | worker CPU | vsock interrupts | bytes / interrupt |
|---|---|---|---|---|
| idle | — | ~3% | — | — |
| guest→host, 1 stream | 4.6–4.8 Gbit/s | ~114% | 7.3k/s | 82 KB |
| guest→host, 4 streams | 4.65 Gbit/s | ~126% | — | — |
| host→guest, 1 stream | 3.1–3.3 Gbit/s | ~223% | 47k/s | 8.7 KB |
| host→guest, relay `-b 65536` | 3.8 Gbit/s | — | 50k/s | 9.4 KB |
| host loopback, no VM | 75 Gbit/s | — | — | — |

Idle, the vsock threads are blocked (`vsock muxer` in `kevent`, `vsock reaper` on its channel)
and no timesync thread exists, since limina turns the datagram off.

## Guest→host: one thread, one syscall per packet

About 85% of the main thread's samples (5,682 of 6,695) sit in
`Vsock::process_stream_tx` → `VsockMuxer::send_stream_pkt` → `UnixProxy::sendmsg` → `__sendto`:
one host-socket write per guest TX packet, all on the event-manager thread. Four streams move no
more than one, so that thread is the ceiling. The guest sends packets of up to 64 KiB
(`VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`), which is why this direction is the cheap one.

## Host→guest: an 8 KiB socket buffer, and every interrupt on vCPU 0

- **Each wakeup finds at most ~8 KiB to deliver.** macOS sizes a UNIX stream socket's buffers at
  `net.local.stream.sendspace` / `recvspace` = 8192. `UnixProxy::recv_pkt`
  (`third_party/libkrun/src/devices/src/virtio/vsock/unix.rs:249`) drains the socket until
  `EAGAIN` and signals the guest once per event, so bytes per interrupt tracks what the socket
  holds: 8.7–9.4 KB, 6.5× the interrupts of guest→host for less data. Raising the host relay's
  write chunk from socat's 8 KiB to 64 KiB (`-b 65536`) left it at 50k interrupts/s and
  9.4 KB each, so the writer is not the limit.
- **The guest posts 4 KiB RX buffers** — `VIRTIO_VSOCK_DEFAULT_RX_BUF_SIZE` is
  `SKB_WITH_OVERHEAD(1024 * 4)` in the pinned kernel's `include/linux/virtio_vsock.h:138`. Each
  descriptor gets its own `recv`, so this sets the syscall count, not the interrupt count.
- **All of them land on guest CPU 0.** `virtio10` (irq 24) had 2.7M interrupts on CPU0 and none
  elsewhere; `effective_affinity_list` is `0` although irqbalance is active. `fc_vcpu 0` is the
  hot worker thread (~74%): 3,724 ticks of guest execution (`hv_trap`), and its host-side waits
  are inside Hypervisor.framework's interrupt delivery — `Gic::compute_list_registers` /
  `apply_list_registers` from `sync_from/to_gic_state` (671 ticks) and `Gic::generate_sgis`
  (431). vCPUs 1 and 2 are mostly parked.
- **The muxer thread is ~45% busy** (2,910 of 6,630 samples outside its waits), most of it the
  per-descriptor `__recvfrom`.
- **The main thread re-arms epoll per packet**: `process_proxy_update` → `update_polling` →
  `Epoll::ctl` → `kevent` (110 ticks), a syscall on top of each data syscall.
- **In the guest**, two kworkers run at ~15% each and hard-IRQ time reaches ~10%.

## Harness artefact

The guest `socat` relay costs 35–40% of a guest core in either direction. A native `AF_VSOCK`
client would remove it; it is part of the bench, not of vsock.

## Candidate levers — none measured yet

- **A bigger proxy socket buffer.** `SO_RCVBUF` on the host-side fd in `UnixProxy` (a libkrun
  change) would let each wakeup carry more than 8 KiB, and so fewer interrupts per byte.
- **Coalesce the interrupt.** virtio-mmio gives the device one interrupt line, so it lives on one
  CPU at a time and cannot be spread; the lever is fewer, larger batches per signal.
- **Larger guest RX buffers.** The 4 KiB default is a guest-kernel constant, and we build the
  kernel; bigger buffers would cut the per-descriptor `recv` calls, not the interrupts.
- **Fewer syscalls per byte on the host.** `recvmsg` into several chained descriptors at once on
  RX; batch sends on TX; skip the `Epoll::ctl` re-arm when the interest set has not changed.
