# Codebases map — where every source tree lives and what it's for

The load-bearing mental model first, because the easy mistake (made 2026-07-06, reading
venus source out of the **host** mesa checkout) is confusing a *host* tree for a *guest*
one. **limina ships two sets of binaries built from different trees:**

- **HOST binaries** run on macOS, inside `limina.app` / the `limina-vmm` worker. This is
  the VMM, the host GPU renderer, and the host Vulkan-on-Metal driver.
- **GUEST binaries** run inside the Linux guest, i.e. baked into the `*.raw` images. This is
  the guest kernel, limina's GPU vendor drivers (KosmicKrisp, venus, virgl, zink and the VA
  driver, under `/opt/limina`), `limina-agent` and `limina-agent-session`. The guest's own
  Mesa, mutter and gnome-shell are the distro's; the GNOME clipboard is stock `spice-vdagent`.

A given component (especially *Mesa*) exists as source in more than one place and gets
**built differently for each side**. Never assume "the venus code I'm reading is what the
guest runs" — check which tree and which version.

Everything under `third_party/` is **gitignored** (from-source clones/builds). Each fork's
`limina` branch is the delta and `third_party/manifest.toml` pins its rev; `cargo xtask vendor`
recreates the `third_party/` trees from those pins. `patches/**` holds only tombstones and
reference material.

---

## HOST side (macOS — `limina.app` / `limina-vmm`)

| Tree | Branch / version | What it is | Governed by | Build → output |
|---|---|---|---|---|
| `third_party/libkrun` | **fork model** `liminavm/libkrun` branch `limina`, pinned in `third_party/manifest.toml` | The VMM (HVF + virtio + rutabaga). Our devices/APIs, incl. the emulated xHCI controller + USB gadgets and the M9 snapshot machinery. | **none** — the branch IS the delta (124 commits over upstream tip `07fd40dc`); `patches/libkrun/` retired 2026-08-06 | linked by `limina-vmm` (path deps) |
| `third_party/libkrunfw` | — | Bundled guest kernel (`linux-6.12.x`) for the **non-EFI** direct-boot path only. | — | libkrunfw dylib |
| `third_party/virglrs/third_party/virglrenderer` | **fork model** `liminavm/virglrenderer` branch `limina`, pinned by **virglrs**, not by limina | **The C reference the Rust rewrite generates format tables from and records goldens against** — not what the worker runs, and **not limina's to pin**: there is no `[virglrenderer]` in `third_party/manifest.toml`, because one owner is enough and this is not it. Both accelerated tiers: venus (Vulkan→KK) and vrend (GL via zink). Our macOS/venus enablement, IOSurface scanout, vkr fixes; major later additions: snapshot/restore journal (0033–0040), ring-wake profiling (0046–0048), ring-fatal fix (0058). This is also where upstreaming work happens (`docs/upstreaming/ledger/virglrenderer.md`). | **none** — the branch IS the delta; `patches/virglrenderer/` retired 2026-08-04 | materialized by virglrs's own `scripts/vendor.sh`; a spike that needs `config.h` must `meson setup build` there itself |
| `third_party/virgl-gl-prefix/` | — | Stale: a secondary C virglrenderer build output. Nothing in limina builds or loads the C renderer any more. | — | prefix only |
| `third_party/mesa-cs.sparseimage` → **`/Volumes/mesa-cs`** | **fork model** `liminavm/mesa` branch `limina-kk`, pinned in `third_party/manifest.toml`; **26.3.0-devel** | **HOST Mesa.** Builds **KosmicKrisp** (`libvulkan_kosmickrisp.dylib`, the sole venus backend) and **zink-on-KK** (host GL). `-Dplatforms=macos`. **venus is NOT built here** (`build-kk/src/virtio/vulkan/` is empty — the venus source is present-but-inert). Build dirs: `build-kk`, `build-zink-kk`, prefix `zink-kk-prefix`. **Both build dirs must be configured `-Db_ndebug=true`** (keep `buildtype=debugoptimized`, so `-O2`/`-g` and crash-report symbolication are unaffected): with asserts compiled in, a guest trips one — invalid Vulkan usage on the vkr ring thread, or a GL stream on zink's threaded-context worker — and SIGABRTs the worker, killing the VM. `scripts/build-app.sh` refuses to bundle **any** Mesa dylib that still references `__assert_rtn`, because this is meson state on the sparse image that nothing in the repo pins. | **none** — the branch IS the delta (20 commits over base `178a3d739`); `patches/kosmickrisp/` retired 2026-08-04. NOT vendored by `cargo xtask vendor` (case-sensitive sparse image) | **`scripts/build-host-mesa.sh`** (`cargo xtask mesa`) — creates the sparse image, clones the fork at its pin and builds BOTH dirs with `-Db_ndebug=true`. Toolchain is version-coupled: `mesa_clc` compiles `src/kosmickrisp/libkk/*.cl` against libclc's `spirv64-mesa3d-.spv`, which only **libclc 22.x** provides (23.x dropped the `.pc` Mesa finds it by and renamed the payload), so `[libclc]` in `third_party/manifest.toml` pins the bottle by digest and the script fetches it into `third_party/libclc/`. → `libvulkan_kosmickrisp.dylib`, zink `libEGL/libGL` |
| `third_party/libepoxy` + `epoxy-egl-prefix/` | — | EGL/GL dispatch the host zink-on-KK path needs. | — | prefix |
| `third_party/imago` | **fork model** `liminavm/imago` branch `limina`, pinned in `third_party/manifest.toml` | libkrun's virtio-blk backend (a crates.io dep), overridden via `[patch.crates-io]` in the **root Cargo.toml**. Discard→punch-hole fix. | **none** — the branch IS the delta | Rust dep |
| `third_party/mlx` | **fork model** `liminavm/mlx` branch `limina`, pinned in `third_party/manifest.toml` | MLX with its Metal backend built for Linux guests over limina's GPU transport (milestone 18): one CMake option, `MLX_METAL_LIMINA`. Built by `spikes/mlx-linux/`, not by limina. | **none** — the branch IS the delta | spike |
| `third_party/cardea` | **ours outright** (`github.com/liminavm/cardea`, MIT), pinned in `third_party/manifest.toml` | **Cardea, the guest's Rust UEFI firmware** for the EFI boot path (virtio-gpu GOP, virtio keyboards, TCG2/TIS measured boot, ACPI or device-tree passthrough). | — (not a fork) | `scripts/build-firmware.sh` (`cargo xtask firmware`), built natively → `target/firmware/efi.fd` — the app's and **the test suite's firmware** |

