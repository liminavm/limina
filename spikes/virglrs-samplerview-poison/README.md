# vkmark's arrival poisons the compositor's vrend context, permanently

Launching **vkmark** on the enhanced tier kills the GNOME session's rendering for good. The
desktop stops updating; the dash animates because gnome-shell keeps submitting frames that are
all refused. Nothing recovers it but restarting the session.

Observed on limina `ce19a95b` with virglrs pinned at `894b36d`, guest F44 enhanced
(`7.1.8-limina16k.4`, mesa `26.1.8-11.limina.fc44`), 4 vCPU / 4 GiB, 1280x800 @ 1.0, booted
through `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`. **2 vkmark launches into a healthy
session, 2 poisons** — the second on a freshly restarted session with nothing else running.

## The fault

One refusal, then the context is dead:

```
[virglrs] refused: vrend ctx 2: CreateObject(SamplerView): GL error 0x502
ERROR krun_rutabaga_gfx::virgl_renderer] virglrs: submit_cmd: the context is poisoned
ERROR krun_devices::virtio::gpu::worker] [SUBMIT3D] ctx 2 submit_command
      -> Err("ErrRutabaga(ComponentError(-22))")
```

`ctx 2` is **gnome-shell's own vrend context** (`CTX_CREATE ctx=2 init=0x2 name="gnome-shell"`),
not vkmark's (`init=0x4`, venus). So the compositor's classic-GL context throws
`GL_INVALID_OPERATION` creating a sampler view — the path where vrend samples a venus client's
buffer. It fires **128 ms** (incident 1) and **227 ms** (incident 2) after vkmark's second
`CTX_CREATE`, before vkmark draws anything.

virglrs then poisons the context, and **every subsequent `CmdSubmit3d` from gnome-shell is
refused for the life of the session** — 26 927 refusals across the two incidents here. Killing
vkmark changes nothing. Only `systemctl isolate multi-user.target` → `graphical.target`
restores rendering.

vkmark itself is unharmed: it completes all six scenes and scores 3903. Its venus path is
healthy; only the compositor dies. So "the Vulkan client works" is not evidence the stack is.

## It is vkmark specifically, not venus clients in general

Fourteen other venus contexts were created and destroyed across the same session with no fault:

| client | `init` | venus contexts | poisoned |
|---|---|---|---|
| `glmark2-es2-wayland` (windowed, zink→venus) | 0x4 | 3 | no |
| `glmark2-es2` | 0x4 | 3 | no |
| `eglretrace` | 0x4 | 3 | no |
| `gfxrecon-replay` | 0x4 | 3 | no |
| `gst-plugin-scan` | 0x4 | 2 | no |
| **`vkmark`** | **0x4** | **2 (into a healthy session)** | **2/2** |

`glmark2-es2-wayland` is the interesting negative: it is *also* a windowed venus-backed client
whose buffers the compositor samples, and it never faults. So the discriminator is narrower than
"venus" — it is something about the buffers vkmark exports (a raw Vulkan Wayland swapchain)
versus the ones zink exports.

Nothing was playing video. The two `gst-plugin-scan` contexts are GStreamer's registry scan at
session start, minutes earlier and benign. No Firefox — whether it trips this is **untested**.

## The failing call is `glTextureView`, and the discriminator is `surface`

A `LIMINA_GL_TRACE=1` run (limina `118c9155` / virglrs `58a29c9`) names it. Full evidence in
`gl-trace-excerpt.log`; the four failures are vkmark's swapchain images:

```
[virglrs] vrend: sampler view: texture_view of resource ResourceHandle(1050) (1280x720
    R8G8B8X8_UNORM, immutable true, surface true, supports_view true) as R8G8B8X8_UNORM
    target 0xde1 internalformat 0x8058 levels 0+1 layers 0+1
[virglrs] vrend: sampler view: texture_view left GL error 0x502
```

Across the whole session the split is total:

| `texture_view` calls | count | left `0x502` |
|---|---|---|
| resource has a surface (`surface true`) | 4 | **4** |
| no surface (`surface false`) | 658 | 0 |

The clean cases include the *same format* at the same target, internalformat, level and layer
range — `(64x64 R8G8B8X8_UNORM, immutable true, surface false, supports_view true) as
R8G8B8X8_UNORM target 0xde1 internalformat 0x8058 levels 0+1 layers 0+1`. So format
compatibility, target, view range and the `supports_view` gate are all exonerated. The only
logged field that differs is `surface`.

`glTextureView` raises `GL_INVALID_OPERATION` if the source texture is not immutable-format.
`immutable true` here is virglrs's own bookkeeping, not a `GL_TEXTURE_IMMUTABLE_FORMAT` query —
so the hypothesis this evidence supports is that a resource carrying a surface reaches GL by a
route that does not produce an immutable-format texture (an EGLImage/IOSurface import rather
than `glTexStorage`), while virglrs's record still says immutable. **Not yet confirmed**: the
check that would settle it is querying `GL_TEXTURE_IMMUTABLE_FORMAT` on the source at the call.

The traced run did **not** poison, exactly as designed — the trace drains the error. Zero
`refused: vrend` lines against 26 927 refusals in the untraced run, and the desktop kept
rendering. That is the drain, not a fix.

## What the log does *not* say

There is no `[virglrs] vrend:` line of any kind between vkmark's `CTX_CREATE` and the refusal —
no "sampled whole" notice, no `composite target`, no `iosurface scanout`. The composite-target
lines in the log all belong to the earlier `gst-plugin-scan` contexts. So the offending resource
announced itself on none of the routes that log.

Exactly one `refused: vrend` line exists in the whole 105k-line worker log. Every other refusal
in it is the benign video-format capability notice.

## Reproduce

```bash
cp -c Fedora-Workstation-44.enhanced.raw poison.raw
env LIMINA_DISK=poison.raw LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 \
    LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
    RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info \
    spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &
port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-poison.log)
ssh -p $port claude@127.0.0.1 \
  'export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0; vkmark -s 1280x720'
grep -e "refused: vrend" -e poisoned /tmp/limina-worker-poison.log
```

To name the GL call rather than the command, add `LIMINA_GL_TRACE=1` to the worker's
environment — `create_sampler_view` then prints which call left the error. **That trace drains
the error, so a traced run does not poison**: it gets further, and a clean traced run is the
trace eating the failure, not a fix. Only the printed line is evidence.

## Two separable questions

1. **Is the sampler view genuinely invalid** for an imported venus resource, or is `0x502` a
   stale error left in GL state and only observed at this check? This is the same class as the
   sampler-view routing fix already pinned, so that fix may be incomplete for the cross-tier case.
2. **Should one failed `CreateObject` poison a context permanently?** Even granting a bad create,
   the blast radius is the whole desktop. The C renderer logged the GL error and carried on, so
   this is a behavioural change of the rewrite independent of question 1.

`worker-excerpt.log` holds every context create/destroy plus a window around each refusal.
