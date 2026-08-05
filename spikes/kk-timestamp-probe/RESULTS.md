# kk-timestamp-probe — KosmicKrisp is NOT the venus timestamp-query gap

**Run 2026-07-25**, dev Mac (Apple M1 Max, macOS 26.5), local KK build
(`/Volumes/mesa-cs/build-kk`), no VM involved.

## Why

`gnome-shell-rs` reports (`docs/fork/venus-timestamp-gap.md`) that under venus every
`VK_QUERY_TYPE_TIMESTAMP` query resolves to **value 0 with availability 1** — a well-formed
zero, so there is no error to branch on. That doc's §5 asks for exactly one experiment first:
*does the host driver pass the same reproducer natively?* This is that experiment.

Three cases, because the guest and a native app hit **different** KK code paths:

- **A** — `vkGetQueryPoolResults` → `kk_GetQueryPoolResults` (CPU readback).
- **B** — `vkCmdCopyQueryPoolResults` into a host-visible buffer → `libkk_copy_queries`
  (the GPU kernel in `src/kosmickrisp/libkk/kk_query.cl`).
- **C** — B, with the copy in a **separate** command buffer submitted alongside.

C matters because Mesa venus **never calls `vkGetQueryPoolResults` on the host**: it serves the
guest from a guest-visible *feedback buffer* which it fills with a `vkCmdCopyQueryPoolResults`
recorded into a linked feedback command buffer
(`mesa/src/virtio/vulkan/vn_query_pool.c: vn_get_query_pool_feedback`,
`vn_feedback.c: vn_query_feedback_cmd_record_internal`). So C is the shape the guest actually
produces.

## Result — all three pass

```
device: Apple M1 Max
  timestampPeriod    = 1
  timestampValidBits = 64 (queue family 0)

A  vkGetQueryPoolResults      -> 0  q0=1014295388863083 avail=1  q1=1014295388895333 avail=1
   delta = 32250 ns
B  vkCmdCopyQueryPoolResults  ->    q0=1014295389409333 avail=1  q1=1014295389436833 avail=1
   delta = 27500 ns
C  vkCmdCopyQueryPoolResults  ->    q0=1014295390004083 avail=1  q1=1014295390032083 avail=1
   delta = 28000 ns
```

Large, advancing, plausible deltas for an otherwise-empty command buffer. KK's timestamp
implementation (`2cf4599ed74`, "kk: implement timestamp queries") is correct on the direct
readback path **and** on the query-copy kernel path, including a cross-command-buffer copy.

## What this rules in and out

- **KK is exonerated** for the guest symptom, at least on M1 Max. Its convention for a
  TIMESTAMP pool is `UINT64_MAX == unavailable` (`kk_query_pool.c: kk_has_available`,
  `libkk/kk_query.cl: libkk_reset_query`), so an unwritten-but-reset query reads
  `0xFFFFFFFFFFFFFFFF`, **not** 0. The guest's `value 0, avail 1` is therefore *not* "KK
  never wrote the timestamp" — it is a pool report holding literal zero.
- **venus's query-feedback path is exonerated too.** Re-running the guest reproducer on
  `dogfood-guest` with `VN_PERF=no_query_feedback` (forces the synchronous
  `vn_call_vkGetQueryPoolResults` host round trip, bypassing the feedback buffer entirely)
  still returns `[0, 0]` / avail 1. So the **host-side query pool genuinely holds zero**.
- **virglrenderer's venus backend is a pass-through** for this: `vkr_query_pool.c` forwards
  `vkGetQueryPoolResults` verbatim, and `vkr_command_buffer.c:519/883` dispatch both
  `vkCmdWriteTimestamp` and `vkCmdWriteTimestamp2` with no special handling.

## In-VM runs — it works here, on both KK builds

Follow-up the same day, on a local `Fedora-Workstation-44.enhanced.raw` clone booted with
`boot-enhanced-efi-kk.sh` (EFI + venus, coexist, `--window --net`). Guest side is **identical**
to the failing one: mesa `26.1.4-3.limina.fc44`, kernel `7.1.4-limina16k`.

| run | host GPU | host KK | `probe` A/B/C | gnome-shell-rs repro |
|---|---|---|---|---|
| guest, devenv KK | M1 Max | `build-kk` devenv (2026-07-24) | **pass** | **pass** |
| guest, packaged KK | M1 Max | `limina.app` bundle, byte-identical to the one deployed on the failing Mac (18 186 000 B, 2026-07-25 17:03) | **pass** | **pass** |
| guest (reported) | **M4 Pro** | same packaged KK | — | **fail** (0 / avail 1) |

