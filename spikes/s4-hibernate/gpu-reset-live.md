# M9.0 spike #3 — does the GPU re-init survive a resource teardown on a *live* venus desktop?

Date: 2026-06-28. Vehicle: `Fedora-Workstation-43.enhanced.raw` clone (`s4-venus-spike.raw`), booted
windowed via `spikes/venus-draw-probe/boot-seated-kk.sh` (16k kernel `6.12.0-dirty`, coexist venus on
zink-on-KK), `--net --ssh-port 2222`. Driven over SSH; host renderer log `/tmp/seated-kk-worker.log`.

## Question

Strategy A (the chosen M9 GPU plan) restores by presenting a **freshly-initialized** virtio-gpu and
letting the guest re-create its GPU resources. Before building snapshot plumbing, prove the guest's
virtio-gpu/DRM + Mesa(venus) + GNOME stack actually **rebuilds a working desktop after the host renderer
loses all its resources — on a running VM, no reboot.**

## What we could and couldn't trigger cheaply

`virtio_gpu` is **built-in** (not a loadable module) and `/dev/dri/card0` is held by fbcon (`vtcon1`) +
`systemd-logind` + `gnome-shell` — so there is **no live "reset the device" knob.** The most faithful
*achievable* trigger is a real **driver unbind/rebind**, which drives a genuine virtio-gpu device reset
and a fresh device re-create. **NB (proven below): the device reset does NOT make the host destroy its
contexts/resources** — libkrun keeps the renderer alive across a reset by design, so whether re-init is
clean depends on the guest having gracefully torn its contexts down first.

## Procedure (all over SSH, VM never rebooted)

1. `systemctl stop gdm` → gnome-shell exits, releases `card0`.
2. `echo 0 > /sys/class/vtconsole/vtcon1/bind` → unbind fbcon, free the scanout.
3. `echo virtio3 > /sys/bus/virtio/drivers/virtio_gpu/unbind` → **host renderer resource reset.**
4. (observe host) 5. `echo virtio3 > .../bind` → device re-created fresh. 6. rebind fbcon.
7. `systemctl start gdm` → autologin → seated desktop rebuilds.

## Round 1 — clean reset (stop gdm first): cold-rebuild works (pixel-verified)

| Observable | Outcome |
|---|---|
| **Host worker survives the live device reset** | ✅ ALIVE throughout; log shows clean `CTX_DESTROY ctx=2`, `CTX_DESTROY ctx=3` (venus/GL contexts torn down in response), then continued. This is the renderer-reset-survival property (cf. libkrun 0022 / `venus_reset`) confirmed **live + seated**, not just at the EFI→kernel boot boundary. |
| **Guest re-inits the device on a running VM** | ✅ `[drm] Initialized virtio_gpu 0.1.0 for a006000.virtio_mmio on minor 0` + `fb0: virtio_gpudrmfb` after rebind. |
| **venus re-enumerates on the fresh device** | ✅ `deviceName = Virtio-GPU Venus (Apple M1 Max)`, `driverName = venus`. |
| **Compositor rebuilds the seated desktop** | ✅ gnome-shell back up **2 s** after `gdm` start (autologin). |
| **Pixel-verify the rebuilt desktop** | ✅ human oracle (2026-06-28): the rebuilt GNOME desktop **renders correctly** in the window (no black/corrupt/frozen frame). |

**Wrinkle (M9.3 hardening note):** one transient `[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response
0x1200 (command 0x200)` (≈ `RESP_ERR_UNSPEC` to a `CTX_CREATE`) fired once during re-init; recovery
proceeded regardless (venus enumerated, GNOME came up).

## ⚠️ Framing: rounds 1–2 crash-and-rebuild (they do NOT test survival)

**`systemctl stop gdm` is how rounds 1–2 free `card0` for the unbind — and it kills the entire session
(compositor + all GPU apps).** So in the window you *see the desktop die and reappear*: those runs are
**crash → fresh autologin desktop**. The "renders correctly" pixel-verify was the **rebuilt** desktop, not
a survivor. (The user caught this watching round 2: *"it looks like the desktop crashed and came back."*
Correct — round 1 did the same, just invisibly, since no apps were lost.) Round 3 below drops the
stop-gdm and tests actual survival directly.

