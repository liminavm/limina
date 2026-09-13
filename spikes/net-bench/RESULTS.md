# gvproxy NAT bulk-transfer profile

What a large transfer over the `--net` path costs, and where the cost lands. SSH into a guest
rides this path (gvproxy's `127.0.0.1:<port> → 192.168.127.2:22` forward), not vsock.

## Vehicle

- `Fedora-Workstation-44.enhanced` clone at payload r27 (kernel `7.1.8-limina16k.4`), 6 vCPUs,
  8 GiB, EFI + venus, headless (`LIMINA_DISPLAY_CAPTURE`), M1 Max host, gvproxy v0.8.8.
- Release `limina` + `limina-vmm` (`LIMINA_BIN` / `LIMINA_VMM_BIN`), libkrun `af648dfe`.
- iperf: guest `iperf3 -c 192.168.127.254` → virtio-net → libkrun unixgram backend → gvproxy
  netstack → host `iperf3 -s -B 127.0.0.1`. `-R` reverses it.
- ssh: host `ssh -p <fwd>` with `head -c <n> /dev/zero` on one end and `/dev/null` on the other,
  default cipher.

`netbench.sh` runs one profiled transfer: worker and gvproxy `%cpu`, `ps -M`, guest `top`, an 8 s
`sample` of both processes, and the guest's net interrupt count.

## Measured 2026-09-13

| path | direction | throughput | worker CPU | gvproxy CPU | net interrupts/s |
|---|---|---|---|---|---|
| iperf | guest → host | 16.5 Gbit/s | 150–220% | 220% | — |
| iperf | host → guest | 1.1 Gbit/s | 170% | 155% | 42k |
| ssh | guest → host | 7.4 Gbit/s | 270% | 180% | 25k |
| ssh | host → guest | 1.0 Gbit/s | 188% | 155% | 39k |

## Host → guest arrives in MTU-sized frames

- The iperf host → guest run moved 4.19 GB on 1.25M net interrupts: 3.3 KB per interrupt, about
  two 1500-byte frames per wakeup. The guest segments with TSO4 on the way out; gvproxy sends at
  its MTU (1500, the default) on the way in, so every 1.5 KB costs a `recv` in the worker, a
  descriptor, and close to one interrupt.
- ssh host → guest runs at the iperf rate, so the path is the limit in that direction, not the
  cipher.
- The guest accepts up to `maxmtu 65535` on the NIC (GUEST_TSO4 is negotiated, so the driver posts
  big buffers).

## gvproxy `-mtu` lifts host → guest

gvproxy hands its MTU to the guest in the DHCP lease, and a stock NetworkManager applies it, so
this needs nothing from the guest. Run through `LIMINA_GVPROXY_BIN` pointing at a wrapper that
adds `-mtu`; the guest's `eth0` came up at the advertised MTU after a fresh boot.

| MTU | path | direction | throughput | worker CPU | gvproxy CPU | rx B/pkt |
|---|---|---|---|---|---|---|
| 1500 | iperf | host → guest | 1.1 Gbit/s | 170% | 155% | — |
| 9000 | iperf | host → guest | 4.0 Gbit/s | 172% | 189% | 8,257 |
| 65520 | iperf | host → guest | 8.1 Gbit/s | 165% | 230% | 32,828 |
| 1500 | iperf | guest → host | 16.5 Gbit/s | 150–220% | 220% | — |
| 65520 | iperf | guest → host | 17.9 Gbit/s | 127% | 217% | — |
| 1500 | ssh | host → guest | 1.0 Gbit/s | 188% | 155% | — |
| 65520 | ssh | host → guest | 6.5 Gbit/s | 215% | 250% | 28,760 |
| 1500 | ssh | guest → host | 7.4 Gbit/s | 270% | 180% | — |
| 65520 | ssh | guest → host | 7.7 Gbit/s | 280% | 190% | — |

At 65520: no errors or drops on the guest NIC, 3 TCP retransmits across all runs, and no
`ENOBUFS` or backend error in the worker log. Throughput climbs with the MTU all the way up, so
limina passes `-mtu 65520` by default (`GVPROXY_MTU` in `crates/limina/src/gateway.rs`); on that
build, without the wrapper, host → guest iperf measured 8.09 Gbit/s.

## Where the CPU goes at 8 Gbit/s host → guest (MTU 65520)

`limina-vmm`'s ~165% is mostly the guest itself: `busyleaves.py` over the worker sample puts
vCPU0 at 63% (51% of it `hv_trap`, i.e. running the guest; every net interrupt lands on CPU0),
the other vCPUs at ~35% together, and the `virtio-net worker` thread at ~36% (20% of that in
`recvfrom`, which on a non-blocking socket is the copy, not a wait). Inside the guest, `mpstat`
taken in the same run has CPU0 at 21.6% hard IRQ + 46.1% softirq, on 28k net interrupts/s for
~30k frames/s.

gvproxy's ~235% is spread over ~12 Go threads. With a symbolized build of v0.8.8 (the Homebrew
binary is stripped), the busy leaves are the Go runtime (~0.9 cores, mostly the garbage
collector: `scanblock`, `findObject`, `tryDeferToSpanScan`) and `sendto` (~0.35); gVisor's TCP
and checksum code is under 0.1.

`perf` inside the guest is not a usable oracle here: with no PMU it samples on a timer that
cannot fire inside interrupt-disabled code, and reported vCPU0 90% idle while `mpstat` and the
host sample both had it about half busy. Its ordering of the busy samples still pointed at
`virtnet_poll` → `receive_buf` → `gro_receive_skb`, with `do_csum` the top busy leaf.

## Fixes, host → guest iperf at MTU 65520

| change | throughput | worker CPU | gvproxy CPU | net IRQs / 30 s | guest CPU0 hard+soft IRQ |
|---|---|---|---|---|---|
| baseline | 8.0 Gbit/s | 165% | 230% | 820k | 21.6% + 46.1% |
| RX asks `needs_notification` | 7.9 Gbit/s | 150–175% | 230% | 584k | 15.3% + 43.9% |
| + `VIRTIO_NET_HDR_F_DATA_VALID` | 8.0 Gbit/s | 160–175% | 235% | 663k | 16.9% + 37.8% |
| + gvproxy `GOGC=400` | 8.7 Gbit/s | 167% | 186% | 708k | — |

- Linux's virtio-net NAPI parks `used_event` while it polls and, once the device has fired,
  trusts it not to fire again until `used_event` moves (`virtqueue_disable_cb_split` in
  `drivers/virtio/virtio_ring.c`). The device signalled every drain regardless. Honouring it
  removes 29% of the interrupts; the rest come from the guest finishing a poll before the next
  32 KB frame arrives and re-arming.
- A blank virtio-net header makes the guest verify every byte's checksum. The frames come out of
  gvproxy's own stack over a local socket, so the device marks them valid when the guest
  negotiated GUEST_CSUM. No checksum errors or discards in the guest afterwards.
- `GOGC=400` cuts gvproxy's collector work; gvproxy's footprint read 65 MB mid-run.