The second row is the important one: the *exact* deployed `libvulkan_kosmickrisp.dylib`, driven
through the *exact* venus path, returns real advancing timestamps. `boot-enhanced-efi-kk.sh`
gained a `LIMINA_KK_ICD` override to make that A/B possible without touching the dogfood Mac.

The `gnome-shell-rs` reproducer binary was copied over unchanged and run in this guest — so the
guest application code is controlled for too.

**Everything except the GPU is now eliminated**: guest Mesa, guest kernel, the reproducer, the
venus transport, virglrenderer, the VMM, and both KK builds. What is left is **Apple M1 Max vs
M4 Pro**.

That points at Metal counter sampling. `mtl_device_supports_timestamps` gates on
`[MTLDevice supportsCounterSampling:MTLCounterSamplingPointAtStageBoundary]`
(`bridge/mtl_device.m:182-192`), and the failing guest reports `timestampValidBits = 64`, so the
M4 Pro *claims* stage-boundary sampling. The suspicion is that it claims it and then does not
populate the sample — `resolveCounters` on an unpopulated slot writing zero would reproduce the
symptom exactly, because KK's "available" test for a timestamp pool is `value != UINT64_MAX`
(`libkk/kk_query.cl:70-71`), so a zero reads as *available*. KK already documents one such
zero-mode ("resolving a slot in its own sampling encoder reads zero",
`kk_encoder.c:394-397`); this would be a second, hardware-dependent one.

## ROOT CAUSE — M4 Pro cannot resolve a counter sample from the command buffer that took it

`probe.c` run natively on the M4 Pro host, against the packaged app's KK, **no VM**:

```
device: Apple M4 Pro
  timestampValidBits = 64
A  vkGetQueryPoolResults      -> 0  q0=0 avail=1  q1=0 avail=1
B  vkCmdCopyQueryPoolResults  ->    q0=0 avail=1  q1=0 avail=1
C  vkCmdCopyQueryPoolResults  ->    q0=0 avail=1  q1=0 avail=1
```

The guest symptom, reproduced with no guest. So this was never a venus bug.

`mtlprobe.m` then drops to raw Metal — no Vulkan at all — and walks KK's sampling shape plus the
variants it could be swapped for. Both machines, same binary:

| variant | M1 Max | M4 Pro |
|---|---|---|
| shared sample buf + **CPU** `resolveCounterRange:` | ok | **ok** |
| shared sample buf + **GPU** `resolveCounters:`, same cmd buffer — *KK's path* | ok | **ZERO** |
| private sample buf + GPU `resolveCounters:`, same cmd buffer | ok | **ZERO** |
| shared sample buf + GPU resolve into a private dst, blitted back | ok | **ZERO** |
| shared sample buf + GPU `resolveCounters:` **in a separate command buffer** | ok | **ok** |
| KK's literal shape (`sampleCount = 1`, start-of-encoder only, resolve range (0,1)) | ok | **ZERO** |
| …same, read back with CPU `resolveCounterRange:` | ok | **ok** |

**The sample is taken correctly on M4 Pro.** Both the CPU resolve and the separate-command-buffer
GPU resolve return real, plausible, advancing timestamps. What fails is narrower and exact:

> On M4 Pro, a counter sample is **not visible to a `resolveCounters:` encoded in the same
> `MTLCommandBuffer` as the encoder that took it.** It materialises at command-buffer completion.
> A resolve encoded in a *later* command buffer reads it fine. M1 Max has no such restriction.

`kk_encoder_write_timestamp` (`kk_encoder.c:361-407`) resolves in a **separate encoder of the same
command buffer** — deliberately, so the write lands in GPU command order and cannot race an
in-stream `vkCmdResetQueryPool` / `vkCmdCopyQueryPoolResults` or KK's GPU-encoded fence signal.
That reasoning is sound and it is exactly the shape M4 Pro does not honour. The existing comment
"resolving a slot in its own sampling encoder reads zero" turns out to be the M1-visible corner of
a wider hardware rule.

Note this is silent by construction: `resolveCounters:` does not fail, it writes zero, and KK's
availability test for a timestamp pool is `value != UINT64_MAX` (`libkk/kk_query.cl:70-71`), so a
zero reads back as a *resolved zero*. Which is precisely what the guest reported.

