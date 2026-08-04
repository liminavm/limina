# Codebases map — where every source tree lives and what it's for

The load-bearing mental model first, because the easy mistake (made 2026-07-06, reading
venus source out of the **host** mesa checkout) is confusing a *host* tree for a *guest*
one. **limina ships two sets of binaries built from different trees:**

- **HOST binaries** run on macOS, inside `limina.app` / the `limina-vmm` worker. This is
  the VMM, the host GPU renderer, and the host Vulkan-on-Metal driver.
- **GUEST binaries** run inside the Linux guest, i.e. baked into the `*.raw` images. This is
  the guest kernel, the guest Mesa (venus + zink), `limina-agent`, and the
  `clipboard@limina` gnome-shell extension (guest mutter is STOCK since 2026-07-11).

A given component (especially *Mesa*) exists as source in more than one place and gets
**built differently for each side**. Never assume "the venus code I'm reading is what the
guest runs" — check which tree and which version.

Everything under `third_party/` is **gitignored** (from-source clones/builds); the
**patch series** under `patches/**` are committed and are the real source of truth.
`cargo xtask vendor` recreates the `third_party/` trees from the series.

---

## HOST side (macOS — `limina.app` / `limina-vmm`)

| Tree | Branch / version | What it is | Governed by | Build → output |
|---|---|---|---|---|
| `third_party/libkrun` | `limina/macos-venus` family | The VMM (HVF + virtio + rutabaga). Our devices/APIs. Series is at **0126**, incl. the emulated xHCI controller + USB gadgets (0095–0105, 0126) and snapshot machinery (0076–0086). | `patches/libkrun/` (UPSTREAM_BASE `07a3f40`) | linked by `limina-vmm` |
| `third_party/libkrunfw` | — | Bundled guest kernel (`linux-6.12.x`) for the **non-EFI** direct-boot path only. | — | libkrunfw dylib |
| `third_party/virglrenderer` | `limina/macos-venus`, head `f580706` | **THE host GPU renderer.** Both accelerated tiers: venus (Vulkan→KK) and vrend (GL via zink). Our macOS/venus enablement, IOSurface scanout, vkr fixes; major later additions: snapshot/restore journal (0033–0040), ring-wake profiling (0046–0048), ring-fatal fix (0058). | `patches/virglrenderer/` 0001–0058 (UPSTREAM_BASE `2048dfb`) | `third_party/virgl-prefix/` ← **the worker links this** (verify: `otool -L target/debug/limina-vmm \| grep virgl`) |
| `third_party/virgl-gl-prefix/` | — | Secondary virglrenderer build output (GL/vrend-focused). Built by `scripts/build-virglrenderer.sh`. NOT the default link. | (see build script) | prefix only |
| `third_party/mesa-cs.sparseimage` → **`/Volumes/mesa-cs`** | `limina/kosmickrisp`, **26.2.0-devel** | **HOST Mesa.** Builds **KosmicKrisp** (`libvulkan_kosmickrisp.dylib`, the sole venus backend) and **zink-on-KK** (host GL). `-Dplatforms=macos`. **venus is NOT built here** (`build-kk/src/virtio/vulkan/` is empty — the venus source is present-but-inert). Build dirs: `build-kk`, `build-zink-kk`, prefix `zink-kk-prefix`. | `patches/kosmickrisp/` (committed on the branch; UPSTREAM_BASE `178a3d739`) + host-zink patches | `libvulkan_kosmickrisp.dylib`, zink `libEGL/libGL` |
| `third_party/libepoxy` + `epoxy-egl-prefix/` | — | EGL/GL dispatch the host zink-on-KK path needs. | — | prefix |
| `third_party/imago` | — | libkrun's virtio-blk backend (a crates.io dep), overridden via `[patch.crates-io]` in the **root Cargo.toml**. Discard→punch-hole fix. | `patches/imago/` | Rust dep |
| `patches/edk2/` (no tree; built from `slp/edk2@krun-support`) | — | GOP `KRUN_EFI` firmware for the EFI boot path. | `patches/edk2/` | `scripts/build-krun-efi.sh` |

### Our own Rust crates (`crates/`) — host side
`limina` (app/CLI/supervisor) · `limina-vmm` (the VMM worker child) · `limina-display` ·
`limina-displayctl` · `limina-input` · `limina-proto` (control-plane wire types) ·
`limina-surfaceport` · `limina-usbip` · `limina-test` (L0/L1/L2 harness). The guest-side
Rust we own lives under **`guest/`** (excluded from the workspace): `guest/limina-agent`,
`guest/limina-agent-session`, `guest/limina-init`, `guest/virtio-gpu-dkms` (built for
`aarch64-unknown-linux-musl`/gnu, delivered into the guest).

