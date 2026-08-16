# Graphics-stack documentation audit — claims re-verified against running VMs

**Date:** 2026-08-16. **Purpose:** the graphics docs and agent memories had accumulated a
chronological journal of every fix, several layers deep, with load-bearing claims that nobody had
re-tested since the stack moved under them. This pass re-derived the current state **from booted
VMs**, not from the notes, so that `docs/graphics.md` could be written from measurement.

Everything below was observed on this host on 2026-08-16 unless dated otherwise.

## Rig

| | |
|---|---|
| Host | macOS 26.5, Apple M1 Max, 32 GB, 16 KiB pages |
| Worker | `target/debug/limina-vmm`, `cargo xtask build`, linked `third_party/virgl-prefix/lib/libvirglrenderer.1.dylib` (verified `otool -L`) |
| Boot | `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` (EFI → GRUB → BLS, coexist GPU, `--window --net`), 6 vCPU / 8 GiB |
| Enhanced guest | `gfxaudit.raw`, CoW clone of `Fedora-Workstation-44.enhanced.raw` |
| Stock guest | `gfxaudit-stock.raw`, CoW clone of `Fedora-Workstation-44.stock.test.raw` |
| Vulkan-compositor guest | `gfxsynoik.raw`, CoW clone of `Fedora-Workstation-44.enhanced.synoik.raw` — the only vehicle that reaches venus present (row 8a) |
| Software-2D differential | `gfxsw2d.raw`, CoW clone of `Fedora-Workstation-44.enhanced.raw`, booted with `--gpu-software-2d` (row 12a) |

## The claim matrix

Legend: **CONFIRMED** = measured true · **FALSIFIED** = measured false · **REFINED** = true only
under a condition the source omitted.

### Host / device

| # | Claim | Source | Verdict |
|---|---|---|---|
| 1 | The default device is coexist: `GPU_COEXIST_FLAGS = 0x35b` = `VENUS｜USE_EGL｜USE_GLES｜USE_SURFACELESS｜THREAD_SYNC｜ASYNC_FENCE_CB｜RENDER_SERVER`, `NO_VIRGL` off | `docs/tiers.md` | **CONFIRMED** — `crates/limina-vmm/src/krun/mod.rs:109-115` |
| 2 | "The worker logs the resolved mode on every boot" (`virtio-gpu virgl_flags = 0x35b …`) | `docs/tiers.md` | **REFINED** — it is `log::info!` (`krun/mod.rs:285`) and the worker defaults to `warn`, so a default boot does **not** print it. Needs `RUST_LOG=info`. |
| 3 | The worker must link our virglrenderer prefix or the GPU silently degrades to software-2D | `limina-virgl-link-trap` | **CONFIRMED** as a live guard — `otool -L` shows `third_party/virgl-prefix/…`; `build.rs` prepends the prefix. |
| 4 | Host GPU-memory cap default = `max(8 GiB, 2 × guest RAM)` | `limina-gpu-mem-budget` | **CONFIRMED** — worker log: `limina GPU budget: cap 16384 MiB` at 8 GiB guest RAM. |
| 5 | "Only ONE HVF VM at a time on this host" | `limina-venus-ghost-tombstone` | **FALSIFIED** — two guests (enhanced on ssh 2222, stock on 2223) ran concurrently for the whole session, both with live 3D. |

### Scanout / present

