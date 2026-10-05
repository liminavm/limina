# Latency-QoS tier 0 on the vCPU threads, at guest level

`spikes/launch-path-qos/` found that a host thread's timer wakes late by a fixed share of how long
it slept: a third under the default QoS, a fifth at latency-QoS tier 0. Only the RT band defeats it.
This spike asks whether that reaches a guest through HVF's in-`hv_vcpu_run` park, and whether tier 0
on every vCPU thread fixes what the guest sees. The guest-visible symptom is a stock guest's
Wayland clients running at about 45 fps on a 60 Hz output.

Measured 2026-10-05 on the dev Mac (M1 Max, 8 P + 2 E, macOS 26.6.2). The guest was a CoW clone of
`Fedora-Workstation-44.stock.test.raw`: stock kernel `6.19.10-300.fc44`, stock Mesa `26.1.8`, a
seated GNOME session, 8 vCPUs and 8 GiB. It booted EFI with a headless display
(`LIMINA_DISPLAY_CAPTURE`), so the guest's vblank is the virtio-gpu driver's own timer. Another
session's idle Debian VM (6 vCPUs, one banded) ran throughout.

## Verdict

**Tier 0 on every vCPU thread brings a stock guest's frame pacing to about 57 fps, and the shipped
band does not.** Five boots per arm, interleaved:

| arm | presented fps (mean, range) | frames >25 ms per 10 s | commit→present p50 | guest timer, 16.7 ms period, p50 / p99 |
|---|---|---|---|---|
| off (no band) | 46.3 (41.9–52.1) | 134 | 58.9 ms | 3.9 / 8.1 ms |
| band (`rt+dyn#1`, shipped) | 42.6 (41.1–46.1) | 171 | 63.2 ms | 2.5 / 7.9 ms |
| **off + tier 0** | **57.2 (51.0–60.0)** | **28** | **37.9 ms** | **1.7 / 3.7 ms** |
| band + tier 0 | 55.9 (54.7–59.4) | 40 | 39.7 ms | 1.0 / 2.9 ms |

- **The host slack reaches the guest.** With nothing set, a guest `clock_nanosleep` loop lands about
  a quarter of its period late: 3.9 ms on 16.7 ms, about 1 ms on 4 ms, and 0.2–0.4 ms on 1 ms. That
  is the same proportional shape as the host thread's 1/3 (default QoS, on an E-core).
- **The shipped band does nothing for this guest, though it works on the vCPU it holds.** Its last
  band event in every boot was "took the band", so it was armed on vCPU 0 throughout the
  measurements. The unpinned `timerlat` shows the split: its 16.7 ms p50 under the band was 4508 /
  3182 / 237 / 226 / 4339 µs. Two boots landed on the banded vCPU and three did not. With 8 vCPUs,
  the deadlines that pace frames mostly land on the other seven. The unbanded baseline reproduces
  the design doc's (46.3 against 43.6 / 39.2 FPS); its 52 FPS for the band on vCPU 0 alone, measured
  with `vkcube` on a smaller guest, does not. That isolates the vCPU count.
- **Tier 0 is not a band.** It changes no priority and reserves nothing, so it has none of the
  band's panic risk and needs no cap. It applies to every vCPU.
- **Adding the band on top of tier 0 buys nothing measurable.**
- **The wait that matters runs under the vCPU thread's policy.** A policy set on the vCPU thread
  moved the guest's timer lateness, which settles the band design doc's open question about HVF's
  own `VirtualClock` thread.
- **Neither touches the low-duty slowdown.** A guest thread at 6% duty runs its chunk in 500 ns in
  every arm, against 84-125 ns at 67% duty: a 4-6x slowdown. Futex-wake cost after a 5 ms idle
  gap is 19-27 µs in every arm. That is placement on E, and no thread policy moves it
  (`spikes/launch-path-qos/`: only a workgroup with bracketed intervals does, and a vCPU cannot
  bracket them).

## Not yet known

- **Power.** Tier 0 shrinks the timer slack; it does not add wakeups. Slack is what lets macOS batch
  wakeups, though, so measure idle package power (`powermetrics`, needs root) before making it the
  default.
- **Other shapes.** The base M1 (4 P + 4 E) with one vCPU per host core, which is where the original
  report came from. The enhanced tier (venus, 16k kernel). Fewer vCPUs, where the band did help in
  August.
- **Why the band adds nothing on top of tier 0.** A lead: `dutyprobe` at 6% duty runs pinned to
  guest CPU 2, not the banded vCPU. Its p90 chunk time was 541 ns to 78 µs in 9 of the 10
  band-family boots, against 500-625 ns in all 10 band-off boots. A 500 ns chunk that takes 50 µs
  means its vCPU's host thread lost its core. A mostly idle RT thread sits on an E-core, where light
  vCPU threads also crowd, which would be consistent with that. It is not measured from the host
  side.

## Method

- `libkrun-latency-qos.patch` (libkrun at `e493221a`): `LIMINA_VCPU_LATENCY_QOS=<0-5>` puts every
  vCPU thread in that latency-QoS tier at start (`vcpu_sched::set_latency_qos`, called before
  `set_realtime_band`). It is off unless set, and the band overrides it while armed.
- `run-arms.sh <out> <reps>`: one boot per arm, with the arms interleaved within a rep. Each boot
  waits for ssh, runs `measure.sh`, saves the `[VCPU-RT]` lines and powers off.
  `LIMINA_VCPU_SCHED=""` turns the band off.
- `measure.sh` (in the guest, after a 20 s settle): `timerlat` at 16.7, 4 and 1 ms;
  `dutyprobe 5000 60 {300,10000}` (median chunk time after the first 100 µs of each burst);
  `wakecost 2 5 5000 600`; and `fcprobe --seconds 10 --size 960x540` in the seated session.
- `probes/`: these tools are copied unchanged from the investigation that reported the symptom.
- Results: `results-2026-10-05/` (2 reps) and `results-2026-10-05-b/` (3 reps); `summary.md` puts all
  five side by side (`summarize.py`).

## Traps

- **One boot is not a measurement.** A smoke boot on the shipped band read 59.6 fps, and the five
  measured boots of the same arm read 41.1–46.1.
- The guest needs `gcc` and `wayland-devel` to build the probes. Stock images lack them, so the
  vehicle clone had them installed once over `--net`.