---

## GUEST side (Linux — baked into the `*.raw` images)

None of these has a *standing* checkout on the dev Mac the way the host trees do — they are
built from a distro SRPM (Mesa, mutter) or a fresh clone (kernel) **inside a Fedora build
guest**, then packaged as RPMs. The **patch series is the durable artifact.**

| Component | Source of truth | Version shipped (dogfood, 2026-07) | Governed by | Built via |
|---|---|---|---|---|
| **Guest Mesa** (venus **+** zink) | Fedora **F44's own** `mesa-*.src.rpm` — *not* the host 26.2.0-devel | `mesa 26.1.4-3.limina.fc44` | `patches/mesa/` **0001 (zink)** + **0015, 0010–0014, 0016, 0017 (venus/zink)** — 0009 is NOT applied (0015 superseded it for bases ≥ 26.1.4) | `scripts/provision/f44/build-mesa-rpm.sh` (applies exactly that subset; dnf-versionlocked) |
| **Guest kernel** (16k) | **fork**: `github.com/liminavm/linux` (of `gregkh/linux`, the stable mirror), branch `limina`, base `v7.1.6`; pinned in `third_party/manifest.toml` | `7.1.5-limina16k` deployed (dogfood); `7.1.4-limina16k` in the enhanced images; `7.1.6-limina16k` incoming | 4 commits on the branch: page-reporting-vs-suspend backport (upstream `0b45f69`), blob-scanout flush fence, widened primary-plane formats, LINEAR modifier. *(16 KiB host-visible alignment left the series 2026-08-03 — it is a no-op on the 16k kernel and lives in `guest/virtio-gpu-dkms/` for the stock-4k tier.)* | `scripts/provision/f44/build-kernel-rpm.sh` on a dev-Mac F44 build guest (builds the pinned rev; no patch stage) |
| **Guest mutter** | **STOCK Fedora** (since 2026-07-11 the payload ships NO mutter; the GNOME clipboard tier is the shell extension below) | distro's own (e.g. `50.3-2.fc44`) | `patches/mutter/` 0003 kept UNSHIPPED for ext-data-control experiments (0001/0002 retired) | optional: `scripts/provision/f44/build-mutter-rpm.sh` |
| **clipboard@limina** (gnome-shell extension) | `guest/gnome-shell-extension/` | tracks repo | — (plain GJS, no build) | staged by `build-all.sh`, installed to `/usr/share/gnome-shell/extensions` |
| **limina-agent** | our repo (`guest/limina-agent`, alongside `guest/limina-agent-session`, `guest/limina-init`, `guest/virtio-gpu-dkms`; `guest/` is excluded from the workspace) | tracks repo | — | cross-compiled, delivered in the guest-tools payload |
| `third_party/mutter` (49.5, detached) | — | reference source checkout only; shipped mutter is RPM-built as above | `patches/mutter/` | — |

Enhanced-tier delivery = these RPMs **replacing stock at `/usr`**; see [[limina-enh-delivery]]
and `docs/images.md` §Component versions for the authoritative version table.

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

1. **`mesa-cs` is the HOST Mesa (KosmicKrisp + zink-on-KK), not the guest venus driver.**
   venus source *lives* in that tree but is unbuilt and at a **different version** (26.2.0-devel)
   than the guest ships (26.1.4). To change guest venus, work against the **F44 mesa SRPM
   version + the `patches/mesa/` guest subset (0015, 0010–0014, 0016, 0017)**, not `/Volumes/mesa-cs`. venus only ever runs in the
   guest; KosmicKrisp only ever runs on the host.
2. **`patches/mesa/` serves two different builds.** `build-mesa-rpm.sh` applies **0001 + 0015 +
   0010–0014 + 0016/0017** to the **guest** RPM. The **host** KK/zink build (`mesa-cs`, branch `limina/kosmickrisp`) carries
   `patches/kosmickrisp/` as branch commits. Same directory of patches, two destinations — always
   check *which build* a given patch number targets.

*(Open to refine: exact virgl-prefix vs virgl-gl-prefix build-arg split; full host-zink
patch subset applied to `mesa-cs`. Add here as confirmed.)*