### Fix direction

Not a capability drop — the hardware can do this. KK needs the resolve to land in a **later**
command buffer than the sample, while keeping GPU ordering:

1. Self-test once at device init (this probe's same-cb variant is ~30 lines) and set a device flag
   when the same-command-buffer resolve returns zero. Hardware-detected beats a GPU allowlist.
2. On such devices, have `kk_encoder_write_timestamp` end the current `MTLCommandBuffer` after the
   sampling encoder and encode the resolve into the next one. Commit order on a queue is execution
   order, so ordering against `CmdResetQueryPool` / `CmdCopyQueryPoolResults` and the fence signal
   is preserved — which is the property the current same-cb design exists to guarantee.
3. Keep `timestampValidBits = 0` as the fallback if the self-test cannot be satisfied at all: the
   conforming answer, and `gnome-shell-rs` already handles it silently.

Worth reporting to Apple as well — `resolveCounters:` silently writing zero, rather than
`MTLCounterErrorValue`, is what made this cost a day of guest-side investigation.

## Running it

```
./run.sh                                     # Vulkan probe, against our KK build
clang -g -O0 -fobjc-arc -framework Metal -framework Foundation -o mtlprobe mtlprobe.m && ./mtlprobe
```

`mtlprobe` needs no Vulkan and no VM — run it on any Mac to classify that GPU's counter-sampling
behaviour. Note the sampling encoder must carry **real work**: an empty blit encoder is elided
before it reaches the GPU and every variant then reads `[0, 0]`, which looks exactly like the bug.

## Running it

```
./run.sh                    # builds probe.c and runs it against our KK build
KK_ICD=/path/to/icd.json ./run.sh
```

Needs `/Volumes/mesa-cs` mounted (or `KK_ICD` pointed elsewhere). `0xABAB…` in the B/C output
would mean the copy never ran; `0` with `avail=1` is the guest symptom.

## Fixed — `patches/kosmickrisp/0010`

`mtl_device_needs_split_counter_resolve` (`bridge/mtl_device.m`) probes the failing shape once at
first use — sample a blit encoder that carries real work, resolve it in the same command buffer,
check for zero — and where that fails `kk_encoder_write_timestamp` retires the current
`MTLCommandBuffer` and encodes the resolve into the next one. `struct kk_encoder` gained an
`extra_cmd_buffers` list, committed ahead of `main` (command buffers execute in commit order, so
the in-stream ordering the same-command-buffer design exists to guarantee is preserved) and
released with it. The fence signalled by the last encoder before a split is waited on by the first
after it — the same fence-across-command-buffers chaining `kk_queue_submit` already uses between
consecutive submissions.

Detected by measurement, not by GPU family: a zero is indistinguishable from a real result to
every caller, so there is nothing to allowlist against.

Validated on M1 Max with `LIMINA_KK_SPLIT_COUNTER_RESOLVE=1` forcing the path on hardware that
does not need it — so this exercises the new code, not the workaround:

| | auto (no split) | split forced |
|---|---|---|
| `probe` A/B/C, natively | pass | **pass** |
| `probe` A/B/C, in-guest through venus | pass | **pass** |
| `gnome-shell-rs` reproducer, in-guest | pass | **pass** |
| `vkmark` (no timestamps → no splits) | — | 2001, no crash |

**Still outstanding: confirm on M4 Pro.** Needs a KK + app rebuild deployed there. Expected result
is that `mtlprobe`'s "GPU resolveCounters, same cmd buffer" row stays ZERO (the hardware behaviour
is unchanged) while `probe` and the compositor's `NIRI_FRAME_LOG=1,gpu` start reporting real GPU
times.

---

## CORRECTION (2026-07-25, on the deployed M4 Pro): the defect is INTERMITTENT, and `0010` is not a fix

The M4 Pro confirmation above ran, and it fails. `gnome-shell-rs` still reads `[0, 0]` on dogfood-mac
with `0010` deployed — verified present in the bundle (`nm` shows
`_mtl_device_needs_split_counter_resolve` at the same address as the local build) and running
(the VMM started 20:22, after the 20:20 deploy).

The reason is that **the original root cause was over-fitted to a single run.** Repeating
`mtlprobe` 50 times on that machine:

| shape | ok | zero | failure |
|---|---|---|---|
| KK shape, same command buffer (pre-fix) | 3 | 47 | **94%** |
| **KK shape, separate command buffer — what `0010` ships** | 41 | 9 | **18%** |
| KK shape, separate command buffer **+ waitUntilCompleted** | 50 | 0 | **0%** |
| KK shape, **CPU** `resolveCounterRange` | 50 | 0 | **0%** |
| the detection probe's own shape (count=2, same cb) | 2 | 48 | 96% |

Three things follow, and each was invisible from a single sample:

1. **It is not deterministic.** Two back-to-back runs disagreed on three separate rows — the
   first reported `private + GPU resolveCounters` and `resolve -> private` as working and the KK
   shape as broken; the second reported the exact opposite. Everything here is a rate, not a
   property, and "measured on M4 Pro" in the §ROOT CAUSE heading above should be read as "usually
   fails on M4 Pro".
2. **`0010` helps but does not fix**: 94% → 18% failure. Shipping it on the strength of one clean
   M1 Max run with the path forced was not enough evidence, and the shape it was validated
   against (count=2, case 5) is not the shape it emits (count=1, case 6) — those were never
   tested together until now. That gap is the direct cause of this correction.
3. **Commit order is the wrong guarantee.** `0010`'s comment argues the resolve is safe because
   command buffers run in commit order. They do, and it still fails 18% of the time: the sample
   becomes visible at command-buffer **completion**, not at execution. Only waiting for
   completion (0/50) or resolving on the CPU (0/50) is reliable.

### Why the guest sees 100% failure rather than 18%

8/8 guest runs read `[0, 0]`. At an 18% per-sample failure rate that is ~1e-6, so the split is
almost certainly **not being taken at all** on that VM. The detection is the suspect: it took
**one** sample and defaulted to "unaffected", and its own shape passes 4% of the time — so ~4% of
boots cache "healthy" for the process lifetime. Every setup failure (nil counter set, refused
allocation) landed on the same default, silently. That is consistent with 0.94^8 = 0.61 for the
observed run of guest zeros.

Fixed in **`patches/kosmickrisp/0011`**: start at "affected", take up to 8 samples, and let only
an unbroken clean run clear the flag. Verified no regression on M1 Max (probe A/B/C still pass,
detection still resolves to "unaffected" there).

### What still needs doing

`0011` restores the *intended* behaviour of `0010`, which is an 18% failure rate — better than
94%, not good enough. The real fix is to **replace the GPU `resolveCounters:` with a CPU
`resolveCounterRange` at command-buffer completion** (0/50 failures, and unlike the completion
wait it does not stall). It is a design change, not a patch: it moves the report write out of GPU
command order, so it has to be reconciled with an in-stream `CmdResetQueryPool` /
`CmdCopyQueryPoolResults`. Deliberately not bundled here — shipping a second workaround validated
against the wrong shape is the mistake this correction exists to record.

Until that lands, `timestampValidBits = 0` remains the honest fallback for affected devices;
`gnome-shell-rs` already handles it silently, and it is strictly better than reporting support
and returning zeros.

---

## ACTUAL ROOT CAUSE (2026-07-25): the sampling encoder is empty, so the sample is never taken

The correction above said `0010` was a partial fix for an intermittent resolve defect. That was
still wrong about the *primary* cause. Instrumenting the driver settled it.

`mtl_counter_sample_buffer_cpu_peek` (a CPU `resolveCounterRange:`) reads the sample buffer
directly — the one path that cannot be blamed on the GPU resolve. Run against the **deployed**
driver on dogfood-mac's M4 Pro, with `LIMINA_KK_TS_TRACE=1`:

```
[KKTS] write_timestamp sb=0x957404930 dst=0x9570dd180 off=0 split=0
[KKTS] release      sb=0x957404930 cpu_peek=0
```

**`cpu_peek=0`.** The sample is not lost by the resolve — it is *never taken*.

`kk_encoder_write_timestamp` creates the sampling blit encoder, optionally waits a fence, signals
a fence, and ends. It encodes **no data movement**. Metal elides a blit encoder that encodes
nothing, and an elided encoder never samples. `mtlprobe` isolates it exactly:

| shape | ok | zero |
|---|---|---|
| KK shape, **empty** sampling encoder, CPU resolve | 0 | **20** |
| KK shape, sampling encoder with a `fillBuffer`, CPU resolve | **20** | 0 |

Same shape, same CPU resolve, same machine — the only difference is whether the encoder carries
work.

