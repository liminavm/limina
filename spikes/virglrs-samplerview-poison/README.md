# The compositor could not sample a client buffer that needed a minted texture view

When gnome-shell's sampler view of an IOSurface-backed client buffer requires a *minted*
`glTextureView`, the host GL refuses it with `GL_INVALID_OPERATION`, and virglrs poisons the
compositor's context permanently. The desktop stops updating for good; the dash animates because
gnome-shell keeps submitting frames that are all refused. Nothing recovers it but restarting the
session.

`vkmark` is the reproducer to hand — its raw Vulkan Wayland swapchain images require such a view.
`glmark2-es2-wayland` on venus does **not**, so it never trips this despite being a windowed,
venus-backed client whose buffers the compositor samples every frame.

Observed at limina `30e5658b` / virglrs `34ed41d`, guest F44 enhanced (`7.1.8-limina16k.4`, mesa
`26.1.8-11.limina.fc44`), 4 vCPU / 4 GiB, 1280x800 @ 1.0, through
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`. **Every vkmark launch into a healthy session
poisons** — 4 for 4 across two pins.

## The fault

One refusal, then the context is dead:

```
[virglrs] refused: vrend ctx 2: CreateObject(SamplerView): GL error 0x502
ERROR krun_rutabaga_gfx::virgl_renderer] virglrs: submit_cmd: the context is poisoned
ERROR krun_devices::virtio::gpu::worker] [SUBMIT3D] ctx 2 submit_command
      -> Err("ErrRutabaga(ComponentError(-22))")
```

`ctx 2` is **gnome-shell's own vrend context** (`init=0x2`), not vkmark's (`init=0x4`, venus). It
fires 128–227 ms after vkmark's second `CTX_CREATE`, before vkmark draws anything. Every
subsequent `CmdSubmit3d` from gnome-shell is then refused for the life of the session — 26 927
refusals across the first two incidents, 5 886 in the verdict run. Killing vkmark changes nothing;
only `systemctl isolate multi-user.target` → `graphical.target` restores rendering.

vkmark itself is unharmed — it completes every scene and scores. "The Vulkan client works" is not
evidence the stack is healthy.

## The failing call, and what does not explain it

`LIMINA_GL_TRACE=1` puts the error on `glTextureView`. Full lines in `gl-trace-excerpt.log`:

```
[virglrs] vrend: sampler view: texture_view of resource ResourceHandle(1180) (1280x720
    R8G8B8X8_UNORM, immutable true, surface true, supports_view true) as R8G8B8X8_UNORM
    target 0xde1 internalformat 0x8058 levels 0+1 layers 0+1
[virglrs] vrend: sampler view: texture_view left GL error 0x502
```

`surface true` is `Resource::surface()` — *"the IOSurface this resource's storage is"*
(`vrend/resource.rs:530`) — so it means IOSurface-backed, **not** a render-target binding.

Ruled out by measurement, not argument:

- **Format, target, view range and the `supports_view` gate.** The same session makes 658
  successful `texture_view` calls, including `(64x64 R8G8B8X8_UNORM, immutable true, surface
  false, supports_view true) as R8G8B8X8_UNORM target 0xde1 internalformat 0x8058 levels 0+1
  layers 0+1` — identical in every field but `surface`.
- **A stale immutability flag.** virglrs `34ed41d` replaced the predicted `immutable` with a
  `GL_TEXTURE_IMMUTABLE_FORMAT` query at both exits of `alloc_texture`. The four still report
  `immutable true` and still leave `0x502`. So KosmicKrisp's `EXT_EGL_image_storage` delivers
  genuinely immutable-format storage, and **the host refuses to view externally imported storage
  regardless of immutability**. `34ed41d` is a correct hygiene fix — two copies of one fact, the
  read one unchecked — and not a cure.

## The cure

virglrs `42008bb` (`reimport-route.patch` as applied and measured here). vkmark's view is an
**identity** view — same format, same target, full level and layer range, so `reinterprets` is
false. The only thing it needed was the `W -> One` swizzle every alpha-less format carries, and a
swizzle needs a *private object*, not a view: GL keeps the swizzle on the texture object, so
sharing one texture across views lets the last writer win for every sampler reading it. The route
asked for a view anyway because the `Reimport` guard was gated on `!supports_view`, and
`supports_view` is true for XBGR8888.

`Route::Reimport` is `glEGLImageTargetTexture2DOES` into a fresh texture name — it aliases the
same EGL image, so there is no copy and no allocation beyond a texture object, once per
`CREATE_OBJECT(SamplerView)`. The new guard routes on the *need* rather than on the storage, so an
identity view of imported storage still mints nothing. The same rule also covers the latent case
where `!supports_view && reinterprets` fell through to `View`: a reinterpretation over storage no
view can be taken of is now served unreinterpreted rather than refused, on the same terms as a
host with no `glTextureView`.

**Measured cured**, untraced, at that patch: vkmark completes all scenes and scores 3363, **zero
`refused: vrend`, zero poisoned submits**. The compositor keeps rendering — successive scanout
captures differ under load — Firefox launches and the desktop keeps painting, and a human
confirmed the desktop looks healthy on screen. The two routing tests fail against the old
`view_route` (`left: View`) and pass against the new, checked by reverting the function rather
than by assuming.

## What is *not* the discriminator

Not the client, and not venus. `glmark2-es2-wayland` on venus, run first in the same session,
makes **zero `texture_view` calls** — its buffers never reach the view route at all. Nor do
`eglretrace`, `gfxrecon-replay`, `glmark2-es2` or `gst-plugin-scan`, which between them opened
fourteen further venus contexts across these runs without a fault.

So the axis is whether the sampler view must *mint* a view of an IOSurface-backed source, which
vkmark's swapchain images require and zink-exported buffers do not. Whether Firefox mints one is
**untested**, and it decides whether a real desktop hits this.

## `supports_view false` does not arise here

All 662 traced `texture_view` calls carry `supports_view true`. virglrs's second, latent
`view_route` poison — where a texture that cannot be viewed is still routed to `glTextureView`
when the view reinterprets — is therefore unexercised by this workload, and the tree's own
contradiction about what such a view does (`resource.rs:1207`, "reads its channels in the wrong
order", against `context.rs:2754`, "the driver refuses with `GL_INVALID_OPERATION`") stays
unsettled. Neither client here produces IOSurface-backed `B8G8R8A8/X8_UNORM`.

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

Add `LIMINA_GL_TRACE=1` to name the GL call. **That trace drains the error, so a traced run does
not poison** — it renders on, and a clean traced run is the trace eating the failure. A traced run
can say which branch we are in; only an untraced run can say whether a fix cures.

## The open question this bug raises independently

Should one failed `CreateObject` poison a context permanently? Even granting a bad create, the
blast radius is the whole desktop from one bad object. The C renderer logged the GL error and
carried on, so this is a behavioural change of the rewrite, and it is what turns a wrong-looking
window into a dead session.

`worker-excerpt.log` holds context creates/destroys and windows around the first two refusals;
`gl-trace-excerpt.log` the traced call sites; `immutable-query.patch` the `34ed41d` change as
applied here.
