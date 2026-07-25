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
