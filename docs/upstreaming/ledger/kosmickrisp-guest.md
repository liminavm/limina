<!-- ledger-check: dep=kosmickrisp-guest -->
# kosmickrisp-guest — upstreaming ledger

`liminavm/mesa` `metal-native-context` branch (`third_party/manifest.toml [kosmickrisp-guest]`),
pin `41353793c95`, base `059715d2aa5` — the `[kosmickrisp]` (`limina-kk`) pin. The series is
therefore the host KosmicKrisp branch plus guest-only commits: the KosmicKrisp driver on the
limina native-context transport (the limina bridge) inside the guest, and the guest-side WSI and
loader changes it needs. Rows here cover only `base..rev`; the `limina-kk` commits underneath are
in `kosmickrisp.md`. Schema + protocol: `README.md`. Rows are keyed by SUBJECT.

Not vendored by `cargo xtask vendor`, for the same reason as `[kosmickrisp]` (Mesa needs a
case-sensitive filesystem): the working tree is `/Volumes/mesa-cs/mesa-metal-guest`, built by
`spikes/kk-on-wire/kk-build.sh` and installed into a guest by
`scripts/provision/deliver-metal-transport.sh`. There is no committed export; builds read the
tree.

No row has been researched against upstream yet; the dispositions are first guesses from the
commit messages and a read of the diffs (2026-10-16). One spot check: upstream `main`
`d44453ce07b` (2026-10-14) still prints MSL floats with `"%.*le"`
(`src/kosmickrisp/compiler/msl_type_inference.c:778`), the bug `41353793c95` fixes.

<!-- kosmickrisp-guest: 8 untracked of 8 in 059715d2aa53..41353793c950 -->
| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `4149cfb9bd4` | kk/limina: stop texture_transfer deadlocking when staging grows | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/limina/lmn_memory.c` |  | **carry** — the limina bridge exists only in this fork. sec: no (guest-side self-deadlock) |
| `17e8b2f12c7` | kk: put LINEAR on the dma-bufs a limina-bridge swapchain shares | `src/kosmickrisp/vulkan/kk_wsi.c` |  | **carry** — gated on a bridge that can share textures; a generic KK `supports_modifiers` opt-in could go upstream later. sec: no |
| `c3707c6d11a` | wsi/wayland: skip feedback tranches with no modifier the driver can use | `src/vulkan/wsi/wsi_common_wayland.c` |  | **upstream-now candidate** — common WSI code, any driver facing a modifier-less KMS scanout tranche (virtio-gpu); needs a repro with a non-KK driver. sec: no |
| `b935c0d6ef0` | kk/limina: let a signal behind a queue wait be exported without blocking | `src/kosmickrisp/bridge/limina/lmn_queue.c` |  | **carry** — bridge queue code; check whether upstream KK's `kk_binary_export_sync_file` has the same `vk_sync_wait(INFINITE)` fallback. sec: no |
| `a6a4793f449` | kk/limina: ask what the host speaks before creating a context | `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/limina/lmn_probe.h`, `src/kosmickrisp/bridge/limina/virtgpu_limina_drm.h` +1 |  | **carry** — depends on the fork kernel's `DRM_VIRTGPU_LIMINA_INFO` uapi. sec: **review** — an ioctl in the driver number range issued against render nodes, mitigated by checking the node is virtio-gpu first |
| `fa04568971b` | limina: a vendor build that installs beside the system Mesa | `meson.build`, `meson.options`, `src/egl/drivers/dri2/platform_drm.c` +5 |  | **carry** (limina packaging, `-Dlimina-vendor`), with a **carve-out**: the option-independent `platform_drm` hunk that checks a GBM device's backend name before reading it as a `gbm_dri_device` fixes an in-process type confusion and could go upstream alone. sec: low (no trust boundary) |
| `9d5b09a061f` | limina: a vendor build serves no client that names other drivers | `src/egl/main/eglglvnd.c`, `src/gallium/drivers/zink/zink_screen.c`, `src/gbm/backends/dri/gbm_dri.c` +2 |  | **carry** — vendor-build device policy. sec: no |
| `41353793c95` | kk: print MSL float literals with a '.' whatever the locale | `src/kosmickrisp/compiler/meson.build`, `src/kosmickrisp/compiler/msl_float.c`, `src/kosmickrisp/compiler/msl_float.h` +2 |  | **upstream-now** — generic KK bug still on upstream `main` (above); ships its own test. sec: no |
