# Vulkan-compositor backdrop-texture leak — investigation, 2026-08-06/07

> ## Correction, 2026-08-07 — read this first
>
> **The churned object is the 4-slot SCANOUT SWAPCHAIN buffer, not the wallpaper texture.**
> The compositor-side investigation
> (the compositor guest's `synoik/vmm-memory-exhaustion-scanout-churn.md`) counted the
> allocations directly: `SYNOIK_VK_FULL_DAMAGE=1` ran `reset_buffer_ages()` every frame, and
> smithay *replaces* a swapchain slot whenever `Arc::get_mut` fails — which is always, with a
> frame in flight. That is 4 fresh 3840×2160×4 buffers per frame, ~118/s ≈ 3.9 GB/s, ~153 GB
> minted in the crash session.
>
> Everything below about *method* stands, and the A/B was real — but it identified the wrong
> object. 4K RGBA is 31.6 MiB whether it is a wallpaper or a scanout buffer, and the
> wallpaper was modulating **redraw activity**, not being the thing leaked. A counted export
> log beats an inference from region sizes; §5's "the wallpaper is the amplifier" should read
> "redraw activity is the amplifier, and the wallpaper drives it".
>
> **Still open, and now clearly on our side:** killing the compositor reclaimed nothing
> host-side. A *healthy* synoik SIGKILL releases fully (126.9 MiB → 4 KiB, measured
> 2026-08-07), so it is the storm state — not abrupt exit as such — that retains. The
> instrument for settling it is the GPU-memory census: `docs/design/gpu-memory-budget.md`,
> next steps in `spikes/venus-churn-retention/RESULTS.md`.

**Verdict (as originally written):** under a Vulkan compositor, the host worker accumulates one
**31.6 MiB** allocation per re-created 4K texture and never releases it. At 4K that is
~51 GB/hour, and macOS **jetsam** SIGKILLs the worker once it becomes the largest compressed
process. A controlled A/B proved the wallpaper is the trigger; the underlying defect is that the
texture is **re-allocated instead of reused**, and resolution only sets the price.

**Status:** root cause isolated, ownership (guest compositor vs. our host release path) **not yet
assigned** — see *Open question* below. Mitigation available today: a ≤1440p wallpaper, or none.

---

## 1. What actually happened

The dogfood VM "crashed" while scrubbing video. It did not crash — the worker was **jetsam'd**:

```
kernel: memorystatus: killing largest compressed process limina-vmm [PID] 142871 MB
```

The supervisor only logs `VM stopped — worker terminated by signal 9`, and **no `.ips` is
produced** (a jetsam SIGKILL is not a crash). Before treating any signal-9 worker death as a
crash, grep the unified log for `jetsam` / `memorystatus`.

A ring-FATAL fired 38 s earlier (`vkCreateImage failed host-side: -1000158000` =
`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`, context `[totem]`). That killed one
guest client's venus context; it did **not** kill the VM. It is a separate, real bug.

## 2. The signature — and why RSS lies

Live worker, 5 min vs ~35 min uptime:

| metric | t+5 min | t+35 min |
|---|---|---|
| Physical footprint | 9.4 G | **43.6 G** |
| vm region count | 5,728 | **35,283** |
| **owned unmapped** | **89.4 M / 98 regions** | **23.5 G / 923 regions** |
| TOTAL swapped | 706 M | **31.9 G** |
| fds | ~400 | ~400 (flat) |
| **RSS** | 15.1 G | **5.2 G — falling** |

`owned unmapped` is *"owned physical footprint (unmapped)"*: memory charged to the task but not
mapped into its address space (Mach memory entries / IOSurface-class GPU resources held by
reference). It is nearly all **swapped**, which is exactly why jetsam sees a giant *compressed*
process.

> **RSS is worse than useless here — it moves the wrong way.** It fell 15.1 G → 5.2 G across the
> window in which footprint climbed to ~40 G. Activity Monitor's "Memory" column would show the
> worker *shrinking* right up until it is killed. Use `vmmap -summary`'s `owned unmapped` row.

**It is a ratchet, not a drip.** A 7-minute idle window held flat (35,298 → 35,188 regions — a
net release of ~110, i.e. nothing) while the preceding ~24 minutes of use took it 5,728 → 35,298.
Corollary: **an idle-vs-idle A/B proves nothing.** Drive the workload and diff across it.

## 3. The arithmetic that cracked it

Size histogram of the leaked regions at the final snapshot:

```
 767  [ 31.6M      <- 767 x 31.6M ~ 24.2 GB, all swapped
  64  [ 128K
   4  [ 16K
```

**767 identical sizes** = one repeated call site, not diffuse growth. And 31.6 MiB is not
arbitrary:

```
3840 x 2160 x 4 = 33,177,600 B = 31.64 MiB
```

A 4K RGBA8 texture, matched to three significant figures. (2560×1440 would be 14.1 MiB;
5120×2880 would be 59 MiB. Only 4K fits.) The wallpaper in use was a 3840×2160 JPEG, set at
20:28:59; the first host `invalid res_id` errors appeared at **20:35:44**, and the VM died at
22:51.

## 4. The controlled A/B (live dogfood VM, same activity both phases)

Counting regions of exactly 31.6M:

| phase | background | 31.6M regions | notes |
|---|---|---|---|
| **A** | the 4K JPEG | **0 → 10** in ~2.5 min | `owned unmapped` 17.1M → 280.9M |
| **B** | solid colour (`picture-uri ''`) | **9 → 9** — flat | more activity than A; `owned unmapped` *fell* 322M → 214.6M |

Phase A's zero is **airtight, not assumed**: total `owned unmapped` at A's baseline was 17.1M —
smaller than one 31.6M region — so none could have existed.

## 5. The wallpaper is the amplifier, not the defect

Something re-allocates the backdrop texture rather than reusing it. Resolution sets the price per
occurrence, at the observed ~27 leaks/min under real use:

| wallpaper | per leak | rate |
|---|---|---|
| 1920×1080 | 7.9 MiB | ~13 GB/hr |
| 2560×1440 | 14.1 MiB | ~23 GB/hr |
| **3840×2160** | **31.6 MiB** | **~51 GB/hr** — kills a 48 GB host in ~3 h |

At 1080p the same bug reads as ordinary memory growth over a workday. Going 4K is what made it
lethal on the same day.

## 6. Open question — guest or host?

`vmmap` cannot distinguish:

- **compositor side** — the old backdrop texture is never destroyed; or
- **host side** — it *is* destroyed and we fail to release the backing IOSurface. This is exactly
  the class of virgl `0015` *"release the backing IOSurface on device/context teardown, not just
  vkDestroyImage"*, and the IOSurfaceRef leak fixlet inside `0031`.

**Discriminator (now built in).** Since 2026-08-07 the renderer keeps a ledger of host memory
allocated on the guest's behalf, per venus context, with an exact-size histogram
(`docs/design/gpu-memory-budget.md`; virglrenderer `24a3c2d9`). It answers this question
directly, no `vmmap` correlation needed:

- the ledger **climbs** in step with `owned unmapped` → the guest is holding the memory;
  the compositor never frees the old texture.
- the ledger stays **flat** while `owned unmapped` climbs → the guest freed it and *we* did
  not release the backing IOSurface. That is the virgl `0015` class.

It also prints `ctx N [name] destroyed with X still charged` when a context tears down with
charges outstanding — a direct statement from the release path, rather than an inference off
a footprint curve. Run the same A/B with the worker log open.

Note the fix belongs in the reuse path either way — even with correct release, re-allocating a
4K texture repeatedly is the thing to stop.

**And the crash is now bounded.** Whatever the ownership verdict, a guest that runs away with
host memory hits the cap and loses *that one context*, with the culprit and its size histogram
named in the log — instead of the host OS killing the VM three hours later. Note what
"bounded" can and cannot mean here: venus allocates asynchronously and discards the host's
result, so the compositor cannot *catch* an OOM and back off; it will be killed. The worker
log is where the diagnosis lives.

## 7. What this is NOT (retracted leads — do not re-run them)

- **The `EXC_GUARD` storm is not the leak.** 73,782 events (`type=0x5` `GUARD_TYPE_VIRT_MEMORY`,
  `flavor=0x1` `kGUARD_EXC_DEALLOC_GAP` — a `vm_deallocate` over an unallocated gap) in one 3 h
  session looked damning. Measured: ~2 guards per venus context create/destroy and ~1 per
  cross-context import, but **2000 import cycles moved the region count by exactly 0**. The
  guards are a *marker of venus traffic*, which is why a Vulkan compositor emits ~410/min and a
  GNOME session ~none. Still a real, smaller bug in our unmap bookkeeping — worth fixing
  separately. Textbook "a scary warning is a LEAD, not a cause".
- **Not resize, blur, or animation on their own.** On a local rig with the same compositor, the
  same 4K wallpaper, and the same zink config: static blur, continuous 60 fps animation, and
  genuine interactive drag-resize all produced **zero** growth. Only one 31.6M region existed —
  the wallpaper texture, allocated once, correctly.
- **Not the JPEG decoder.** The size is a plain RGBA texture; nothing about decode is implicated.

## 8. Reproduction rig

A local repro exists and does not need the dogfood machine. synoik is installed as the
gnome-shell replacement exactly as the dogfood box does it — a user drop-in that swaps
`ExecStart`:

```ini
# ~/.config/systemd/user/org.gnome.Shell@{user,wayland}.service.d/override.conf
[Unit]
OnSuccess=gnome-session-shutdown.target
OnSuccessJobMode=replace-irreversibly

[Service]
Environment=GSETTINGS_SCHEMA_DIR=/usr/local/share/synoik/glib-2.0/schemas
ExecStart=
ExecStart=/usr/local/bin/synoik --session
```

plus GDM autologin (nobody is at the keyboard). Going through GDM is what gives the compositor a
real logind seat — three direct-launch attempts failed on exactly that (`Failed to open session:
Invalid argument`, then `openvt` console conflicts).

`sample-leak.sh` (this directory) is the oracle. It supersedes the older
`sample-worker.sh`, whose `phys_footprint` column parsed `awk -F=` (vmmap prints `:` on macOS 26)
and whose `mach_ports` column needs privileges.

## 9. Traps this investigation actually hit

- **The Bash tool's shell is fish, and `log show --predicate '…'` silently matches NOTHING there.**
  It returned 0 events and produced a confident, wrong "this does not reproduce locally". Always
  run `log show --predicate` through `bash -c`, or stage a script file.
- **`set-window-width <N>` does not take a bare pixel count.** A churn loop drove the window to
  **55360 px**; the client died (`Dimension X value 55360 exceeds the limit of 8192`) within
  seconds and every sample read flat because the workload was gone. The loop sent stderr to
  `/dev/null`, which hid it. **Never suppress stderr in a driver loop**, and verify the workload
  is still alive at the end of a run.
- **Setting `picture-uri` alone is not enough** — leave `picture-uri-dark` populated and the
  light/dark preference silently keeps the 4K texture alive, yielding a false null.
- **Restarting GDM does not re-read `/etc/environment.d`** — the systemd *user manager* survives.
  A zink-vs-virtio_gpu A/B silently ran twice on the unchanged stack until
  `/proc/PID/environ` was checked. Put the env on the unit, and verify at the process.
- The human was the oracle twice: "I don't see ghost" and "ghost appeared and disappeared" both
  caught dead workloads that the host-side numbers reported as clean flat lines.

## 10. Incidental findings (compositor side)

- synoik hands a client a window size past `maxImageDimension2D` without clamping.
- wgpu then hits a secondary panic during teardown (`Trying to destroy a
  SwapchainAcquireSemaphore that is still in use by a SurfaceTexture`), turning a recoverable
  validation error into an abort.
- Reported flicker — "the background bleeds through everything for a few frames" — is a frame
  showing backdrop but no window content. Plausibly the same root cause seen from the front: a
  *freshly allocated* backdrop presented before it is fully written. Untested, but if the reuse
  fix lands, check whether the flicker goes with it.
