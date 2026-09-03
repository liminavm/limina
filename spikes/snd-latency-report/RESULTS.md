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

## Oracle

In the guest, while audio plays:

```
grep -E '^delay|^hw_ptr|^appl_ptr' /proc/asound/card0/pcm0p/sub0/status
```

`delay - (appl_ptr - hw_ptr)` is the frames the device claims to hold. It must equal the
figure the worker logs as `snd: host output latency N frames`. It was 0 on every sample
before the fix and 1346 on every sample after.
