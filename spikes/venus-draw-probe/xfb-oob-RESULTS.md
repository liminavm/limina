# KK transform-feedback OOB write — RED/GREEN reproducer

Security triage finding (docs/upstreaming/00-obvious-fixes-and-security.md, §2B item 1).

## The bug
KK's XFB command handlers index the fixed 4-element `gfx->xfb.buf[4]`
(`kk_cmd_buffer.h`, 40-byte entries) with a **guest-controlled** base and no clamp:
- `kk_CmdBindTransformFeedbackBuffersEXT`: `idx = firstBinding + i`
- `kk_CmdBeginTransformFeedbackEXT` / `kk_CmdEndTransformFeedbackEXT`: `idx = firstCounterBuffer + i`

venus is untrusted from KK's side; a non-conformant guest passing `firstBinding > 3`
makes KK write a **guest-controlled value** (`gpu_base` = a buffer GPU address, `size`)
to a **guest-controlled offset** (`base + firstBinding*40`). Controlled-address +
controlled-value = a strong host memory-corruption primitive. Upstream KK has no XFB
(limina-authored), so no coordinated-disclosure embargo — but fix before ship/upstream.

## Reproducer: xfb-oob-probe.c
Calls KK **directly** via its Mesa ICD (`vk_icdGetInstanceProcAddr`) — NOT the system
Vulkan loader. Two traps discovered while building this (both instances of "proxies lie —
verify the code path is exercised"):
1. The system Vulkan loader does **not** dispatch these extension commands to KK in a
   standalone host process. An earlier loader-based probe was a silent no-op; every
   "SURVIVED" was meaningless. `dladdr` on the resolved pointer showed it was
   `vk_cmd_enqueue_CmdBindTransformFeedbackBuffersEXT`, not KK's function.
2. KK records via **vk_cmd_queue**: the real `kk_Cmd*TransformFeedback*` handlers (and
   the OOB) run at **replay time, on vkQueueSubmit**, not at record time. The probe must
   submit the command buffer. (Instrumenting KK with an fprintf is what revealed this —
   at record time the handler never fired.)

Also: a non-ASan **-O2** build did NOT crash even at a 42 GB offset (the OOB store past a
fixed 4-array is UB the optimizer exploits/elides). Use a **-O0** build so the store
executes, or ASan. Non-ASan SIGSEGV is only a sound oracle at -O0.

## Result (Apple M1 Max)
Build KK at `-Dbuildtype=debug` (-O0). `sizeof_entry=40` confirmed at runtime.

    # RED  (pre-fix dylib):
    KK_DYLIB=.../build-kk-prefix/.../libvulkan_kosmickrisp.dylib ./xfb-oob-probe 0x2000000
      -> Bus error (rc=138); trace stops at "writing gpu_base" to base+1.34GB, never
         reaches "wrote gpu_base OK".
    # GREEN (fixed dylib, same input):
    KK_DYLIB=.../build-kk/.../libvulkan_kosmickrisp.dylib ./xfb-oob-probe 0x2000000
      -> SURVIVED (rc=0); the `idx >= ARRAY_SIZE(gfx->xfb.buf)` guard breaks before the write.

Same probe, same input; the only difference is the 3-line clamp in kk_cmd_draw.c.

## The fix
`patches/kosmickrisp/0007-*` (or folded into 0001's XFB code): in all three handlers,
after `idx = first* + i`, `if (idx >= ARRAY_SIZE(gfx->xfb.buf)) break;`. idx is
monotonic in i, so break (not continue) is correct.

## Build
- probe: `build-xfb-oob-probe.sh` (needs only vulkan-headers; dlopens KK directly).
- pre-fix KK: `git worktree add --detach /Volumes/mesa-cs/mesa-prefix HEAD` (fix is
  uncommitted), `meson setup ... -Dbuildtype=debug`, `ninja`. Remove the worktree after.