| # | Claim | Source | Verdict |
|---|---|---|---|
| 6 | vrend renders **directly into** the display IOSurface (EGLImage-backed scanout, zero blits) | `docs/design/vrend-iosurface-scanout.md` (shipped 2026-08-04) | **CONFIRMED** — worker log, both tiers: `iosurface scanout: 2560x1440 PIPE_FORMAT_B8G8R8X8_UNORM EGL-backed (IOSurface id N), renders land in the surface directly` |
| 7 | Zero-copy scanout is enhanced-tier only | implied by the old tier docs | **FALSIFIED** — the **stock** guest gets the identical EGL-backed scanout. It is a host-side property of vrend, independent of guest tier. |
| 8 | Present is "a full-frame CPU readback per flush", `SET_SCANOUT_BLOB` panics, "there is no zero-copy scanout of a GPU texture" | `docs/research/03-graphics-virtio-gpu-3d.md` | **FALSIFIED** — obsolete by ~2 months; both venus and vrend present zero-copy. The whole document predates the KosmicKrisp switch. |
| 8a | The **venus** present path (`SET_SCANOUT_BLOB` + MTLTexture import) works | `docs/graphics.md` §4 | **CONFIRMED — measured 2026-08-16 on the synoik image.** A GNOME guest structurally cannot test this (mutter composites on vrend, so its silence is a false negative — the `limina-synoik-image` lesson), so this needed its own boot: `Fedora-Workstation-44.enhanced.synoik.raw`, kernel `7.1.8-limina16k`, synoik up on `seat0`. Worker log: `[LIMINA-VKR-MTLTEX] scanout memory <- MTLTEXTURE import of IOSurface id=135 (tex=0xaef328780)` (and id=137). Desktop **human-confirmed correct** in the window — no shear, no garbage. Corroborated by the L2 test `synoik_session_reaches_a_rendered_desktop`, green in 28 s on 2026-08-15. |

### Guest tiers

| # | Claim | Source | Verdict |
|---|---|---|---|
| 9 | Enhanced session GL rides **vrend**, Vulkan rides **venus** | `docs/tiers.md`, `limina-baseline-3d-plan` | **CONFIRMED** — `gnome-shell` maps `libgallium-26.1.6.so` and no venus ICD; `GL_RENDERER = virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))`; `vulkaninfo` = `Virtio-GPU Venus (Apple M1 Max)`, `driverName = venus` |
| 10 | A bone-stock 4 KiB guest gets accelerated GL with **no guest install** | `docs/tiers.md` tier 2 | **CONFIRMED** — stock F44 (kernel `6.19.10-300.fc44`, 4096-byte pages, stock mesa `26.0.3-4.fc44`, no limina env at all): `OpenGL renderer string: virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))`, GL 3.2 compat |
| 11 | KosmicKrisp is at **Vulkan 1.3** | `docs/tiers.md`, several memories | **FALSIFIED** — every renderer string now reads `zink Vulkan 1.4`; KK went 1.4 at the 2026-08-05 MTL4 rebase and the tier doc was never updated. |
| 12 | venus needs a 16 KiB guest | `docs/tiers.md` tier 3 | **CONFIRMED** on a bone-stock guest, and the exact mechanism was captured live (row 13). |
| 12a | Software-2D (`--gpu-software-2d`) is a usable llvmpipe desktop tier; on the enhanced image it shows only a blinking cursor "by design" | `docs/tiers.md` tier 1, `limina-tier2-venus` | **BOTH TRUE — the tier depends on the GUEST, and the cause is ours.** On a **stock** guest the floor holds: `boot.rs::fedora_stock_image_software_2d_floor_renders_desktop` asserts a usable GNOME desktop on this device and is green in the suite. On an **enhanced** guest it does not: `/dev/dri/card0` + `renderD128` exist and `drm_info` reports a full mode list, but mutter logs `egl: failed to create dri2 screen` → `Failed to create gbm device: No such file or directory` → `Failed to setup: No GPUs found`, `org.gnome.Shell@gdm.service: Failed with result 'protocol'`, and gdm gives up. **Clean differential run 2026-08-16:** same image, same device — `mv /etc/environment.d/90-limina-zink.conf` aside, `systemctl restart gdm`, and a greeter appears on `seat0` within 45 s. The cause is our own `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` + `GALLIUM_DRIVER=virgl` pinning mesa to a driver that needs the 3D device. An earlier draft of this row generalised the enhanced result into "software-2D is not a desktop tier" — wrong, and it would have justified removing a flag the compatibility floor depends on. **Disposition: enhanced + software-2D is an UNSUPPORTED configuration, wontfix (user, 2026-08-16)** — the enhanced tier exists to use the 3D device, and the floor that must survive a GL-less host is the stock tier, which does. |
| 13 | "venus works on STOCK 4k F44" | `limina-blob-map-16k-alignment` description | **REFINED / misleading as written** — true only *with* `guest/virtio-gpu-dkms` installed, which no shipped image or payload carries. On the stock image as it ships, venus does **not** work: `hv_vm_map failed: ret=0xfae94003 host=… guest=0x280021000 size=0x100000 (host%16k=0 guest%16k=4096 size%16k=0)` → `ResourceMapBlob -> ErrUnspec`. The **offset** half of the 16k/4k wall, exactly as `docs/design/16k-page-requirement.md` describes. |
| 14 | On a stock guest "Vulkan has only lavapipe" (i.e. it degrades) | `docs/tiers.md` §Tier selection | **FALSIFIED** — stock-tier Vulkan is **dead, not degraded**: `vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY`. Isolating the ICDs proves the loader is the amplifier: `VK_DRIVER_FILES=…/lvp_icd…` alone → `llvmpipe` enumerates fine; `…/virtio_icd…` alone → the OOM. venus's hard failure takes healthy lavapipe down with it. `docs/design/16k-page-requirement.md` §"shopping list" item 1 already says this and is the correct source; `tiers.md` contradicted it. |
| 15 | Guest components on the enhanced image match `docs/images.md` §Component versions | `docs/images.md` | **CONFIRMED** — kernel `7.1.8-limina16k` (16384-byte pages, `7.1.4`/`7.1.6-2` co-installed), mesa `26.1.6-1.limina.fc44`, mutter `50.1-1.limina.fc44`, gnome-shell `50.0-1.fc44` |
| 16 | Post-r9 the guest kernel advertises no format modifiers | `docs/images.md` | **CONFIRMED** — `DRM_CAP_ADDFB2_MODIFIERS = 0`, primary plane formats `XR24` only (cursor plane `AR24`) |

