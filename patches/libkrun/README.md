# libkrun patch series

limina vendors libkrun under `third_party/libkrun` (gitignored — a from-source checkout we
build against via its internal Rust crates, decision D2.1). Our changes to libkrun live
here as a `git format-patch` series so they survive a re-clone and stay reviewable across
rebases onto upstream.

- **`UPSTREAM_BASE`** — the upstream libkrun commit the series applies onto.
- **`NNNN-*.patch`** — our patches, in order. Author with `git format-patch` from the
  vendored checkout (commit on a `limina/*` branch first), output here.

## Apply onto a fresh checkout

```sh
scripts/apply-libkrun-patches.sh
```

This checks out `third_party/libkrun` at `UPSTREAM_BASE` and `git am`s the series.

## Add / update a patch

1. Edit `third_party/libkrun` directly, commit on a `limina/*` branch (one logical change
   per commit) with a `Co-Authored-By` trailer.
2. Re-export: `git -C third_party/libkrun format-patch <base>.. -o "$PWD/patches/libkrun"`.
3. Commit the regenerated `.patch` files to the limina repo.

## Current patches

- **0001 — software 2D virtio-gpu scanout for GL-less hosts (no renderer init).** libkrun
  maps `RESOURCE_CREATE_2D` onto a virgl GL render target, which has no host context on
  macOS, so 2D resource creation fails and nothing reaches the display. Shadows 2D
  resources in host CPU memory (create/attach-backing/transfer/set-scanout/flush) without
  touching rutabaga — a working software scanout baseline (fbcon, EFI GOP, simpledrm).
  Adds an opt-in software-2D-only device mode (`set_gpu_software_2d`) that skips renderer
  init entirely: `rutabaga` stays `None` (renderer-backed 3D/blob/context/capset/fence
  commands degrade to ERR_UNSPEC), and the device advertises a plain 2D virtio-gpu (no
  VIRGL/BLOB/CONTEXT_INIT, 0 capsets) so the guest never issues 3D. This removes the
  virglrenderer/Metal dependency (and its init cost/hangs) from limina's Tier-1 display
  floor. With the mode off, renderer init and the accelerated Venus/blob/3D path are
  unchanged.
- **0002 — PL011 drops serial output on WouldBlock instead of erroring.** The serial TX
  path runs on the vCPU thread; a non-blocking sink (e.g. a pty with no reader) returns
  WouldBlock on a full buffer. Drop the byte rather than stall the vCPU or error per byte.
- **0003 — virtio-console `ConsoleInOut` port + quiet raw-mode ENOTTY.** Adds a
  `PortConfig::ConsoleInOut` that wires non-tty fds (a file + a FIFO) as a *console* port
  (so the guest exposes it as `hvc0`, not a `/dev/vport` data port) with no `isatty`
  gating; downgrades the terminal raw-mode ENOTTY on a non-tty fd from error to debug.
- **0004 — HVF support 16-bit (halfword) MMIO writes.** The aarch64 data-abort write path
  matched only len 1/4/8 and `panic!`ed on len=2, killing the vCPU thread. The ARM PL011
  driver uses 16-bit `writew`/`readw`; the read side already handled len=2. Add the write
  case. (Was the real cause of the apparent "PL011 amba-probe deadlock".)
- **0005 — FDT mark PL011 serial node as arm,primecell.** Lets the guest's AMBA layer bind
  `amba-pl011` and expose a real bidirectional `/dev/ttyAMA0` (the interactive serial debug
  console), instead of PL011 being an earlycon/output-only stdout-path. Safe given 0004.