## Round 2 — under heavy load (glxgears + vkcube + Firefox WebGL live at reset)

Re-ran with three workloads rendering into venus first (glxgears **59.5 FPS** via GLX/Xwayland; vkcube
selecting `Virtio-GPU Venus` directly; Firefox `CanvasRenderer` WebGL context) — ~10 live contexts.
Drove the same stop-gdm → unbind → rebind → start-gdm cycle.

- **Host worker SURVIVED** the heavy teardown (**10** `CTX_DESTROY`, in-flight fences) — stayed alive.
- **Recovered to a *healthy* renderer:** venus re-enumerates; a **fresh** post-reset vkcube selects venus
  and glxgears renders **60.6 FPS**; new `CTX_CREATE`s after the re-init window all succeed (INFO, no fail).
- **NEW FINDING — the re-init under load is NOT clean.** A transient burst fired during the rebind window:
  `CTX_CREATE … invalid context id` → `ErrRutabaga(InvalidContextId)`, `ResourceCreateBlob`/`CmdSubmit3d`/
  `create_fence` → `ErrRutabaga(ComponentError(-1))`, `ResourceMapBlob`/`Unref` → `ErrInvalidResourceId`,
  plus virtio-mmio `update virtio queue in invalid state 0x0`. It **self-clears** (later contexts create
  fine), so the renderer recovers — but it points at a **desync: the reset `virtio_gpu` device and the
  venus render-server component are not reset in lockstep**, so commands land at the component before it's
  re-inited. That's a concrete **M9.3 hardening bug** for the host quiesce/renderer-reset entry point.
  (The light-load round-1 only grazed it — one `0x1200` error — because little was in flight.)

## Round 3 — raw unbind with the session LIVE (no stop-gdm): the real survival test

At the user's request, with gnome-shell + glxgears + vkcube all rendering, the raw `virtio_gpu` unbind
was issued **without** stopping gdm or releasing anything. Observations:

- **The unbind succeeded via DRM hot-unplug** (so my earlier "can't unbind with the session live" premise
  was wrong) — and **the live session did NOT survive:** gnome-shell + glxgears + vkcube **all crashed**
  (guest dmesg shows the `drm_gem_dmabuf_release` → `drm_dev_put` teardown backtrace; `/dev/dri/*` vanished).
- **The rebind left the renderer WEDGED on orphaned contexts.** On re-init, `CTX_CREATE ctx=3 (gnome-shell)`
  → `invalid context id`; the log then shows `CTX_DESTROY ctx=3` → `CTX_CREATE ctx=3` **succeeds**. The
  greeter's `ctx=2` stays orphaned (its owner is dead, so nothing ever `CTX_DESTROY`s it) → the gdm greeter
  is the only thing that comes back; the seated desktop does not. (New *short-lived* contexts — a later
  `vulkaninfo` — still work, so the renderer isn't dead; it's a **stale-context collision**.)

## Root cause (grounded in our source, not inferred)

- `Gpu::reset()` — `third_party/libkrun/src/devices/src/virtio/gpu/device.rs:379` — on a virtio_gpu device
  reset sends `WorkerCmd::Deactivate` and goes `Inactive` **"WITHOUT tearing down the renderer"** (re-`virgl_renderer_init`
  is expensive). **So the host renderer, and its rutabaga context table, persist across a device reset by design.**
- `VirtioGpu::reset_session()` — `third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:698` — clears
  the **device-level** maps (`self.resources`, `scanouts`, fence state, parked present frames) but **NOT the
  rutabaga renderer's context map.**