### The verification recipes themselves

| # | Claim | Source | Verdict |
|---|---|---|---|
| 17 | "`vulkaninfo` over ssh enumerates **nothing**; an empty ssh `vulkaninfo` is a false negative" | `limina-boot-default-efi-venus`, `CLAUDE.md`, `limina-gpu-mem-budget` | **FALSIFIED as an unconditional rule.** On the F44 enhanced image with the desktop seated, a plain non-login `ssh … vulkaninfo` reports `Virtio-GPU Venus (Apple M1 Max)`, and `echo $VK_DRIVER_FILES` in that same shell returns the value from `/etc/environment.d/90-limina-zink.conf`. It also enumerates venus with `VK_DRIVER_FILES` explicitly unset, because the venus ICD is a normal entry in `/usr/share/vulkan/icd.d/`. Mechanism not pinned down (systemd 259 / pam 1.7.2; the user manager does hold the vars — `systemctl --user show-environment` shows them). **Rule that replaces it: print the variable in the shell you are actually using, and treat an *empty* result as "check the env", not as "venus is broken".** |
| 18 | `~/bin/set-guest-display.py` exists in the guest for pinning the display before a benchmark | `docs/tiers.md` §geometry trap | **FALSIFIED** on the current `enhanced.raw` — the file is absent. Ship it or stop citing that path. |
| 19 | The ledger workload `glmark2-wayland-venus` measures "zink→venus" GL | `perf/README.md` | **FALSIFIED (labelling)** — since the 2026-08-04 GL flip the same command runs on **vrend**. The row name and its description have been measuring a different tier than they claim for twelve days. |

### Performance

The tier ranking in `docs/tiers.md` §Performance is not reproducible as written and its virgl row
is dead. Measured here (enhanced guest, 2560×1440 window, display **not** pinned — so these are
orientation, not ledger rows):

| workload | result |
|---|---|
| `glmark2-es2-wayland -b build:duration=3` (GL → vrend → zink-on-KK) | **3942 fps / score 3941** |
| `vkmark -b vertex -b texture` (Vulkan → venus → KK) | **3319 / 3320 fps, score 3319** |

For contrast the doc's table has virgl at **57** ("vsync-limited"), venus at **2019** and llvmpipe
at **342**, and concludes "venus is the tier to be on". Neither the number nor the conclusion
survives: GL on vrend is not frame-capped here and is the tier the session actually runs on. The
ledger's last recorded `glmark2-wayland-venus` was 2931–2972 on 2026-08-09.

