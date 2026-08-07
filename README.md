# limina

A native macOS app (Apple Silicon) that runs **Linux** desktop guests on
[libkrun](https://github.com/containers/libkrun) + Hypervisor.framework, built in
Rust, aiming to **replace Parallels**. Today it boots a stock Fedora guest to a usable
desktop, and an *enhanced* guest (our custom kernel + drivers) to a full venus-accelerated
GNOME desktop in a native window.

The target feature set — see [Status](#status) for what's already shipped:

- High-quality 3D graphics acceleration (venus → KosmicKrisp → Metal)
- First-class fullscreen + mouse capture + macOS key-combo capture
- Customizable key bindings + keyboard-layout remapping (e.g. swap Command/Option)
- Clipboard sharing and host-folder sharing (virtiofs)
- USB device passthrough
- NAT networking (bridged deferred)
- Low host+guest memory overhead and dynamic memory (a `min..max` range the VM
  takes/returns via ballooning)

We own the whole stack and patch any layer to get the behavior we want — libkrun,
virglrenderer, imago, the guest kernel, Mesa, and custom guest drivers/agents are all
fair game (patch series committed under `patches/**`, source clones vendored under the
gitignored `third_party/`). Two tiers always coexist: an **unmodified stock distro must
always boot** (degraded), and installing our custom kernel/drivers/agent **unlocks** the
full experience. See [`CLAUDE.md`](CLAUDE.md) for the project tenets and
[`docs/`](docs/) for the design.

## Status

Milestones M1–M6 and M10 are shipped (boot → native Metal window → console/serial →
NAT networking → venus 3D → clipboard/virtiofs/agent → dynamic memory → multiple
disks/CD-ROM). M7 USB works as an end-to-end mock over vsock — real-device claim needs
the deferred privileged helper. M8 polish is largely done (fullscreen, key remap,
combo/pointer capture, runtime resize), and the `cargo xtask` dev surface (M11) is in.
In flight: M9 suspend/resume + snapshots (designed), M8 audio + x86, and productization
tails. The live status is in [`docs/roadmap.md`](docs/roadmap.md).

## Getting started (dev)

```sh
cargo xtask setup     # fresh-clone bootstrap: vendor third_party/ + enable git hooks
cargo xtask build     # build + codesign the worker + venus link-check
cargo xtask test      # the full HVF-gated boot suite ("did I break boot")
cargo xtask run --disk <enhanced.raw>   # boot to the seated venus desktop in a window
```

`cargo xtask --help` lists the whole surface (also `sign`, `app`, `bundle`). Each
command shells out to the tested `scripts/`, which stay the source of truth. New to the
repo? Read [`docs/dev-onboarding.md`](docs/dev-onboarding.md).

## Layout

| Path | Purpose |
|------|---------|
| `crates/` | The host Rust workspace — `limina` (AppKit supervisor/UI), `limina-vmm` (the HVF worker), display/input/proto/surfaceport/usbip helpers, `limina-test` (the boot-test harness) |
| `guest/` | aarch64-linux guest components (their own workspace): `limina-agent` (+ session helper), `limina-init`, `limina-config`, the virtio-gpu DKMS driver, the GNOME clipboard extension |
| `xtask/` | The `cargo xtask` dev-task runner |
| `scripts/` | The tested build/boot/provision scripts xtask wraps |
| `patches/` | Committed `git format-patch` series: libkrun, virglrenderer, imago, linux, mesa, mutter, edk2 (KRUN_EFI firmware), kosmickrisp |
| `spikes/` | Standalone experiments (each with a `RESULTS.md`) |
| `docs/` | [research](docs/research/) (source-cited), [design](docs/design/), [roadmap](docs/roadmap.md), [images](docs/images.md), [codebases](docs/codebases.md) |
| `third_party/` | Vendored / patched native dependencies (gitignored; recreated by `cargo xtask vendor`) |

## Host environment (reference)

- macOS 26.5, Apple M1 Max, 32 GB RAM, arm64, **16 KiB host pages**
- Rust 1.88, full Xcode 26.4 / Apple clang 21
- Homebrew provides the VM stack (libkrun, krunkit, libkrunfw, virglrenderer,
  molten-vk, vulkan-loader, gvproxy, libusb, qemu, cmake/meson/ninja) — though the
  shipping app links our own patched builds, not Homebrew's.

## License

limina is licensed **GPL-2.0-only WITH LicenseRef-limina-exception** — the GNU
General Public License, version 2, plus an additional permission (the *limina
linking exception*, [`LICENSES/LicenseRef-limina-exception.txt`](LICENSES/LicenseRef-limina-exception.txt))
that explicitly allows combining limina with Apache-2.0-licensed material and
conveying the result. The exception exists because limina statically links
[libkrun](https://github.com/liminavm/libkrun) and other Apache-2.0 crates, and
Apache-2.0 is famously incompatible with plain GPLv2; the additional permission
resolves that combination directly while keeping limina's own code GPLv2-only.

Some subtrees carry their own licenses (see [`REUSE.toml`](REUSE.toml)): the
kernel-derived `guest/virtio-gpu-dkms/` is GPL-2.0-only, patches against Mesa
are MIT, and an instrumentation patch against MoltenVK is Apache-2.0. The repo
is [REUSE](https://reuse.software/)-compliant; `reuse lint` verifies it.

Contributions are accepted under the same terms, including the linking
exception.
