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
| `third_party/libkrun` | **fork model** `liminavm/libkrun` branch `limina`, pinned in `third_party/manifest.toml` | The VMM (HVF + virtio + rutabaga). Our devices/APIs, incl. the emulated xHCI controller + USB gadgets and the M9 snapshot machinery. | **none** — the branch IS the delta (124 commits over upstream tip `07fd40dc`); `patches/libkrun/` retired 2026-08-06 | linked by `limina-vmm` (path deps) |
| `third_party/libkrunfw` | — | Bundled guest kernel (`linux-6.12.x`) for the **non-EFI** direct-boot path only. | — | libkrunfw dylib |
| `third_party/virglrenderer` | **fork model** `liminavm/virglrenderer` branch `limina`, pinned in `third_party/manifest.toml` | **THE host GPU renderer.** Both accelerated tiers: venus (Vulkan→KK) and vrend (GL via zink). Our macOS/venus enablement, IOSurface scanout, vkr fixes; major later additions: snapshot/restore journal (0033–0040), ring-wake profiling (0046–0048), ring-fatal fix (0058). | **none** — the branch IS the delta (60 commits over base `2048dfb`); `patches/virglrenderer/` retired 2026-08-04 | `third_party/virgl-prefix/` ← **the worker links this** (verify: `otool -L target/debug/limina-vmm \| grep virgl`) |
| `third_party/virgl-gl-prefix/` | — | Secondary virglrenderer build output (GL/vrend-focused). Built by `scripts/build-virglrenderer.sh`. NOT the default link. | (see build script) | prefix only |
| `third_party/mesa-cs.sparseimage` → **`/Volumes/mesa-cs`** | **fork model** `liminavm/mesa` branch `limina-kk`, pinned in `third_party/manifest.toml`; **26.2.0-devel** | **HOST Mesa.** Builds **KosmicKrisp** (`libvulkan_kosmickrisp.dylib`, the sole venus backend) and **zink-on-KK** (host GL). `-Dplatforms=macos`. **venus is NOT built here** (`build-kk/src/virtio/vulkan/` is empty — the venus source is present-but-inert). Build dirs: `build-kk`, `build-zink-kk`, prefix `zink-kk-prefix`. **`build-kk` must be configured `-Db_ndebug=true`** (keep `buildtype=debugoptimized`, so `-O2`/`-g` and crash-report symbolication are unaffected): with asserts compiled in, a guest's invalid Vulkan usage trips one on the vkr ring thread and SIGABRTs the worker, killing the VM — four incidents, see `limina-kk-empty-clear-rect`. `scripts/build-app.sh` refuses to bundle a dylib that still references `__assert_rtn`, because this is meson state on the sparse image that nothing in the repo pins. | **none** — the branch IS the delta (20 commits over base `178a3d739`); `patches/kosmickrisp/` retired 2026-08-04. NOT vendored by xtask (case-sensitive sparse image) + host-zink patches | `libvulkan_kosmickrisp.dylib`, zink `libEGL/libGL` |
| `third_party/libepoxy` + `epoxy-egl-prefix/` | — | EGL/GL dispatch the host zink-on-KK path needs. | — | prefix |
| `third_party/imago` | **fork model** `liminavm/imago` branch `limina`, pinned in `third_party/manifest.toml` | libkrun's virtio-blk backend (a crates.io dep), overridden via `[patch.crates-io]` in the **root Cargo.toml**. Discard→punch-hole fix. | **none** — the branch IS the delta | Rust dep |
| edk2 (no local tree required) | **fork model** `liminavm/edk2` branch `limina`, pinned in `third_party/manifest.toml` | GOP `KRUN_EFI` firmware for the EFI boot path (RELEASE; the krunkit blob is a DEBUG build that dead-loops on ASSERT — the #14 wedge). | **none** — the branch IS the delta (6 commits over `slp/edk2@krun-support`); `patches/edk2/` retired 2026-08-06 | `scripts/build-krun-efi.sh` (clones the pinned rev in its container volume) → `target/krun-efi/KRUN_EFI.gop.fd` — **the test suite's default firmware** |

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
| **Guest Mesa** (venus; guest GL is virgl/vrend since drop-guest-zink 2026-08-04) | **fork**: `github.com/liminavm/mesa`, branch `limina-guest`, base `mesa-26.1.5` (the Fedora SRPM base both tracks build); pinned in `third_party/manifest.toml [mesa-guest]`; worktree `/Volumes/mesa-cs/mesa-guest` | `mesa 26.1.5-6.limina.fc44` (LIMINA_REL=7 respin incoming, task #11) | 6 venus commits on the branch, exported by `scripts/export-mesa-guest-patches.sh` into the committed `patches/mesa-guest/` series (the old `patches/mesa/` pool is a tombstone) | `scripts/provision/f44/build-mesa-rpm.sh` (F44 build guest) / `scripts/build-mesa-rpm.sh` (F43, fc43 container) — both apply the whole series; dnf-versionlocked |
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

1. **`/Volumes/mesa-cs/mesa` is the HOST Mesa (KosmicKrisp + zink-on-KK), not the guest venus
   driver.** venus source *lives* in that tree but is unbuilt there and at a different version
   (main tip) than the guest ships (26.1.5). To change guest venus, commit on the
   **`limina-guest` branch (worktree `/Volumes/mesa-cs/mesa-guest`)** and re-export the series
   per `patches/mesa-guest/README.md`. venus only ever runs in the guest; KosmicKrisp only
   ever runs on the host.
2. **One mesa fork, two branches, two destinations.** `liminavm/mesa` carries the **host**
   KK/zink build on `limina-kk` (fork model, no patch directory) and the **guest** venus RPM
   delta on `limina-guest` (exported to `patches/mesa-guest/` for the RPM specs). Always
   check *which branch/build* a given mesa change targets.

*(Open to refine: exact virgl-prefix vs virgl-gl-prefix build-arg split; full host-zink
patch subset applied to `mesa-cs`. Add here as confirmed.)*