This is the trap `mtlprobe.m` has warned about in a comment since it was written — *"the sampling
encoder MUST carry real work"* — dutifully applied to every probe case and **never checked against
KK's own encoder**. M1 Max does not elide it, which is why it went unnoticed there, and why
forcing the split on M1 Max "validated" a fix for a defect that machine does not have.

Fixed in **`patches/kosmickrisp/0012`**: fill the 8 bytes at `dst_offset` inside the sampling
encoder. Free in effect — the resolve overwrites exactly that slot immediately after.

### There are two bugs, not one

With the fill in place, `cpu_peek` is nonzero for **every** sample — sampling is completely fixed
— and the query report is still 0 much of the time. So the GPU `resolveCounters:` defect is real
too; it was simply masked by the fact that there was never a sample to resolve. Post-fix on M4 Pro:

```
A  vkGetQueryPoolResults -> q0=0 avail=1  q1=999520408044583 avail=1
```

Real timestamps appear where there were none, but partially. M1 Max passes A/B/C cleanly (no
regression).

**Still to do:** replace the GPU resolve with the CPU one. The mechanism is now known-good — the
`cpu_peek` added for diagnosis returns a correct timestamp on every sample, 0 failures observed —
but wiring it into the report moves the write out of GPU command order, so it has to be reconciled
with an in-stream `CmdResetQueryPool` / `CmdCopyQueryPoolResults`. That is its own change.

### Deployment state (2026-07-25 evening)

`0012` is **in the `.app` bundle rebuilt this evening**. It was *not* in the one before it: that
bundle's `libvulkan_kosmickrisp.dylib` was stamped 20:51 and `0012` was committed at 21:12 — which
is the entire reason the compositor session "still couldn't get timing information" after the
earlier deploy. **Check the artifact, not the commit:**

```sh
nm -a <bundle>/Contents/Frameworks/libvulkan_kosmickrisp.dylib | grep -E 'mtl_blit_fill_buffer|cpu_peek'
```

Absent ⇒ pre-`0012`. `build-app.sh` bundles `/Volumes/mesa-cs/build-kk/.../libvulkan_kosmickrisp.dylib`
directly, so an unrebuilt KK ships stale and silent.

**Set expectations at ~82%, not 100%.** With `0012` fixing the sampling and `0011` engaging the
split, what remains is bug #2's 18% per-timestamp resolve failure — so roughly **0.82² ≈ two thirds
of frame pairs** are usable and consumers must discard zero samples. That 82% comes from mtlprobe's
isolated shape, **not** from KK in situ post-`0012`; a 50× in-guest probe loop would replace the
prediction with a measurement, and is the cheapest next step here.

### Method note

Three fixes were proposed for this defect before the driver was instrumented, and all three aimed
at the resolve because the resolve was the only thing anyone had looked at. One `fprintf` of a CPU
read — twenty minutes of work — falsified all of them. Instrument the stack you own *first*.

---

## FIXED (2026-07-26) — the resolve is on the CPU now: `patches/kosmickrisp/0013`

Bug #2 is closed by doing what the correction above said was needed: **replace the GPU
`resolveCounters:` with a CPU `resolveCounterRange:` at command-buffer completion** (0/50 on both
machines, versus 94% failure same-cb and 18% later-cb for the GPU resolve). The reason that was a
design change and not a patch is that it moves the report write out of GPU command order — which is
exactly the objection `0008` raised when it chose the GPU resolve in the first place. That
objection is answered, not sidestepped:

| observer of a timestamp report | how it is ordered against the CPU write |
|---|---|
| the window before the write | sampling encoder fills the report `0xff` = **unavailable**, in GPU command order at the sample point |
| in-stream `vkCmdResetQueryPool` | `encodeWaitForEvent:` on the retired-prefix shared event |
| in-stream `vkCmdCopyQueryPoolResults` — **venus's path, every frame** | same; the sampling command buffer is retired first, since waiting for it from inside itself would deadlock |
| `vkResetQueryPool`, `vkDestroyQueryPool`, `vkGetQueryPoolResults`+`WAIT_BIT` | CPU `waitUntilSignaledValue:` |
| a bare `vkGetQueryPoolResults` poll | publishes already-materialised in-flight samples, non-blocking |

That last row was a real hole worth recording. The completion handler runs *after* the event
`vkQueueWaitIdle` waits on, so an app that synchronises and then polls once loses the race
**every time** — measured 0 successes in 10 runs, not a flake. Publishing in-flight samples on
demand fixes it and cannot invent a value: a counter sample buffer is freshly allocated per
timestamp and resolves to 0 until the GPU takes its sample, so a nonzero read is necessarily real.

