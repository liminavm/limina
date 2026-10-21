# netd-tcp-translate — passt-style TCP for limina-netd on macOS

`limina-netd` (`docs/design/multi-vm-networking.md` §3.2) maps each guest TCP connection onto a
host socket by translating sequence numbers, with no TCP stack of its own. passt does that on
Linux with props macOS lacks: `SO_PEEK_OFF` (peek from an offset) and a cheap discard of peeked
data. This spike measures, on the host alone, what each way of holding the guest's unACKed data
costs, and how netd can learn a socket's buffer state.

**The bar is gvproxy, which netd replaces**, not vmnet: on the same M1 through a real guest, gvproxy
moved 13–15 Gbit/s guest → host and 7.2–7.5 Gbit/s host → guest, at 23–35% and ~50% host CPU per
Gbit/s (`spikes/net-bench/RESULTS.md`). These host-only numbers leave out the worker, the ring hop
and the guest, so they bound netd's own share of the cost; they are not end-to-end throughput.

Build and run: `./run.sh [probe|rx|tx|all]` (`SECS` per run, default 5; every run under
`gtimeout`). `NETD_SPIKE_NOBUILD=1` runs binaries built elsewhere.

- `probe.c` — a loopback pair whose peer reads nothing, then drains: `TCP_CONNECTION_INFO`'s
  `tcpi_snd_sbbytes`, `SO_NWRITE` and `EVFILT_WRITE`'s data on the send side, `SO_NREAD` and
  `EVFILT_READ`'s data on the receive side.
- `rx.c` — host → guest. A child writes into loopback as fast as it can; this process plays netd,
  delivering to a guest emulated as a lossless consumer (memcpy into a ring in 64 KiB frames) that
  ACKs 100 µs later, with at most `window` bytes in flight. Three strategies:
  - **peek** (passt): peek the receive queue from its head each round, deliver past what is in
    flight, read out what the guest ACKed;
  - **retain**: read once into a per-connection buffer, deliver from it, free on the ACK;
  - **forget**: read straight into the guest frame, keep nothing.

  `drop` loses one frame after 1 GiB; ACKs stop at it until netd resends from the last ACK.
- `tx.c` — guest → host: 64 KiB writes into loopback, with the room known by `getsockopt`
  (`sockopt`), from the write event (`kevent`), or not tracked (`none`).

Only netd's CPU (`getrusage`) is counted; the far end is a separate process.

## Measured 2026-10-19, M1 Mac mini (8 cores, macOS 27.0.1), quiet

### Socket state comes with the events

| | Send side | Receive side |
|---|---|---|
| Event data | `EVFILT_WRITE` data = `SO_SNDBUF − tcpi_snd_sbbytes`, exactly, while there is room; no event when full | `EVFILT_READ` data = `SO_NREAD` |
| Unacked bytes | `tcpi_snd_sbbytes` = `SO_NWRITE`: what the send buffer holds, sent-but-unacked included; it falls as the peer ACKs | — |
| Buffers | autotuned unless set: send 147 → 172 KB | receive 130 → 260 KB as data arrived |

netd learns room to write and bytes to read from the events it already waits on. The peer-ACKed
count (what passt ACKs the guest on) needs `tcpi_snd_sbbytes` or `SO_NWRITE`: the write event gives
`sndbuf − sbbytes`, and the send buffer autotunes, so the difference alone does not pin `sbbytes`.

### Host → guest (`rx`)

Gbit/s, and netd CPU per Gbit/s, ACK lag 100 µs:

| window | socket buffers | forget | retain | peek |
|---|---|---|---|---|
| 128 KiB | autotuned | 7.2, 1.9% | 7.2, 2.0% | 7.4, 2.6% |
| 512 KiB | autotuned | 34.7, 1.6% | 30.2, 1.1% | 29.1, 3.4% (CPU 100%) |
| 2 MiB | autotuned | 102.5, 1.0% | 111.7, 0.9% | 28.1, 3.5% (CPU 98%) |
| 8 MiB | autotuned | 98.4, 1.0% | 62.7, 1.6% | 28.1, 3.5% (CPU 98%) |
| 128 KiB | 8 MiB | 7.8, 4.1% | 7.5, 3.0% | 7.8, 4.3% |
| 512 KiB | 8 MiB | 36.0, 1.8% | 34.4, 1.6% | 26.0, 3.8% (CPU 100%) |
| 2 MiB | 8 MiB | 55.4, 1.8% | 75.6, 1.3% | 26.3, 3.8% (CPU 100%) |
| 8 MiB | 8 MiB | 55.5, 1.8% | 42.8, 2.3% | 26.2, 3.8% (CPU 100%) |

One frame lost (2 MiB window, 8 MiB buffers): **forget stalls for good** (nothing to resend);
retain and peek resend once and run on at their usual rate.

- **peek is CPU-bound at 26–29 Gbit/s from a 512 KiB window up**, at 3.4–3.8% per Gbit/s, two to
  three times the read strategies. Without `SO_PEEK_OFF` each round copies everything in flight
  again; the window it can afford is what caps it.
- **retain costs what forget costs** (0.9–2.3% against 1.0–1.8%): the extra memcpy into netd's own
  buffer is small next to the syscalls, and the buffer it keeps (one window per connection) is
  what makes a resend possible.
- A 128 KiB window is ACK-lag-bound for all three (128 KiB / 100 µs ≈ 10 Gbit/s).
- Pinning 8 MiB socket buffers did not help over autotuning, and cut the 2 MiB-window peaks from
  102–112 to 55–76 Gbit/s.
- All three clear gvproxy's host → guest bar (7.2–7.5 Gbit/s at ~50% per Gbit/s) by a wide margin
  on netd's share of the cost.

### Guest → host (`tx`)

| room known by | autotuned | 8 MiB buffers |
|---|---|---|
| `kevent` (write-event data) | 53.7 Gbit/s, 1.8% per Gbit/s | 53.7, 1.9% |
| `sockopt` (`TCP_CONNECTION_INFO` per write) | 53.9, 1.8% | 52.1, 1.9% |
| `none` (write 64 KiB until `EAGAIN`) | 0.01 | 0.03 |

- **A non-blocking write larger than the free space mostly fails with `EAGAIN`** rather than
  writing short, and the edge-triggered write event then often does not come (most `none` waits
  ran out their 1 ms timeout); registered level-triggered, the event fires but the same write fails
  again, a busy loop at 0.07–0.13 Gbit/s. **netd must size every write to the room the write event
  reported**, which it gets for free.
- A `getsockopt(TCP_CONNECTION_INFO)` per write cost nothing measurable, so reading
  `tcpi_snd_sbbytes` to ACK the guest on what the peer has ACKed is affordable.

## What this means for netd

- **Host → guest: read-and-retain, not peek.** Same sequence translation and no TCP stack, but
  netd reads the socket once into a per-connection buffer of the guest's window and frees on the
  guest's ACK. It costs what read-and-forget costs, keeps the ability to resend a frame the guest
  lost, and avoids peek's 3× CPU and ~27 Gbit/s ceiling.
- **Guest → host:** write what the write event says fits; ACK the guest by `tcpi_snd_sbbytes`
  falling (peer-ACKed), as passt does.
- **Event loop:** kqueue data carries readable bytes and writable room; no `getsockopt` on the
  data path except the ACK count.

Not measured here: the end-to-end path through the worker and a guest, many connections at once,
UDP, and a real network instead of loopback.
