# Crash forensics: gnome-shell / vkmark SIGSEGVs (2026-06-11)

Question: are the unexplained guest crashes (gnome-shell PID 774 + vkmark PID 2467,
both ~12:28:39 on 2026-06-11; plus one GNOME session death under the glmark2 tight
loop) in our graphics stack (zink/venus, patched mesa) or unrelated noise?

**Verdict: (a) for the crash class — it IS in our stack, but in the venus ICD's
*teardown* path, not the render path.** It is an **upstream mesa venus bug**
(dangling TSD destructor after the Vulkan loader dlcloses `libvulkan_virtio.so`),
not introduced by our patches and not a rendering-correctness problem. The specific
12:28:39 cores were destroyed with the re-cloned disk **(c)**, but two surviving
sibling cores from Jun 10 were fully symbolized and root-caused, and the 12:28 pair
matches the recurring pattern (crash fires exactly at session teardown).

## What evidence survived

The running guest booted 2026-06-11 17:16 from a fresh clone of the golden image, so:

- **Gone:** all state from the Jun 11 morning clone — the 12:28:39 cores (gnome-shell
  774, vkmark 2467) and that boot's journal (`journalctl --list-boots` jumps from
  Jun 10 20:54 straight to Jun 11 17:16). Confirmed: `coredumpctl list --since
  2026-06-10` shows nothing after Jun 10 11:09.
- **Survived (baked into the golden image):** three present cores, all gnome-shell:

  | Time (Jun 10) | PID | Signal | Boot context |
  |---|---|---|---|
  | 09:07:35 | 739 | SIGBUS | mutter `.so`s being **cp'd over the loaded files** at 09:07:34 |
  | 10:13:47 | 755 | SIGSEGV | `systemctl restart gdm` issued at 10:13:46 |
  | 11:09:33 | 757 | SIGSEGV | mutter install + `systemctl restart gdm` at 11:09:33 |

- Current boot: healthy. flickloop (`glmark2-es2-wayland --run-forever`) running
  continuously since 17:18 with no crash; no coredumps; no OOM kills in any
  surviving journal.

## Symbolized backtrace (cores 755 and 757 — identical signature)

```
Program terminated with signal SIGSEGV, Segmentation fault.
[Current thread is 1 (LWP 806)]            # 757: LWP 807 — a worker thread, not main
#0  0x00007fff87659aa0 in ?? ()            # PC == si_addr, SEGV_MAPERR: EXECUTING unmapped memory
#1  0x00007fffaa8dc630 __nptl_deallocate_tsd () at /lib64/libc.so.6
#2  0x00007fffaa8df538 start_thread () at /lib64/libc.so.6
#3  0x00007fffaa94abdc thread_start ()
```

A thread, while **exiting**, ran its pthread TSD destructors and jumped to a
destructor whose code had been **unmapped**.

## Root-cause chain (core 755, each link verified empirically)

1. `$_siginfo`: si_code=1 (SEGV_MAPERR), **si_addr == PC** = `0x7fff87659aa0` —
   the fault is the destructor call itself.
2. Raw dump of glibc's `__pthread_keys` from the core: **key #15** has
   `destr = 0x7fff87659aa0` — exactly the fault PC. Neighboring keys symbolize to
   libGLdispatch, libEGL, glib, cogl-trace, libxml2, etc. (all still mapped).
3. The core's NT_FILE mappings show the PC in a large unmapped gap
   (`0x7fff84bdc000–0x7fff8c008000`), directly below zink's
   `/opt/mesa-zink/lib64/libgallium-26.2.0-devel.so`.
4. Diff of the core's mapped-library set vs the live (healthy) gnome-shell: the
   crashed process is missing `/usr/lib64/libvulkan_virtio.so` (the **venus ICD**,
   our mesa bake), `libVkLayer_MESA_device_select.so`, and `dri_gbm.so` — i.e. the
   Vulkan loader had already **dlclose'd the ICD** (vkDestroyInstance during
   zink/EGL teardown).
