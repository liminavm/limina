# Debian LUKS prompt not presented — repro harness

**Symptom (one dogfood sighting, managed Debian VM, release app):** after GRUB's "Loading initial
ramdisk" the window kept that frame. The guest had drawn the passphrase prompt: clicking the window
re-rendered it and the prompt was there.

**Status: not reproduced; parked until it is seen again.** Measured 2026-10-02 against the
`Debian-testing.luks.raw` guest:
- **Flat vehicle:** 41 boots.
- **Managed start through the release app:** 21 boots, using the Debian VM's own `vm.toml` and its
  `state.toml`, so it starts fullscreen on the saved display.

Every boot presented the prompt, at about 8 s (flat) or 7.5-11 s (managed). That is after the
fullscreen transition settles; it settles at launch.

## How it works

Each boot runs to the passphrase prompt and is then killed, so the user never types the passphrase.

**Recording, per boot:**
- **Frames:** `LIMINA_WINDOW_CAPTURE` at 250 ms. Every distinct frame is kept with its time since
  launch.
- **Serial console:** `--console`.

**`judge.py`'s verdict:**
- STUCK when the serial console shows `Please unlock disk` but the last presented frame's prompt
  row differs from a reference. The reference is a healthy boot's last frame, under
  `work.noindex/ref/frames/`.
- STUCK when the last frame is not the guest driver's 2560x1440 mode.

**What the oracle can and cannot see.** The capture records the surface the window *hands its
layer* (`window/windows.rs`, at `core.show`), not what the window server composites. A prompt
frame that never reaches the window process shows up as STUCK. A frame that is set but never
composited does not. This shell has no Screen Recording grant, so `screencapture` cannot stand in
for glass. If the symptom is window-server side, the oracle is a human, or a log of the layer
writes alongside window state (occlusion, active Space, key).

## Use

All paths are under `work.noindex/` (gitignored), or under `$CONREDRAW_DIR` if set.

1. **The disk:** `cp -c Debian-testing.luks.raw spikes/console-redraw-repro/work.noindex/disk.raw`.
2. **Flat runs:** `cargo xtask build`. Run `iter.sh ref 35` once, rename `ref`'s output into
   `work.noindex/ref`, then run `loop.sh 1 40`.
3. **Managed runs:**
   1. `cargo xtask app` (the harness drives `target/Limina.app`).
   2. Create the bundle `work.noindex/lib/DebianRepro.liminavm/vm.toml`: a copy of the Debian VM's
      `vm.toml` with a new name, uuid and MAC, and the disk pointing at the clone.
   3. Copy the Debian VM's `state.toml` to `work.noindex/state.seed.toml`.
   4. Run `mloop.sh 1 30`. Each boot takes over its display fullscreen for about 35 s.

Both loops stop at the first STUCK boot and leave its frames, console and log in `i<n>/` or
`m<n>/`.
