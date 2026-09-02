# Hardware-decoded video after a snapshot restore: the codec was gone and nothing said so

Measured on the dev Mac (M1 Max, macOS 26.5), Debian testing guest (stock tier,
`Debian-testing.luks.raw`), managed suspend + restore through `limina suspend` / `limina start`.
Chrome on YouTube (2026-08-30) and Showtime on a local 4K VP9 file through gst-va (2026-09-02).

Vehicle: `RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info`, `LIMINA_GPU_TRACE=1`,
`LIMINA_VIDEO_TRACE=1`, `LIMINA_WINDOW_CAPTURE`. Artifacts here: `trace-excerpt.log` (the Chrome
run's GPU trace), `guest-screencast.mp4` (recorded inside the guest while the symptom was live),
`clocks.log` (a 50 Hz sampler of all four guest clocks across a restore), `debian-capture.png`.

## The finding

After the restore the host had **no video codec and no video buffers** for the guest's handles.
The classic re-creation journal filed `CREATE_VIDEO_CODEC` and `CREATE_VIDEO_BUFFER` as
"durable-unknown: counted, not kept" (`vrend_journal.c`), so the replay re-created every
resource, sampler view and shader of the context and none of its video objects. The guest kept
decoding: within five seconds of the Showtime restore the backend logged 905 times

```
virgl: video codec: 0x0, video buffer: 0x0, invalid.
```

and the player recycled its pool of decode targets — the pictures they already held, in pool
order (the "same few frames back and forth" of the Chrome screencast: 35 distinct pictures in
6 s, traversed in 17 ascending and 15 descending runs), and surfaces never written since
allocation as solid green (NV12 all-zero). Showtime then reported "a lot of buffers are being
dropped" and stopped; Chrome healed only when it tore its decoder down.

Fixed in virglrenderer `2c5f6e9c` (journal the creates; a fresh codec drops inter frames until
the next keyframe; a create that produced nothing returns EINVAL; lookup misses logged
rate-limited). Same cycle afterwards: the codec is re-created during replay, the next keyframe
re-seeds it, 670 decodes / 669 pictures in the next seconds, the player keeps playing with the
picture advancing.

The drop itself was still visible: with the codec back but dropping, the player presents its
pool of untouched targets in pool order until the keyframe (81–134 dropped frames in the
dogfood clips, 3–5 s of the same bounce, then the picture advances). The backend now freezes
the window on one picture: the first dropped frame's target is copied into every later one
(`copy_picture`). In the L2 vehicle (`l2_video_vaapi_restore`, 25 fps, keyframe per second)
the 18 dropped frames showed 7 distinct pictures before and one after.

## What the measurements rule out, and the one they misled

| candidate | measurement | verdict |
| --- | --- | --- |
| guest clock discontinuity | 1203 samples across a 181 s restore: `CLOCK_MONOTONIC` +0.94 s, `CLOCK_REALTIME`/`BOOTTIME` +181.07 s, zero backward steps on any clock | not the cause |
| player / compositor scheduling | same cycle with Chrome's VA-API decoder disabled (zero `drv_video` mappings, renderer at ~64 % CPU decoding in software) plays back cleanly | not the cause; it is the hardware path |
| host presentation | the bounce is already present in the guest's own screencast, upstream of scanout and window present | not the cause |
| journal gap (video objects not re-created) | **GPU trace: `unknown_ctx=+0 unknown_res=+0 errs=+0`, ~145 accepted submits/s after the restore** | **wrongly excluded** |

The counters cannot see this fault. `vrend_decode_begin_frame`, `vrend_decode_decode_bitstream`
and `vrend_decode_end_frame` discard the return of `vrend_video_*` and return 0, so a decode
against a codec the host does not have is an accepted, error-free submit. The only witness was
the backend's own log line, at `warn`, which the first run did not have open. **Rule: never
exclude a video theory on the GPU error counters; read the video backend's log.** The `errs`
counter is exactly as good as the handlers that feed it, and these do not.

## What the guest's screencast said, correctly

Recorded inside the guest (mutter's composition) while the symptom was live: a bounded pool of
already-decoded pictures re-presented in alternating order, with new pictures arriving only in a
trickle. That is the signature of a player handing its compositor surfaces whose *content* no
longer follows the stream — a dead decoder behind a pool of live surfaces — and it placed the
fault in the guest's surfaces, which was right. The reading that then needed the host-side
counters, "the commands must be rejected", was the step that went wrong.
