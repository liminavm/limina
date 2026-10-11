# What headless frame capture costs

Measured 2026-10-10 with `measure.sh`: stock F44 (`stock.test`, `cp -c` clone), EFI + coexist on
KosmicKrisp, 4 vCPU / 4096 MiB, 1280x800, M1 Max, macOS 26.6.2, **debug builds** of `limina` and
`limina-vmm`, host otherwise idle (load ~1.7, no other VM). Each run is `vkcube --c 600` in the
guest's GNOME session, timed in the guest (startup included); CPU is `ps -o time` deltas over the
run divided by its wall time.

| mode | guest vkcube fps | worker CPU | supervisor CPU |
|---|---|---|---|
| `png` — `--display-capture` (no gate) | 59.8 / 60.0 / 60.1 | 60-73% | 0.1% |
| `gate` — `--headless-capture`, no capture | 58.1 / 55.3 / 54.8; 53.6 / 47.1 / 53.8; 41.0 | 57-67% | 1.2-1.7% |
| `capture` — gate on, `capture start`/`stop` around each run | 45.2 / 41.3 / 42.7; 42.0 | 54-62% | 42-44% |
| `window` — `--window`, for reference | 41.3 / 41.3 / 42.4 | 55-57% | 31% |

Capture summaries (three runs): 603-609 frames received, 601-609 captured, 0-2 dropped, 0 never
presented; 41-44 captured fps, ~0.45 MB a frame (~270 MB per 600); per-frame cost on the reader
thread p50 0.49-0.55 ms, max 4-37 ms.

- **The gate's cost is the worker's window present path, not the supervisor.** The supervisor's
  reader and ack threads cost 1.2-1.7% CPU. The guest runs slower under the gate than on the PNG
  sink, which presents through a CPU readback with no flip fence, and at least as fast as under a
  window, which runs the same worker path. Turning fence presents off under the gate
  (`LIMINA_FENCE_PRESENT=0`) did not recover the rate (42.9 / 39.9 / 42.9 fps), so the ack is not
  the brake.
- **The shown-ack gate never waited without a capture.** An instrumented run counted acks whose
  replaced surface was still in use: 0 of 1800 with the gate alone, 1 of 600 with a capture.
- **A running capture costs the guest about a quarter of its rate here**, and the supervisor
  ~43% of a core, nearly all of it PNG encoding on four threads in a debug build. The gate run's
  spread (41-58 fps over seven runs) is larger than the effect of anything toggled within it, so
  read single runs with care.