- Therefore: a reset that **was** preceded by graceful per-context `CTX_DESTROY` (rounds 1–2, where the
  guest tore the session down first — 10× `CTX_DESTROY` observed) leaves rutabaga's table empty → clean
  re-init. A reset that **was not** (round 3 — the crash skipped cleanup) leaves **orphaned rutabaga
  contexts** that collide with the re-initialized guest's fresh low ids → `invalid context id`. This is a
  real libkrun bug (`reset_session` should also drop the rutabaga contexts), **but it is tangential to the
  snapshot critical path** — see below.

## What we actually learned (grounded) vs. what's still open

**Observed (this session):**
1. The **host worker is robust** — it survives virtio_gpu device resets/unbinds, light or heavy load, and
   keeps serving new contexts. Solid.
2. The **clean path rebuilds** a correct desktop (round 1, pixel-verified).
3. A **running guest session does NOT survive abrupt loss of its GPU device** (round 3): the compositor +
   GPU apps crash. **Seamless survival is therefore not free — it requires guest-side resubmit support.**
4. `reset_session` leaves rutabaga contexts orphaned → stale-context collision on an *un-quiesced* reset.

**Inferred (reasoning, NOT tested here — flag before building on it):**
- A real host-side **restore uses a fresh worker → a fresh, empty rutabaga**, so round 3's stale-context
  collision is largely a **same-worker artifact and would not occur on a real restore.** What a restore
  *does* face is the guest's restored RAM holding resource/Vulkan handles that reference host state the
  fresh renderer never built → the guest must **resubmit/replay** it.
- Unbind/rebind triggers the guest's **remove/probe** (full teardown, apps die) — the *wrong* trigger;
  the snapshot path needs the guest's **freeze/restore** (preserve the object list, resubmit), i.e. the
  Dongwon-Kim series. So none of the three rounds is a faithful restore; that genuinely needs M9.3 plumbing.

## Corrected verdict (supersedes the earlier "renderer-reset hook" framing)

The earlier write-up's "first M9.3 increment = a libkrun renderer-reset hook" was a **misdiagnosis**: on a
real restore the renderer is *always* fresh (new worker), so there is no in-process renderer to reset and
no stale-context collision. The real guest-side work, split by tier:

- **Kernel virtio-gpu DRM driver (primary, required):** carry the **Dongwon-Kim drm/virtio freeze/restore
  series** — re-create virtqueues + resubmit `RESOURCE_CREATE_3D`/`ATTACH_BACKING`/`CONTEXT_CREATE` so the
  fresh host renderer's tables match what the guest believes exists. This alone largely covers the **virgl
  (GL) tier** (virglrenderer rebuilds from the resubmitted stream + guest-backed contents; Mesa virgl ≈
  transparent) — the Parallels/VMware-proven path.
- **Mesa venus + the host venus render-server (the hard, less-charted tier):** the kernel knows nothing
  about Vulkan objects, but the canonical `VkDevice`/image/pipeline graph lives in the host render-server
  (→ Metal) and is gone on a fresh worker. Transparent venus resume needs a venus-level **object-graph
  replay** (or a `DEVICE_LOST`-style re-create handshake) + render-server support — **not solved upstream.**
  → that's the **venus-resume spike** (read `src/virtio/vulkan/` for any suspend/replay hooks; pin down what
  the render-server holds vs. what the guest can replay).

So #3's true outcome: the host renderer is robust; the clean path rebuilds; **a live session does not
survive GPU loss without guest-side resubmit**; the real gate is guest-side (kernel Dongwon-Kim for virgl,
venus replay for the venus tier), *not* a host hook; and `reset_session`'s orphaned-context bug is a real
but side-path libkrun fix.

## Reproduce

```
cp -c Fedora-Workstation-43.enhanced.raw s4-venus-spike.raw
LIMINA_DISK="$PWD/s4-venus-spike.raw" LIMINA_EXTRA_ARGS="--ssh-port 2222" \
  bash spikes/venus-draw-probe/boot-seated-kk.sh > spikes/s4-hibernate/venus-boot.log 2>&1 &
# wait for "guest SSH forward ready" in /tmp/seated-kk-worker.log, then run the 7-step procedure above
# over: ssh -p 2222 claude@127.0.0.1  (creds in the limina-fedora-access memory)
```