**Do not lift the two numbers above into the ledger** — the display was unpinned, which is the trap
`limina-perf-display-pinning` exists for. They are sufficient to retire a stale ranking, not to
replace it.

### Tearing (human oracle, 2026-08-16)

| # | Claim | Source | Verdict |
|---|---|---|---|
| 20 | vrend has no fence-accurate present (`try_park_present` unreached, `FENCEPRESENT` never fires), so the #24 tear/pacing arc does not cover the tier the desktop runs on | `docs/hardening-backlog.md` | **MECHANISM UNCHANGED — SYMPTOM ABSENT.** The user stress-tested the GNOME overview in and out repeatedly on **both** present paths and reports it smooth, no tearing: the vrend path (`gfxtear.raw`, a clone of `enhanced.raw`; `gnome-shell` maps `libgallium-26.1.6.so` and no venus ICD; scanout `2560x1440 … EGL-backed (IOSurface id 175)` at `2560×1440@59.99`) and the venus path (`gfxsynoik.raw`, MTLTEXTURE import). Overview toggling is the workload the user identifies as historically the most tear-prone, so this is the right stress, not an easy one. **What this does NOT show:** the mechanism gap is read from source and was not re-tested — `FENCEPRESENT` is absent from both logs, but those DIAGs are `RUST_LOG=trace`-gated and both workers ran at the default `warn`, so their absence is not evidence. The honest state moves from "open, tearing risk" to "mechanism absent, no observable symptom on this rig". |

## New observations the docs did not have

- **vrend refuses `CREATE_VIDEO_BUFFER`**, on both tiers, at every boot:
  `context N failed to dispatch CREATE_VIDEO_BUFFER: 22` → `vrend_decode_ctx_submit_cmd: context
  error reported N "gst-plugin-scan" Illegal command buffer`. Benign (the victim is a throwaway
  probe context, per `limina-vrend-context-poison`), but it is permanent boot noise that reads as
  an error, and it is worth having written down so the next reader does not chase it.
- **`docs/design/drm-format-modifier-for-real.md` still says "design, pre-code" for work that
  shipped.** The tree disagrees with its own doc: `/Volumes/mesa-cs/mesa/src/vulkan/runtime/vk_image.h:80`
  now reads `#if DETECT_OS_LINUX || DETECT_OS_BSD || DETECT_OS_APPLE` (the guard the doc calls a
  "modest carry"), `src/kosmickrisp/vulkan/kk_{image,image_layout,format,physical_device}.c` all
  carry modifier code, and the exported guest series (`patches/mesa-guest/`) is down to eight
  patches with **no 0010** — exactly the deletion the doc set out to earn. `docs/upstreaming/ledger/mesa.md`
  already records it as `RETIRED WHOLE 2026-08-04`. The design doc is the only thing still claiming
  otherwise.
- **The stock tier's Vulkan death (row 14) is a live two-tier-guarantee violation**, not a
  historical note: "degrade gracefully" is exactly what does not happen. The fix is already
  identified and queued (`docs/upstreaming/ledger/mesa.md`, the venus stub-instance patch).

## How to re-run this

```sh
cargo xtask build                                   # builds + signs + checks the virgl link
cp -c Fedora-Workstation-44.enhanced.raw gfxaudit.raw
LIMINA_DISK=$PWD/gfxaudit.raw LIMINA_BOOT_LOG=/tmp/gfxaudit.log \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &
# read the port from the log: "guest SSH forward ready: ssh -p N …"
```

Then in the guest: `uname -r`, `getconf PAGE_SIZE`, `rpm -q limina-kernel-16k mesa-vulkan-drivers`,
`vulkaninfo --summary`, `sudo drm_info`, and
`systemd-run --user --wait --pipe --collect --setenv=WAYLAND_DISPLAY=wayland-0 glmark2-es2-wayland -b build:duration=3`
for a renderer string from inside the seated session. Repeat against
`Fedora-Workstation-44.stock.test.raw` for the baseline tier.