### Our own Rust crates (`crates/`) — host side
`limina` (app/CLI/supervisor) · `limina-vmm` (the VMM worker child) · `limina-display` ·
`limina-displayctl` · `limina-input` · `limina-proto` (control-plane wire types) ·
`limina-surfaceport` · `limina-usbip` · `limina-test` (L0/L1/L2 harness). The guest-side
Rust we own lives under **`guest/`** (excluded from the workspace): `guest/limina-agent`,
`guest/limina-agent-session`, `guest/limina-init`, `guest/virtio-gpu-dkms` (built for
`aarch64-unknown-linux-musl`/gnu, delivered into the guest).

---

## GUEST side (Linux — baked into the `*.raw` images)

The kernel is built from a fresh clone of its fork at the pin, in the unified Linux build
container or a booted F44 guest, and packaged as an RPM. The GPU vendor drivers are built from
the `[kosmickrisp-guest]` checkout on the case-sensitive sparse image, in their own build
container, for every distro at once. Both, with the agents, reach a guest in the payload
`scripts/package-enhanced.sh` assembles.

| Component | Source of truth | Version shipped (dogfood, 2026-07) | Governed by | Built via |
|---|---|---|---|---|
| **Guest GPU drivers** (vendor build) | **fork**: `github.com/liminavm/mesa`, branch `metal-native-context`, pinned as `[kosmickrisp-guest]` in `third_party/manifest.toml`; checkout `/Volumes/mesa-cs/mesa-metal-guest` | `/opt/limina/<wire>/` (its `MESA_REV` names the rev); `docs/images.md` §Component versions | the branch IS the delta: the host `limina-kk` branch plus guest-only commits (the limina transport, guest WSI, venus/virgl/zink/VA fixes); ledger `docs/upstreaming/ledger/kosmickrisp-guest.md` | `scripts/build-gpu-drivers.sh` (`-Dlimina-vendor=true`, Ubuntu 24.04 base for the oldest glibc), packaged with the DKMS virtio-gpu module by `scripts/package-gpu-drivers.sh`, installed by `guest/gpu-drivers/install.sh`. They drive every virtio-gpu: GL is zink on KosmicKrisp over a limina transport, virgl elsewhere; Vulkan is KosmicKrisp on the transport, venus otherwise; VA-API is virgl with H.264/HEVC. GLX clients and Flatpaks stay on the distro's Mesa. |
| **Guest Mesa** (system) | **STOCK distro** | distro's own | — | — |
| **Guest kernel** (16k) | **fork**: `github.com/liminavm/linux` (of `gregkh/linux`, the stable mirror), branch `limina`, base `v7.1.13`; pinned in `third_party/manifest.toml` | `7.1.13-limina16k.2` in the F44 enhanced images (`docs/images.md` §Component versions) | **Two commits over `v7.1.8`** (`6292465948e0` expose per-scanout rects as suggested connector offsets, `89ba418db60b` type and attach PRIME-imported dmabufs) — `scripts/export-linux-patches.sh` derives them for the consumers that apply a series. The pre-7.1 series is gone: the modifier/primary-plane work was **deliberately dropped** when we moved to upstream's direction of *not advertising fb modifiers* on virtio-gpu (synoik was patched to accept INVALID-modifier planes instead, #39); the page-reporting-vs-suspend backport landed upstream (`0b45f69`). The kernel is still a fork so we can carry a patch the moment we need one — it just carries none. *(16 KiB host-visible alignment left the series 2026-08-03 — a no-op on the 16k kernel; it lives in `guest/virtio-gpu-dkms/` for the stock-4k tier.)* Consequence to remember: **`drmModeAddFB2WithModifiers` returns EINVAL** on this kernel (`drm_internal_framebuffer_create: driver does not support fb modifiers`), so anything that scans out must fall back to plain `drmModeAddFB2`. | `scripts/provision/f44/build-kernel-rpm.sh` — in the unified F44 build container (`cargo xtask enhanced`) or inside a booted F44 guest (builds the pinned rev; no patch stage). The base config differs by where it runs: the running kernel's in a guest, the image's `kernel-core` in the container (`CONFIG_BASE` overrides). |
| **Guest mutter** | **STOCK Fedora** (the payload ships no mutter; the GNOME clipboard is stock `spice-vdagent`) | distro's own (e.g. `50.3-2.fc44`) | `patches/mutter/` 0003 kept unshipped for ext-data-control experiments | — |
| ~~**clipboard@limina** (gnome-shell extension)~~ | RETIRED 2026-08-15 (#37 step 4) — `guest/gnome-shell-extension/` is gone and `install-enhanced.sh` REMOVES it from guests that carry it | — | — | GNOME's clipboard is stock `spice-vdagent`; `limina-agent-session` yields to it |
| **limina-agent** | our repo (`guest/limina-agent`, alongside `guest/limina-agent-session`, `guest/limina-init`, `guest/virtio-gpu-dkms`; `guest/` is excluded from the workspace) | tracks repo | — | cross-compiled, delivered in the guest-tools payload |
| `third_party/mutter` (49.5, detached) | — | reference source checkout only | `patches/mutter/` | — |

Enhanced-tier delivery is additive: the kernel RPM installs beside the stock kernel, and the GPU
drivers beside the distro's Mesa, which nothing replaces or locks. `docs/images.md` §Component
versions is the authoritative version table.

---

## Reference / comparison trees (not shipped)

- `third_party/virglrenderer-slp` (detached, `bfec2d2`) — **upstream krunkit's** virglrenderer
  (`slp` = Sergio, the krunkit/libkrun author). A baseline to diff our fork against; **we do
  not link it.**
- `third_party/krunkit` — upstream krunkit, reference.
- `third_party/MoltenVK-src` — **retired** venus backend (2026-06-13, crashed the compositor).
  Kept for the archived instrumented oracle (`spikes/archive/moltenvk/`). KK replaced it.
- `third_party/VK-GL-CTS` — Khronos conformance suite, for driver testing.
- `third_party/gnome-shell-rs` — measurement vehicle (cited in `docs/images.md`).
- `third_party/smithay` — reference compositor tree.
- `third_party/wayland-protocols`, `third_party/venv-mesa` — build deps (protocol XML; the
  Python venv meson/mako use for Mesa builds).

---

## The two traps this map exists to prevent

1. **`/Volumes/mesa-cs/mesa` is the HOST Mesa (KosmicKrisp + zink-on-KK), not the guest
   drivers.** venus source *lives* in that tree but is unbuilt there. To change anything the
   guest runs — venus, virgl, the VA driver, guest zink, or KosmicKrisp on the limina
   transport — commit on **`metal-native-context` (checkout `/Volumes/mesa-cs/mesa-metal-guest`)**
   and rebuild with `scripts/build-gpu-drivers.sh`.
2. **One mesa fork, two branches, two destinations.** `liminavm/mesa` carries the **host**
   KK/zink build on `limina-kk` and the **guest** vendor drivers on `metal-native-context`
   (`limina-kk` plus guest-only commits). Neither has a patch directory. Always check *which
   branch/build* a given mesa change targets.
