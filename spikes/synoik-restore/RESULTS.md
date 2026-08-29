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

## The wedge probe: firefox CAN render after a restore here

Measured 2026-08-29 on a synoik poke session (`wedge-pre.png` / `wedge-post.png` /
`wedge-nav.png`). After a restore, firefox was told to open a new tab on a second page; it
rendered it correctly — solid fill, correct glyphs. So on this stack a restored firefox is not
wedged, and "unchanged pixels" in the L2 gate was not hiding a dead client on that run.

Three limits on how far that goes. The dogfood report is about firefox **nightly**; this is the
image's stock firefox. **That restore came back clean** — nautilus was not black — so what was
tested is "can a client render after a healthy restore", not after one that lost content. And it
is a single trial of a symptom the user describes as *always* occurring on their machine, which
makes the version difference the first thing to vary.

Also learned from the same session, and it changes how any of these runs should be read: **the
blank-window failure did not reproduce on this cycle at all.** A "Critical Updates" notification
appeared during the restore, and its damage forced a repaint that healed the desktop before the
capture. Whether a given cycle shows the fault therefore depends on whether anything happened to
damage the screen first — so a clean cycle is not evidence of a fix, and any A/B here needs the
capture taken before incidental damage, not merely after the restore.

## Three failure shapes, and only one of them is explained

The mechanism above accounts for **content that comes back blank and heals on repaint**. It does
not account for everything seen, and the shapes must not be collapsed:

- **Blank-but-healing.** Surfaces come back empty (transparent over whatever is behind; black
  when the desktop root is behind them) and any repaint restores them. This is the missing
  content capture above.
- **Wedged.** On the dogfood Mac, firefox nightly comes back with rendering permanently broken
  and **resizing its window does not fix it** — reported by the user, not reproduced here. A
  resize forces a full redraw, so a symptom that survives one is not lost content: the client's
  own GPU state did not survive. Separate fault, separate cause, unexplained.
- **Whole-desktop blank until unrelated damage.** The poke-VM cycle above: an idle restored
  session that painted nothing at all (`submits=+0` for minutes) until a new client arrived. The
  user has not seen this on the dogfood Mac, where something is always repainting.

A fix for the first shape must not be read as a fix for the other two.

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