### Measured, M1 Max, this build

`probe` gained an **A′** leg (the same read with `WAIT_BIT`) precisely because A is a bare poll and
the spec lets that answer `VK_NOT_READY` — without both legs you cannot tell a lost race from a
lost value.

| case | before `0013` | after `0013` |
|---|---|---|
| A `vkGetQueryPoolResults`, bare poll | **0/10** (`VK_NOT_READY`) | **39/40** |
| A′ same read + `WAIT_BIT` | 10/10 | 40/40 |
| B/C `vkCmdCopyQueryPoolResults` (venus's shape) | pass | pass |
| `timerprobe` real `glQueryCounter` / `GL_TIME_ELAPSED` on zink-on-KK | pass | pass, sane monotonic ns |
| F44 enhanced desktop, EFI+venus windowed, on this KK | — | boots to GDM, **user-eyeballed normal** |

The single A miss is an honest `VK_NOT_READY` — never a zero presented as available, which was the
whole disease.

The desktop row matters because `0013` changes command-buffer *structure*, not just where a value is
read: a timestamp followed by a consumer now retires the sampling command buffer and makes the next
one wait on a CPU-signalled event. Under venus that fires on every submission that carries a
timestamp, so "the probes pass" would not have covered it — the F44 enhanced image booted EFI+venus
on this KK build reaches GDM with no software-2D degradation and renders normally.

### And the probe is gone

`mtl_device_needs_split_counter_resolve` (`0010`/`0011`) is **deleted**, along with the GPU resolve
it existed to schedule. There is now one path on every GPU. That is deliberate: detection whose own
shape passes 4% of the time on affected hardware is worse than a mechanism that is correct
everywhere, and a single path means **the machine we develop on exercises the code we ship** —
which is the assumption whose absence broke `0010`.

### CONFIRMED on the affected hardware — M4 Pro, 100/100

Measured on dogfood-mac (Apple M4 Pro, macOS 26.5.2), against this KK build, **no VM**, via
`./run-remote-m4.sh dogfood-mac 100`:

```
A  real=100  not_ready=0  ZERO-as-available=0  / 100
B  real=100  ZERO-as-available=0  / 100
C  real=100  ZERO-as-available=0  / 100
```

Against the same probe on the same machine before any of this: **A/B/C all `q0=0 avail=1`, every
run.** The disease — a zero presented as an available result — does not occur once in 300 samples.

The 100/100 also settles the earlier estimate. `0012` alone was predicted to leave ~82% per
timestamp (bug #2's 18% resolve failure) ⇒ ~⅔ of frame *pairs* usable, and the guidance written
into `venus-cost.md` was that consumers must discard zeros. **That residual is gone**: the 18% was
the GPU resolve, and there is no GPU resolve any more. Expect real GPU times for every frame.

The traced run shows the design working end to end on the hardware that breaks:

```
[KKTS] write_timestamp seq=1 sb=… off=0
[KKTS] write_timestamp seq=1 sb=… off=8
[KKTS] resolve seq=1 … value=1038806716893666      <- published by the poll
[KKTS] resolve seq=1 … value=1038806716893666      <- again by the completion handler, same value
[KKTS] write_timestamp seq=2 …
[KKTS] barrier seq=2 (signalled=1)                 <- B's copy waits for seq 1's report
```

Both halves are visible: the idempotent double-publish (poll first, handler after, identical value)
and the barrier firing for the `vkCmdCopyQueryPoolResults` cases — which is venus's every-frame path.

**Not yet measured:** `gnome-shell-rs` in a guest on that machine. The host driver is proven; the
guest-visible end of it needs a KK + `.app` rebuild deployed there, which is a separate step.

## What it costs — `tsbench`, `0012` vs `0013`

Moving the resolve to the CPU is not free: a timestamp followed by an in-stream consumer now
retires the sampling command buffer and makes the next one wait, on the GPU, for an event the CPU
signals from the completion handler. That is a GPU→CPU→GPU hop in the frame. `tsbench.c` measures
it against the pre-`0013` driver rather than arguing about it. Median of 3000 submits, 3 runs on
M1 Max and 2 on M4 Pro, all rows stable to the last digit shown:

| shape | M1 Max `0012` | M1 Max `0013` | M4 Pro `0012` | M4 Pro `0013` |
|---|---|---|---|---|
| **none** — no timestamps (control) | 0.263 ms | 0.263 ms | 0.156 ms | 0.158 ms |
| **ts** — 2 timestamps, read after the fence | 0.357 ms | **0.316 ms** | 0.206 ms | **0.199 ms** |
| **tscp** — 2 timestamps + in-stream copy (venus) | 0.419 ms | **0.503 ms** | 0.221 ms | **0.286 ms** |

Per-submit throughput (32 deep, one wait per batch) moves the same way: `ts` 0.139 → 0.103 ms on
M1 Max, `tscp` 0.160 → 0.237 ms.

Three things to read out of that:

- **An app that does not use timestamp queries pays nothing.** The `none` row is identical on both
  machines — that path is not touched, and it is the control that says the rest is signal.
- **Timestamps without an in-stream consumer got *faster*** (−11% M1 Max, −3% M4 Pro): `0013`
  deletes two GPU resolve encoders per frame and adds no barrier, because nothing observes the
  report before the completion handler runs.
- **The barrier costs ~0.08 ms (M1 Max) / ~0.065 ms (M4 Pro) per submission** that carries both a
  timestamp and an in-stream consumer. Under venus that is every frame, since the query-feedback
  command buffer rides in the same submission — but it is **~0.4% of a 16.7 ms frame at 60 Hz**,
  ~0.8% at 120 Hz, and it is per *submission*, not per timestamp (a compositor's two timestamps
  share one batch and one barrier). It is not an FPS-visible cost for a desktop; it would only
  matter to something submitting thousands of timestamp-querying command buffers a second.

Note the M4 Pro `0012` column is a driver that returns **zeros** on that machine, so its numbers
are "fast but wrong" — there, the delta is the price of getting an answer at all.

**If it ever does matter**, the barrier is avoidable for exactly venus's shape: a
`vkCmdCopyQueryPoolResults` from a timestamp-only pool into host-visible memory could be serviced
by the CPU in the same completion handler that resolves the samples, with no GPU kernel and no
event wait. Deliberately not done here — it is a second special case, and 0.4% of a frame does not
buy one.

Run it with `./tsbench.sh [iters]` locally, or `./tsbench-remote.sh dogfood-mac 3000 <baseline.dylib>`
for the A/B on the M4 Pro.

## 2026-08-05 — upstream's MTL4 implementation probed on the M4 Pro: CLEAN, no bug to file

The 2026-08-05 KK rebase (`limina-kk` → mesa main `418f2963a15`) dropped our whole timestamp
arc in favor of upstream's Metal-4 implementation (`ed807097` + `7f540b05`: MTL4CounterHeap,
per-stage `writeTimestampWithGranularity:`, resolve in the SAME MTL4 command buffer that wrote
the timestamps). That same-cmdbuf resolve is exactly the shape our Metal-3 findings said loses
values on M4-class GPUs — so the open action item was: probe it on the affected hardware before
trusting it.

Done (run-remote-m4.sh dogfood-mac 100, rebased dylib git-c72c9aa806, macOS 26.5.2, M4 Pro,
log: m4-probe-mtl4-upstream.log):

- **A (bare poll): real=100, not_ready=0, ZERO-as-available=0.**
- **A' (+WAIT_BIT), B, C (GPU-side copy, the venus shape): 100/100 real, zero disease.**
- Values advance between submissions; `timestampValidBits=64`, period 41.67 ns.

**Verdict: the Metal-3 hazard does NOT carry over to MTL4CounterHeap on the M4 Pro.** Either
the MTL4 counter-heap materialises samples at resolve time by construction, or the granularity
machinery orders it; either way there is nothing to report upstream. The "file a mesa issue if
zeros reproduce" action item is CLOSED — no issue.

One semantics change, noted not judged: on an otherwise-EMPTY command buffer our Metal-3 impl
returned deltas of ~27–32 µs (blit-encoder execution overhead — see the 2026-07 sections
above); upstream MTL4 returns **delta = 0 ns** (both writes snap to the same boundary). For an
empty bracket 0 is arguably the more honest answer, but a consumer that used the empty-bracket
delta as an overhead floor will now read 0. Real-work deltas were not probed here (probe.c
encodes nothing between the writes); the guest compositor's GL timer queries after the next
dogfood deploy are the real-work oracle — if those read 0, revisit.
