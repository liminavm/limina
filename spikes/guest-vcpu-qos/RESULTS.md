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

## At 6 vCPUs

Three boots per arm (`results-2026-10-05-6cpu/`):

| arm | presented fps | guest timer, 16.7 ms period, p50 |
|---|---|---|
| off | 46.1 / 57.4 / 46.2 | 3.6 / 4.1 / 4.4 ms |
| band (shipped) | 42.3 / 59.6 / 42.4 | 0.2 / 3.8 / 4.0 ms |
| off + tier 0 | 47.8 / 60.0 / 57.1 | 1.5 / 1.9 / 1.3 ms |

The band does no better at 6 vCPUs than at 8. Tier 0 cuts the guest's timer lateness by the same
2.5x as at 8, every boot. Frame pacing is bimodal at this size: rep 2 ran at 57-60 fps in every arm,
so something on the host shifted across arms within that rep. Tier 0 had two good boots of three,
against one for each of the others. That is suggestive, not settled at n=3.

## Power

`power-arms.sh` marks the windows and `power-summary.py` reads them against a root `powermetrics
--samplers cpu_power -i 1000` capture taken alongside. Two reps, interleaved: no VM, then each arm
idle for 120 s and animating (fcprobe, 960x540) for 60 s. Results are in `power/summary.md`; the raw
capture is not committed.

| window | CPU mW per rep | mean | presented fps (animating) |
|---|---|---|---|
| no VM | 247 / 166 | 207 | - |
| off, idle | 254 / 183 | 218 | - |
| band, idle | 243 / 285 | 264 | - |
| tier 0, idle | 246 / 364 | 305 | - |
| off, animating | 4940 / 4892 | 4916 | 46.5 / 42.0 |
| band, animating | 5098 / 4881 | 4989 | 41.0 / 43.0 |
| tier 0, animating | 5191 / 5285 | 5238 | 59.5 / 59.4 |

- **Animating, tier 0 costs 6.5% more CPU power and delivers 34% more frames.** That is 88 mJ per
  presented frame, against 111 for no band and 119 for the shipped band. The 60 s windows reproduce
  the frame-rate split cleanly, and tier 0 had 30-34 frames over 25 ms against 678-1137 for the others.
- **Idle is not resolved.** The empty host drifted 81 mW between reps, more than any arm differs
  from another. Tier 0's two idle windows read +28 mW and +181 mW over the band-off arm of the same
  rep. Settling it needs more reps, or a quieter host than one carrying another session's VM.
- The ~5 W animating cost on this host is a debug-build figure: these runs used `target/debug`'s
  unoptimized worker. The release bundle on a base M1 animates the same client for 0.35-0.5 W (below).

## Base M1 (4 P + 4 E, 16 GB), one vCPU per host core

The original report's shape. The same vehicle and arms ran from a signed release bundle carrying
the knob (`power-arms-bundle.sh`), 3 reps, with presented fps over 60 s windows
(`power-abacate/`):

| arm | presented fps | frames >25 ms per 60 s |
|---|---|---|
| off | 42.3 / 40.9 / 41.8 | 1048 / 1143 / 1086 |
| band (shipped) | 42.7 / 41.1 / 41.2 | 1034 / 1129 / 1124 |
| **off + tier 0** | **59.4 / 58.1 / 59.6** | **35 / 114 / 25** |

The shipped band changes nothing on this shape either, and tier 0 restores 60 fps in every boot.
The power half of this run is void. Finder and `iconservicesagent` held that host's P-cluster
98-100% active at 3.2 GHz throughout (Finder had burned 462 CPU-hours over 54 days of uptime), so
the empty-host baseline read 4.3 W and no arm could be told apart from it.

## Idle power on a quiet base M1

After a runaway Finder was relaunched and the Aerial wallpaper replaced, the empty host read 24-44
mW. Same bundle, 3 reps, 180 s idle and 20 s animating windows (`power-abacate-idle/`):

| window | CPU mW per rep | mean | presented fps (20 s) |
|---|---|---|---|
| no VM | 27 / 44 / 24 | 32 | - |
| off, idle | 110 / 42 / 110 | 87 | - |
| band, idle | 60 / 44 / 44 | 49 | - |
| tier 0, idle | 63 / 55 / 39 | 52 | - |
| off, animating | 329 / 347 / 367 | 348 | 40.8 / 44.0 / 45.5 |
| band, animating | 488 / 513 / 305 | 435 | 59.7 / 59.9 / 38.8 |
| tier 0, animating | 450 / 485 / 487 | 474 | 54.9 / 54.3 / 59.0 |

- **Tier 0 adds no idle cost over the band**: about 20 mW over an empty host for an idle 8-vCPU
  GNOME guest, either way. Both off-arm reps that read 110 mW were the first boot after the
  baseline; the cause is unexamined, but it is not tier 0.
- **Animating, energy per frame is flat across arms**: 8.0 mJ off, 8.2 band, 8.4 tier 0.
- **On this quiet host the band reached 60 fps in two boots of three**, while the earlier abacate
  run, with the P-cluster pinned by Finder, gave it 41-43 in every boot. Tier 0 ran at 54-59 here.
  These are 20 s windows, so treat them as a hint. Whether the band works depends on host load,
  and band + tier 0 has not been run on this shape.

## Not yet known

- **Idle power** is settled on a quiet base M1 (see below): tier 0 costs no more than the band.
- **Other shapes.** The enhanced tier (venus, 16k kernel). Fewer vCPUs, where the band did help in
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
