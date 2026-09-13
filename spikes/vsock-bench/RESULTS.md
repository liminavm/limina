# vsock bulk-transfer profile

What a large transfer over libkrun's vsock costs, where the cost lands, and what moves it.

## Vehicle

- `Fedora-Workstation-44.enhanced` clone at payload r27, 6 vCPUs, 8 GiB, EFI + venus, M1 Max host.
- Release `limina-vmm`, booted with `LIMINA_VMM_BIN=target/release/limina-vmm` — a debug worker's
  unoptimised device code would dominate the profile. libkrun at `1315acdb` for the baseline,
  `e1c9a195` (EVENT_IDX) and `95625e59` (1 MiB proxy send buffer) for the fixes.
- Path: guest `iperf3 -c 127.0.0.1:5201` → guest `socat TCP-LISTEN:5201 VSOCK-CONNECT:2:7000` →
  libkrun muxer → `LIMINA_VSOCK_BENCH=7000:/tmp/limina-vsockbench.sock` → host
  `socat UNIX-LISTEN TCP:127.0.0.1:5201` → host `iperf3 -s`.
- The host relay runs at socat's defaults (8 KiB writes, 8 KiB send buffer) unless a row says
  otherwise; "big writer" is `socat -b 1048576 UNIX-LISTEN:…,fork,sndbuf=1048576`.
- The bench socket lives in `/tmp` because macOS caps a UNIX socket path at 104 bytes; socat
  truncates a longer one silently and binds the wrong path.

`vsockbench.sh` runs one profiled 30 s transfer (worker `%cpu`, `ps -M`, guest `top`, a 10 s
`sample`); `irqbench.sh` counts the guest's vsock interrupts over a 10 s run each way.

## Measured 2026-09-13

Guest → host:

| libkrun | host relay | throughput | worker CPU | interrupts/s |
|---|---|---|---|---|
| `1315acdb` | default | 4.4–4.8 Gbit/s | 119% | 7.6k |
| `e1c9a195` | default | 4.5 Gbit/s | 94% | 6.8k |
| `95625e59` | default | 11.2–11.3 Gbit/s | 190% | 20.7k |
| `95625e59` | big writer | 12.1 Gbit/s | — | 24.9k |

Host → guest:

| libkrun | host relay | throughput | worker CPU | interrupts/s | bytes/interrupt |
|---|---|---|---|---|---|
| `1315acdb` | default | 3.1–3.2 Gbit/s | 223% | 46k | 8.7 KB |
| `1315acdb` | `-b 65536` | 3.8 Gbit/s | — | 50k | 9.4 KB |
| `1315acdb` | 1 MiB `rcvbuf` + `sndbuf`, 8 KiB writes | 3.3 Gbit/s | — | 44k | 9.2 KB |
| `1315acdb` | big writer | 9.2 Gbit/s | — | 21.6k | 53 KB |
| `e1c9a195` | default | 3.0–3.2 Gbit/s | 234% | 27.5k | 14.7 KB |
| `95625e59` | default | 3.0–3.3 Gbit/s | 234% | 27.6k | 14.8 KB |
| `95625e59` | big writer | 9.3 Gbit/s | — | 18.9k | 61 KB |

Host loopback with no VM: 75 Gbit/s. Idle, the vsock threads are blocked (`vsock muxer` in
`kevent`, `vsock reaper` on its channel) and no timesync thread exists, since limina turns the
datagram off.

## On a macOS UNIX stream socket, size the sender's buffer

macOS gives a UNIX stream socket 8 KiB of send and receive buffer (`net.local.stream.sendspace` /
`recvspace`), and what the connection holds in flight is bounded by the *sender's* send buffer.
Raising the receiver's buffer, or the sender's write size alone, changes nothing; the sender's
`SO_SNDBUF` together with writes that fill it does. Every direction below is set by which side is
the sender.

## Guest → host: libkrun is the sender

- At `1315acdb`, 85% of the event-manager thread's samples (5,682 of 6,695) sit in
  `Vsock::process_stream_tx` → `VsockMuxer::send_stream_pkt` → `UnixProxy::sendmsg` →
  `__sendto`. A connected proxy's socket is blocking, and a guest packet carries up to 64 KiB
  (`VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`), so each packet waited on the host reader through an 8 KiB
  buffer. Four streams move no more than one.
- EVENT_IDX lets the guest skip the doorbell while the device is still draining TX: the same
  throughput on 94% of a core instead of 119%.
- A 1 MiB `SO_SNDBUF` on the proxy socket takes the stream to 11.3 Gbit/s, at 17% of a core per
  Gbit/s instead of 21%.

## Host → guest: the host peer is the sender

- `UnixProxy::recv_pkt` (`third_party/libkrun/src/devices/src/virtio/vsock/unix.rs`) drains the
  socket until `EAGAIN` and signals the guest once per wakeup, so the bytes per interrupt are what
  the socket holds when the muxer wakes. With the host relay at socat's defaults that is ~9 KB,
  whatever libkrun does; a big writer takes the same libkrun to 9.2 Gbit/s at 53 KB per
  interrupt.
- EVENT_IDX cuts the interrupts by 40% with the default writer, with throughput and worker CPU
  unchanged. The guest driver re-arms `used_event` at the end of each drain, so suppression only
  holds while it is mid-drain.
- **All interrupts land on guest CPU 0.** `virtio10` (irq 24) had 2.7M interrupts on CPU0 and none
  elsewhere. virtio-mmio gives a device one interrupt line, so it cannot be spread across CPUs.
  `fc_vcpu 0` is the hot worker thread (~74% at `1315acdb`), and its host-side waits are inside
  Hypervisor.framework's interrupt delivery — `Gic::compute_list_registers` /
  `apply_list_registers` from `sync_from/to_gic_state` (671 ticks) and `Gic::generate_sgis` (431).
- **The guest posts 4 KiB RX buffers** — `VIRTIO_VSOCK_DEFAULT_RX_BUF_SIZE` is
  `SKB_WITH_OVERHEAD(1024 * 4)` in the pinned kernel's `include/linux/virtio_vsock.h:138`. Each
  descriptor gets its own `recv`, so this sets the syscall count, not the interrupt count.
- The muxer thread is ~45% busy at `1315acdb` (2,910 of 6,630 samples outside its waits), most of
  it the per-descriptor `__recvfrom`.
- The event-manager thread re-arms the proxy's epoll registration on each credit update:
  `process_proxy_update` → `update_polling` → `Epoll::ctl` → `kevent` (110 ticks).
- In the guest, two kworkers run at ~15% each and hard-IRQ time reaches ~10%.

## Harness artefact

The guest `socat` relay costs 35–40% of a guest core in either direction. A native `AF_VSOCK`
client would remove it; it is part of the bench, not of vsock.

## Candidate levers — none measured yet

- **limina's own host-side writers.** Anything limina sends to the guest over a vsock port moves
  at the rate its own socket allows: a large `SO_SNDBUF` and large writes on the host end, per the
  rule above.
- **The muxer's level-triggered registrations.** While the guest's RX ring is empty, a readable
  proxy socket keeps waking the muxer thread until the guest refills.
- **The epoll re-arm per credit update**, when the interest set has not changed.
- **`recvmsg` into several chained descriptors** on RX, instead of one `recv` per 4 KiB buffer.
