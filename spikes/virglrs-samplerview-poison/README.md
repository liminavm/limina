# A venus client's arrival poisons the compositor's vrend context, permanently

Launching any venus Vulkan client on the enhanced tier kills the GNOME session's rendering
for good. The desktop stops updating; the dash animates because gnome-shell keeps submitting
frames that are all refused. Nothing recovers it but restarting the session.

Observed on limina `ce19a95b` with virglrs pinned at `894b36d`, guest F44 enhanced
(`7.1.8-limina16k.4`, mesa `26.1.8-11.limina.fc44`), 4 vCPU / 4 GiB, 1280x800 @ 1.0, booted
through `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`. Reproduced 2/2, the second time on a
freshly restarted session with nothing else running.

## The fault

One refusal, then the context is dead:

```
[virglrs] refused: vrend ctx 2: CreateObject(SamplerView): GL error 0x502
ERROR krun_rutabaga_gfx::virgl_renderer] virglrs: submit_cmd: the context is poisoned
ERROR krun_devices::virtio::gpu::worker] [SUBMIT3D] ctx 2 submit_command
      -> Err("ErrRutabaga(ComponentError(-22))")
```

`ctx 2` is **gnome-shell's own vrend context** (`CTX_CREATE ctx=2 init=0x2 name="gnome-shell"`),
not the client's (`init=0x4`, venus). So the compositor's classic-GL context throws
`GL_INVALID_OPERATION` creating a sampler view — the path where vrend samples a venus client's
buffer. It fires ~230 ms after the client's `CTX_CREATE`, before that client draws anything.

virglrs then poisons the context, and **every subsequent `CmdSubmit3d` from gnome-shell is
refused for the life of the session** — 26 927 refusals across the two incidents here. Killing
the client changes nothing. Only `systemctl isolate multi-user.target` → `graphical.target`
restores rendering.

The triggering client is unharmed: vkmark completes all six scenes and scores 3903. Its venus
path is healthy; only the compositor dies.

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

Whether Firefox (also a venus client here) trips the same path is **untested** — it decides
whether the graphics perf battery can run on this tree at all.

## Two separable questions

1. **Is the sampler view genuinely invalid** for an imported venus resource, or is `0x502` a
   stale error left in GL state and only observed at this check? This is the same class as the
   sampler-view routing fix already pinned, so that fix may be incomplete for the cross-tier case.
2. **Should one failed `CreateObject` poison a context permanently?** Even granting a bad create,
   the blast radius is the whole desktop. The C renderer logged the GL error and carried on, so
   this is a behavioural change of the rewrite independent of question 1.

`worker-excerpt.log` holds every context create/destroy plus a window around each refusal.