5. Of the unloaded libs, **only `libvulkan_virtio.so` imports
   `pthread_key_create`** (via mesa's c11 `tss_create` wrapper).
6. In mesa source, the only destructor-registering `tss_create` compiled into the
   ICD is venus's: `vn_tls_key_create_once()` →
   `tss_create(&vn_tls_key, vn_tls_free)` at
   `src/virtio/vulkan/vn_common.c:385` (destructor `vn_tls_free`,
   `vn_common.c:368`). **There is no `tss_delete` anywhere in venus** — the key
   (and its destructor pointer into the ICD's text) outlives the dlclose.

So: any thread that ever touched venus (mutter's KMS thread calls
`vn_GetMemoryFdKHR` every frame per the VN_DEBUG journal lines; zink's
`zfq/zcq/zcfq/disk$` queue threads submit venus commands) holds a `vn_tls` value.
When gnome-shell tears down (logout / `systemctl restart gdm`), zink destroys its
VkInstance, the loader unloads the ICD, and the **last venus-touching thread to
exit jumps through the dangling `vn_tls_free` pointer → SIGSEGV**. It is a
shutdown-ordering race, which is why it's intermittent (3 cores out of ~12 gdm
restarts on Jun 10).

This code is **unmodified upstream mesa** (our local mesa diff touches `vn_wsi.c`
etc., not `vn_common.c`) — an upstream venus lifecycle bug, exposed by normal
ICD unloading.

## The other crashes

- **PID 739 SIGBUS (09:07:35): fully explained, not a stack bug.** The journal
  shows `cp .../libmutter-cogl-17.so.0.0.0 /usr/lib64/mutter-17/` at 09:07:34 —
  overwriting the *loaded* file in place truncates/replaces mapped pages → SIGBUS
  with a garbage stack one second later. This is the install-procedure hazard that
  `install-mutter-fix.sh` (install to temp + `mv`) was created to prevent.
- **Jun 11 12:28:39 — gnome-shell 774 + vkmark 2467 (evidence destroyed).** Both
  dying in the same second is the signature of a session-teardown event, not a
  render-path fault: gnome-shell exits (and intermittently hits the vn_tls crash
  above — PID 774 is a boot-time shell PID, same as 755/757), and vkmark either
  dies on compositor disconnect or hits the same ICD-unload TSD bug on its own
  exit. Consistent with the demonstrated recurring class; not provable, because the
  clone's disk was rm'd.
- **GNOME session death under the glmark2 tight loop (evidence destroyed).** That
  boot's journal died with the clone. No OOM kills anywhere in surviving journals.
  Current boot has the same flickloop running 25+ min crash-free.

## Journal context / current-boot leads

- Recurring `[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200 (command
  0x202/0x203)` (CTX_ATTACH/DETACH_RESOURCE → ERR_UNSPEC): present in **every**
  surviving boot (11–69 per boot, including healthy ones) — chronic noise, not
  crash-correlated, but worth an eventual look at the host-side handler.
- The mutter version-mismatch gdb warnings on the cores are expected (libs were
  re-baked Jun 10 20:54, after the crashes); libc/loader frames symbolized cleanly
  and the analysis does not depend on mutter symbols.

## Repro / verification plan for a DEDICATED session (not the running one)

1. Boot a scratch clone; log in to the desktop; confirm venus is active.
2. Run `vkmark` (or anything venus) once; then `sudo systemctl restart gdm` in a
   loop (~10 iterations). Expect intermittent gnome-shell SIGSEGV cores with
   `__nptl_deallocate_tsd` frame #1 and PC in unmapped memory.
3. Mitigation A/B: set `VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1` in the
   shell's environment (loader keeps ICDs mapped) — crashes should vanish.
4. Proper fix to carry/upstream: have venus pin itself (`dlopen` self with
   `RTLD_NODELETE` on first `vn_tls_get`, the pattern other drivers use) or
   `tss_delete` + drain on last-instance destroy. File/check upstream mesa issue.

## Bottom line

The gnome-shell SIGSEGVs are **crashes in our graphics stack's teardown path**:
upstream mesa venus `vn_tls_free` (`src/virtio/vulkan/vn_common.c:368`, key
registered at `:385`) called after the Vulkan loader unloaded
`libvulkan_virtio.so`. They occur **only at session exit**, never mid-render, and
say nothing bad about rendering correctness or the tier-2 seated-desktop state.
The SIGBUS was self-inflicted (cp-over-mapped-lib). The glmark2-loop session death
remains unexplained with evidence destroyed; the current boot is running the same
loop crash-free.
