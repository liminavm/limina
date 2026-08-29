# A restore blanks a Vulkan compositor's desktop

**Verdict: ours, not synoik's.** The venus device-memory content capture can only read memory
`vkMapMemory` accepts. A Vulkan compositor's framebuffers are device-local and non-host-visible,
so their pixels are never captured, and the restored desktop is blank until something forces a
full repaint.

## What is observed

On `Fedora-Workstation-44.enhanced.synoik.raw`, suspend a seated synoik session and restore it:

- Everything that keeps drawing (vkmark, a WebGL page in firefox) renders correctly.
- **Everything static is black** — wallpaper, idle client windows, desktop icons. The panel
  paints, over a smear of stale wallpaper.
- The clients are alive (same pids), synoik is alive (same pid), `boot_id` is unchanged.
- Host-side every oracle is green: submits advance, `errs=+0`, `unknown_ctx=+0`,
  `unknown_res=+0`, no `ErrRutabaga`, no `ErrInvalidResourceId`. Replay reports success.
- **Opening any new client heals the whole desktop** — the wallpaper, the pre-suspend nautilus
  window and the icons all come back intact. The textures were never lost; the *scanout* was.

`pre2.png` / `post2.png` / `post2b.png` (blank, stable over four minutes) / `post2c.png` (healed
by a new client) are the frames.

## The mechanism

The snapshot log names it directly:

```
gpu snapshot: content read failed for ctx 2 mem 66 (14745600 bytes)
gpu snapshot: content read failed for ctx 2 mem 204 (14745600 bytes)
gpu snapshot: 685 ops, 11 vkr journals, 20 blob contents, 114 memory contents (263 MiB), 7 classic contents (204 MiB)
```

14745600 = 2560 × 1440 × 4 — one full-screen RGBA frame, twice: the compositor's double-buffered
scanout images.

`vkr_device_memory_content_copy` (`src/venus/vkr_device_memory.c`) captures a memory object by
`vkMapMemory` + `memcpy`. That is only legal for host-visible memory; a device-local allocation
returns failure, the bytes are skipped, and nothing downstream treats the gap as fatal —
`limina_memory_read` returning false is a `warn!` in `virtio_gpu.rs` and the snapshot proceeds.

A GL compositor never exposes this: its compositing textures are classic vrend resources, and
`classic_contents` (payload v6) captures those by GL readback, which needs no mappable memory.
The venus path has no equivalent — so the gap is invisible on the GNOME/mutter image and total on
a Vulkan compositor.

The damage-tracking behaviour on top of it is correct, not a second bug: the compositor has no
reason to repaint a region it did not change, so a blank framebuffer stays on screen until
unrelated damage forces a full recomposite.

## The fix this points at

Device-local memory cannot be read back byte-wise through `vkMapMemory`. It has to be copied
through a host-visible staging allocation, per bound resource — `vkCmdCopyImageToBuffer` for
images (layout/format known to vkr), `vkCmdCopyBuffer` for buffers — with the symmetric path on
restore. Until then, a `content read failed` at snapshot should be loud enough to predict the
symptom rather than sit at `warn` next to a success-shaped summary line.

## What the L2 gate measured, and where the model does not yet fit

`l2_synoik_restore_landmarks` reproduces a failure on every run, but a **narrower** one than the
poke session's whole-desktop blank, and the two halves of the evidence do not join up yet.

Seven allocations are skipped at snapshot (1280x800 session): two in the compositor's context at
4128768 bytes — its double-buffered scanout — and five in vkstill's at 1966080 bytes each, the
swapchain images of a client asking for `minImageCount + 1`. Both sets are render targets, which
is what the `vkMapMemory` capture cannot read. That much matches.

The pixels do not. Post-restore the compositor's scanout is *correct*: panel, firefox chrome and
the still WebGL canvas all come back byte-identical (`l2-pre.png` / `l2-post.png`). The single
wrong region is the **idle nautilus window's visible strip, which comes back black** — and
nautilus's buffer is not in the skipped list at all.

**What distinguishes nautilus from firefox here is NOT known.** Both are idle: the test's page
draws one frame and stops, so neither client is redrawing, and "firefox recovered because it
repaints" is an explanation the evidence does not support. Unchanged pixels prove the
compositor still had a good buffer for firefox — nothing about whether firefox could still
render. A wedged client holding its last good buffer is indistinguishable from a healthy one in
a still-workload comparison, by construction. Candidate differences to test, none confirmed:
occlusion (nautilus is partly behind firefox), the buffer path (firefox renders in its own GPU
process), and window size.

So "the compositor's framebuffers are not captured" explains the poke session and does **not**
explain this run. A candidate worth testing before any fix is written: a client buffer reaches
the compositor as an *import*, `vkr_device_memory_capturable` skips imports on the stated
grounds that "capture/restore happens at the source", and for a GL client rendering into a
dmabuf that source is a blob resource which the classic content dump may not cover either — in
which case the alias resolves to nothing and the compositor composites black. That is a
hypothesis, not a finding: it needs the aliased source identified in a live snapshot.

