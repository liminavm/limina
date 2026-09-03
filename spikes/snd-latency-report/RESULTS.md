# What virtio-snd never told the guest

`devlatency.swift` prints the CoreAudio output latency that sits *downstream of our render
callback* — the part of the audio path the guest cannot infer for itself.

## The gap

The guest driver feeds `virtio_snd_pcm_status.latency_bytes` straight into `runtime->delay`
(`sound/virtio/virtio_pcm_msg.c`, in every kernel since 5.13), and that is what
`snd_pcm_delay` — and so PipeWire, and so every player's A/V sync — is built on. The device
wrote `latency_bytes: 0` in every status word, so `runtime->delay` was pinned at zero and
applications believed audio was audible the instant CoreAudio's callback consumed it.

Frames still in our SPSC ring or the virtqueue are **not** part of this figure. A tx
descriptor is completed only once the callback has consumed its frames, so the guest's own
`appl_ptr - hw_ptr` already covers them; counting them again would double the delay.

## Measured

MacBook Pro (M1 Max, macOS 26.5), built-in speakers, 48 kHz, 2026-09-03:

| term | frames |
| --- | --- |
| device latency (`kAudioDevicePropertyLatency`) | 70 |
| stream latency (`kAudioStreamPropertyLatency`) | 690 |
| safety offset | 74 |
| IO buffer | 512 |
| **total** | **1346 (28.0 ms)** |

The stream term dominates and is not folded into the device term — query both.

Bluetooth output is the case that hurts: the link latency lands in the same device property
and runs to a couple of hundred milliseconds. Reported as zero, it is a lipsync error the
user sees as sound trailing the lips.

## It reaches ALSA and stops there

Reporting `latency_bytes` is necessary but, on a PipeWire guest, not sufficient. Measured on
F44 (PipeWire 1.6.2) with `paplay --latency-msec=50`, the two arms selected by
`LIMINA_SND_ZERO_LATENCY=1`:

| device reports | ALSA `delay` − (appl−hw) | app-visible `pa_stream_get_latency` |
| --- | --- | --- |
| 1346 frames | 1346 | 78 190 µs |
| 0 (old behaviour) | 0 | 78 267 µs |

The kernel side moves by exactly the reported amount. The figure an application sees does not
move at all — 77 µs against the 28 000 µs it should have gained.

The drop is in PipeWire's ALSA sink, not in its PulseAudio compatibility layer: the sink node's
`Latency` param reads 512 frames — its own period — in **both** arms, so the device's
`snd_pcm_delay` component is never folded into the graph latency. Clients that call
`snd_pcm_delay` directly do see it; anything going through PipeWire does not.

So a player on a stock PipeWire guest still schedules video against an audio clock that is short
by the whole device latency. Closing that needs a guest-side change, and the two-tier guarantee
says a stock guest must keep working without one.

## Oracle

In the guest, while audio plays:

```
grep -E '^delay|^hw_ptr|^appl_ptr' /proc/asound/card0/pcm0p/sub0/status
```

`delay - (appl_ptr - hw_ptr)` is the frames the device claims to hold. It must equal the
figure the worker logs as `snd: host output latency N frames`. It was 0 on every sample
before the fix and 1346 on every sample after.
