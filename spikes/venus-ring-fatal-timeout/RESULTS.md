# A ring wait slower than the diagnostic threshold poisoned the venus context

**2026-08-01. Root-caused, fixed (virglrenderer `vkr_context.c`), A/B-verified.**

## Symptom

`gnome-control-center` died with `SIGABRT` on the F44 enhanced image, from a GTK *timer*:

```
#7  g_malloc ()                            ← ../glib/gmem.c:106: failed to allocate 281474368445232 bytes
#8  gdk_vulkan_save_pipeline_cache.isra ()   libgtk-4.so.1
#9  gdk_vulkan_save_pipeline_cache_cb ()
#10 g_timeout_dispatch ()
```

Intermittent — the same panel opened fine on a second try. The guest also logged
`MESA: error: ZINK: vkGetPipelineCacheData failed (VK_ERROR_OUT_OF_HOST_MEMORY)` three times in the
seconds before, so it was not one app misusing one call.

## The chain, read backwards from the host log

```
21:07:50.946 ERROR virgl: vkr: context 7: ring FATAL set at vkr_dispatch_vkWaitRingSeqnoMESA:399
21:07:50.952 WARN  Some(ResourceCreateBlob) -> ErrRutabaga(ComponentError(-1))
21:07:50.952 WARN  Some(ResourceMapBlob)    -> ErrInvalidResourceId
```

1. The context is marked FATAL in the ring-seqno wait.
2. Every later blob create is refused.
3. Guest venus needs a blob for its **reply shmem**: `vn_renderer_shmem_pool_grow_locked` →
   `vn_renderer_shmem_create` fails → `vn_ring_get_command_reply` returns NULL → the generated
   `vn_call_vkGetPipelineCacheData` returns **`VK_ERROR_OUT_OF_HOST_MEMORY`**
   (`vn_protocol_driver_pipeline_cache.h:477` — the OOM is "no reply buffer", not a Vulkan OOM).
4. GTK4 asks for the cache size, ignores the failure, and `g_malloc`s the uninitialised size.

So the pipeline cache was a **casualty**, not the cause — and neither KosmicKrisp nor GTK is where
the bug lives.

## Root cause: a timeout mistaken for an error

`vkr_context_wait_ring_seqno` (ours, added by the M9.3 sync fast-forward patch) logs a diagnostic
when its `cnd_timedwait` times out and treats every other non-success return as failure:

```c
if (ret == thrd_timeout) {        /* dead branch */
   vkr_log("wait_ring_seqno STUCK >500ms ...");
} else if (ret != thrd_success) {
   ok = false;                    /* → vkr_context_set_fatal(ctx) */
}
```

Mesa's c11 shim maps `ETIMEDOUT` to **`thrd_busy`**, not `thrd_timeout`
(`src/mesa/compat/c11/threads_posix.h:147`; `thrd_timeout = 1`, `thrd_busy = 3`). So the STUCK
branch could never run, and **any ring wait exceeding 500 ms poisoned the context** — the
diagnostic meant to report a stall was creating a fatal one instead. Intermittent because it needs
a wait to cross the threshold.

`probe.c` pins the platform fact and prints the branch the real code would take:

```
cnd_timedwait on timeout returned 3 (thrd_busy)
  thrd_timeout = 1, thrd_busy = 3
  -> logs STUCK: NO (dead branch)
  -> ok=0  =>  vkr_context_set_fatal(ctx) — CONTEXT POISONED BY A 500ms STALL
```

## Fix + A/B

Test `thrd_busy` (and `thrd_timeout`, so the code stays right on a conforming C11 runtime); a
timeout logs once and **keeps waiting**. Only a genuine condvar error fails the wait. The
diagnostic moved from `vkr_log` (INFO) to `vkr_log_error`, since at INFO it was invisible at the
worker's default `warn` — the second reason this hid for so long.

`LIMINA_RING_WAIT_WARN_MS` (default 500) makes the slow path reachable on demand, which is what
turned this from "wait for it to happen again" into a deterministic A/B. Same disk, same env,
threshold 1 ms:

| arm | ring FATAL | STUCK diagnostics |
|---|---|---|
| before | **1** (within 90 s of boot) | 0 — dead branch |
| after  | **0** | **3**, and every wait completed |

A GREEN line shows how benign the poisoned waits were:

```
wait_ring_seqno STUCK >1ms ctx 3 ring …: want=330108 head=329216 tail=330108 status=0x0
```

`head` trailing `want` by ~900 with `status=0x0` — a healthy ring, momentarily behind.

## Reproducing

```sh
# RED needs the old comparison; GREEN is the shipped code.
LIMINA_RING_WAIT_WARN_MS=1 LIMINA_DISK=<enhanced.raw> \
  bash spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
grep -c "ring FATAL" /tmp/enhanced-efi-kk-worker.log   # 0 expected
grep -c "STUCK"      /tmp/enhanced-efi-kk-worker.log   # >0 expected
```

```sh
cc -o probe probe.c && ./probe    # exits 1 on the buggy classification, 0 when fixed
```

## Lessons

- **A diagnostic can be the defect.** This code was added to *observe* a wedge; it manufactured
  one. Anything that classifies a return code needs a test that the classification is reachable.
- **Check what the shim returns, not what the standard says.** `thrd_timeout` is the C11 spelling;
  the vendored POSIX shim returns `thrd_busy`. Reading the header beat assuming the enum.
- **Read the host log at the moment of the guest symptom.** The guest-side story ("Vulkan is out of
  memory") pointed at KK and the pipeline cache. One host log line named the real site.