Note also what stayed green on the failing run: **colour diversity 4083 -> 4083**, every process
alive. The content floor and every process oracle passed on a desktop with a black window in it.

## The wedge, reproduced locally: an idle venus client poisons the restore

The dogfood symptom — firefox comes back dead and no repaint revives it — **reproduces on a
local synoik poke VM**, and the trigger is not firefox. It is the presence of a **second venus
context in the session that is not presenting**.

The controlled pair, same image (`Fedora-Workstation-44.enhanced.synoik.raw`), same host, same
2560x1440 display, one suspend/restore each, both first-cycle on a fresh clone:

| session contents at snapshot | uncapturable allocations | after restore |
| --- | --- | --- |
| firefox (GL) on an animating WebGL page | 2 x 14745600 (the compositor's scanouts) | **healthy** — wallpaper, window and animation all continue |
| the same, plus `vkstill` in idle mode | the same 2, plus 5 x 1966080 (vkstill's swapchain) | **wedged** |

Wedged means, precisely: synoik is alive and sleeping but never composites again; the host
receives so few applies that `LIMINA_WINDOW_CAPTURE` never writes a file; firefox's Web Content
process burns **0 CPU ticks in 10 s**, and still 0 after a new client is started to force damage;
and a freshly launched `vkstill` gets far enough to be a running process but never reaches its
own `device Virtio-GPU Venus` line — **no client can initialise Vulkan any more**. Nothing in
the guest logs an error. `dmesg` ends at a clean `PM: suspend exit`.

The one guest-side lead is in synoik's own log, right at the restore: the virtual connector is
**disconnected and re-added** (`disconnecting connector: "Virtual-1"`, then `new connector`),
with `ERROR ... missing surface in vblank callback for crtc crtc::Handle(42)` between them. After
the re-add synoik logs nothing further. Whether the connector churn is the cause or another
symptom is not established.

A second cycle on the same session (idle vkstill again, second suspend) gives the same wedge with
a fuller picture, because that restore did present a frame: `c2_post.png` — the whole desktop
black, only firefox's CSD titlebar and the panel drawn. Starting a live client heals the
**wallpaper and the compositor** (`c2_heal.png`) and leaves the idle vkstill window and firefox's
page **still black**. So the two shapes coexist: compositor content is blank-but-healing, client
content behind a wedged context is gone for good.

## What the dogfood Mac has that the local runs did not

Read off `Dev.liminavm`'s supervisor log and the guest, 2026-08-29:

- The dogfood session is **synoik**, not mutter (`/usr/local/bin/synoik --session`).
- Its firefox is **Nightly 157.0a1 from `/opt/firefox`**, not the image's Fedora RPM — and on
  that machine firefox maps `libvulkan_virtio`, i.e. it holds a venus context. The RPM firefox in
  the local poke maps only GL. That is the single difference that best explains "the user's
  firefox wedges and mine does not": on dogfood firefox *is* one of the venus clients.
- Its snapshots skip **40** allocations, not 2 — spread over ~9 contexts, four identically sized
  buffers each (a Vulkan WSI swapchain per client), every one of them 2048 pixels wide. The local
  poke skips 2 (compositor) or 7 (compositor + vkstill).
- Its restore drops wire entries in classes the local runs never hit: `recording`=41 and
  `noted`=9 alongside the usual `free`. Every local restore drops in `free` alone. Those two
  classes are logged per entry only at `debug`, so a dogfood run reproducing the wedge under
  `RUST_LOG=...krun_devices=debug` would name exactly which state was lost.

## What this changes for the L2 gates

`l2_synoik_restore_landmarks` drives `vkstill` presenting continuously, and a client that
presents every frame repaints itself — which hides the fault this gate exists to catch. The asset
now takes `VKSTILL_IDLE_AFTER=<n>`: draw n frames, then stop submitting and only service the
Wayland connection, leaving the last presented image as the compositor's only copy. That idle
client is what turns a healthy restore into a wedged one, so it is what the gate should run.

## Also seen

A **`SIGSTOP`ped Vulkan client appears to hold out the suspend quiesce.** Freezing vkmark to
get a still venus-drawn surface for the L2 landmark test left the guest unable to suspend at
all — the worker never reached exit 126 inside 120 s, where the same run without the freeze
suspended in seconds. One A/B, not a controlled one: worth an explicit repro before it is
written down as a fact, but it is the obvious suspect for any "this guest will not suspend"
report where a GPU client is stopped or stuck mid-frame.


`limina suspend <disk>` leaves the **supervisor process alive** after the worker snapshots and
exits — reproduced on all three cycles here. It keeps its gvproxy (and its SSH port), and the
next `limina suspend` on that disk refuses with "multiple limina supervisors match".
