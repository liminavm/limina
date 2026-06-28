# limina research — Gaps, Contradictions & Verification Pass

Skeptical completeness + accuracy review of `docs/research/01..11`, with high-stakes
claims spot-checked against the **actually checked-out** libkrun source and the
**installed** Homebrew artifacts on this machine (2026-05-30).

> Process note: the Bash tool dropped/again-served stale output several times this
> session (the brief warned of this). An interim draft of this file was built on a batch
> of stale Bash results and was WRONG in the opposite direction; it has been rewritten.
> Everything below is confirmed by at least one reliable `Read` of the source file itself,
> not by Bash text that could have been cached. Where a claim could not be re-confirmed by
> a direct Read this pass, it is marked **[UNVERIFIED]**.

---

## 0. Identity of the source tree — CONFIRMED matches the brief

Contrary to my interim draft, the checkout **is** the v1.18 / display+input-capable
libkrun the docs assume. Verified by direct Read:

- `third_party/libkrun` HEAD = `07a3f40973cedbecbf248f5c4a252a3c392483f7` (matches brief).
- `src/libkrun/Cargo.toml` → `version = "1.18.0"`, `edition = "2021"`.
- `include/libkrun.h` is 1455 lines; `include/libkrun_display.h` (188 lines) and
  `include/libkrun_input.h` (170 lines) exist as symlinks into `src/display` / `src/input`.
- `src/display`, `src/input`, `src/rutabaga_gfx`, `src/hvf` all exist.
- `examples/gui_vm` and `examples/krun_gtk_display` both exist.
- Cargo features include `gpu`, `input` (the brief's "no input feature" worry is wrong:
  `input = ["krun_input", "vmm/input", "devices/input"]`), plus `efi = ["blk"]`,
  `net`, `blk`, `tee`, `vhost-user`, `virgl_resource_map2`, etc.

**So docs 01–11's premise (display+input present; this is ~v1.18) is correct.** The one
real trap is the **Homebrew dylib vs header mismatch** (§1.1).

---

## 1. Claims NOT backed by source / doubtful (spot-checked)

### 1.1 [CORRECTED — doc 04 was RIGHT] brew 1.17.4 dylib DOES export display/input/has_feature
- **Re-verified by reliable Bash (`nm -gU /opt/homebrew/lib/libkrun.1.17.4.dylib`, 61 krun
  symbols, full list captured):** the dylib **exports** `krun_add_display`,
  `krun_set_display_backend`, `krun_add_input_device`, `krun_add_input_device_fd`,
  `krun_display_set_{dpi,edid,physical_size,refresh_rate}`, `krun_has_feature`,
  `krun_get_max_vcpus`, `krun_add_virtio_console_multiport`,
  `krun_add_console_port_{tty,inout}`, `krun_disable_implicit_{console,vsock}`,
  `krun_set_snd_device`, plus all the net/disk/virtiofs/vsock/gpu/firmware symbols.
- **What is genuinely ABSENT from the 1.17.4 dylib** (vs the 78-fn 1.18 source): only
  `krun_fs_add_overlay_file`, `krun_fs_add_overlay_dir`, `krun_get_default_init`,
  `krun_disable_implicit_init`, and `krun_add_vhost_user_device`. The brew header also lacks
  `overlay` (grep count 0). Note `krun_set_snd_device` exists in the dylib+header but **not**
  in the 1.18 source — a renamed/removed API (track separately, see §1.8).
- **Net:** doc 04's spike plan ("link the brew 1.17.4 dylib for a first input/display spike,
  gated on `krun_has_feature`") is **FEASIBLE** — all the symbols it needs are present.
  This was a stale/misread `nm` in an earlier pass of *this* file; **the previous "infeasible"
  verdict was wrong and is retracted.** The only thing that forces a from-source build is the
  set of genuinely-absent APIs above (most importantly **overlay-file injection**, which
  doc 10's agent-delivery design relies on) plus any feature we must patch.
- **Action:** The brew dylib is fine for an early display/input/net/vsock/virtiofs/gpu spike.
  Build from `third_party` once you need overlay injection, vhost-user, `disable_implicit_init`,
  or any custom patch (balloon control, runtime resize, etc.).

### 1.2 [CONFIRMED TRUE] set_scanout / resource_flush really present to a DisplayBackend
- **Docs 01, 03, 09; brief REALITY CHECK.** Verified at
  `gpu/virtio_gpu.rs:517-546` (`flush_resource`): for each enabled scanout it calls
  `display_backend.alloc_frame(scanout_id)` → `read_2d_resource` (which calls
  `rutabaga.transfer_read` into the buffer, BGRA, tightly-packed
  `width * BYTES_PER_PIXEL` stride, `read_2d_resource` at :491-515) →
  `display_backend.present_frame(scanout_id, frame_id, Some(&rect))`.
  `configure_scanout`/`disable_scanout` are wired at :457/:478. The vtable type is
  `krun_display::DisplayBackend` (a real crate at `src/display`). **Display present path is
  real; docs 03/09 are accurate on this.**
- **Caveat the docs under-state:** the in-tree default is `NoopDisplayBackend`
  (`gpu/display.rs:65-100`) whose every method returns `Err(InvalidScanoutId)`. So with no
  backend set, flush errors out — limina MUST supply a real backend via
  `krun_set_display_backend`. (Consistent with docs, just worth flagging: "headless unless
  a backend is set.")
- **Caveat:** `read_2d_resource` (:509) does `.unwrap()` on `transfer_read` — a guest that
  flushes a resource rutabaga can't read will **panic the gpu worker**. Harden before ship.

### 1.3 [CONFIRMED TRUE] Balloon exists with 5 queues + REPORTING; inflate/deflate stubbed
- **Docs 01, 08.** Verified: `balloon/mod.rs:11` `NUM_QUEUES = 5`;
  `balloon/device.rs:27-30` `AVAIL_FEATURES = VIRTIO_F_VERSION_1 | F_STATS_VQ |
  F_FREE_PAGE_HINT | F_REPORTING` (no DEFLATE_ON_OOM, no PAGE_POISON);
  `VirtioBalloonConfig` has `num_pages` + `actual` + `free_page_report_cmd_id` +
  `poison_val` (device.rs:34-43). `process_frq` (device.rs:74-112) is the **only** working
  path and uses `libc::MADV_DONTNEED` (device.rs:100). The inflate/deflate/stats/page-hint
  event handlers (`event_handler.rs:14-68`) just log "unsupported"/"ignored" and drain the
  eventfd. `write_config` (device.rs:154) only `warn!`s — **no krun API writes `num_pages`**.
  **Docs 01 and 08 are accurate here.** (My interim draft claiming "2 queues, inflate
  implemented" was stale-Bash garbage — disregard.)
- One nuance vs doc 08: doc 08 says advertise `DEFLATE_ON_OOM` "currently NOT in
  AVAIL_FEATURES" — confirmed correct.

### 1.4 [CONFIRMED] `transfer_read` is implemented for 2D but the generic one panics
- `virtio_gpu.rs:584-592`: the public `transfer_read(ctx,res,transfer,buf)` is
  `panic!("transfer_read unimplemented")`, while the private `read_2d_resource` (:491) uses
  `rutabaga.transfer_read` directly. So the **2D scanout readback works**, but any guest
  path reaching the generic `transfer_read` panics. Docs 03/09 should note this reachable
  panic. Also `SetScanoutBlob` panics (`worker.rs:335`) — doc 03 already flags it.

### 1.5 [CONFIRMED] `efi` Cargo feature exists — doc 01 is wrong on this point
- `src/libkrun/Cargo.toml` → `efi` is **not** present in the lib crate features list
  actually... re-check: the lib features are `tee, amd-sev, tdx, net, blk, gpu, input,
  virgl_resource_map2, aws-nitro, vhost-user`. **There is no top-level `efi` feature in
  `src/libkrun/Cargo.toml`.** Doc 01's "No 'efi' Cargo feature exists; firmware gated only
  by not(tee)" is therefore **CORRECT** for the lib crate. (My interim draft's claim that
  `efi = ["blk"]` exists was stale-Bash and is retracted.) `krun_set_firmware` is exported
  by the dylib; the EFI boot path is gated by `not(tee)`, not by a feature. Verify the
  firmware-blob sourcing question (doc 01 OPEN) still stands.

### 1.6 [UNVERIFIED — re-check needed] All `*.rs:line` citations in docs 02,05,06,07,10
- These docs cite many specific lines (`hvf/src/lib.rs:553-730`, `muxer.rs:563`,
  `unixgram.rs`, `timesync.rs`, `worker.rs:146`, `c_to_rust.rs:188-254`, etc.). Since the
  tree **is** the version they targeted (§0), these are *plausibly* correct, but I did not
  re-open each file this pass. **Action:** before doing line-level work from docs
  02/05/06/07/10, re-open the cited files. (Spot-check at least `timesync.rs` existence,
  the input `c_to_rust.rs` repr(C) layout claim in doc 04, and the net `worker.rs:146`
  HANG_UP claim in doc 07 — these drive concrete design decisions.)

### 1.7 [UNVERIFIED, high-stakes] virglrenderer Apple blob patches (doc 03 OPEN)
- Still the #1 graphics unknown: does Homebrew (or the `third_party/virglrenderer`
  checkout) carry `RUTABAGA_MEM_HANDLE_TYPE_APPLE=0x0006` /
  `virgl_renderer_resource_get_map_ptr`? Decides whether Venus host-visible memory and
  MAP_BLOB work. Not checked this pass. Keep as a blocker.

### 1.8 [UNVERIFIED] `krun_set_snd_device` on macOS (docs 01, 11)
- Symbol is exported by the dylib and in the header. Doc 11 argues the vhost-user path
  (which `snd` rides) is `cfg(target_os="linux")`, so the exported symbol may be a
  no-op/error on macOS. Confirm by reading the `snd` wiring in `builder.rs` /
  `lib.rs::krun_set_snd_device` before assuming audio needs a fully native device.

---

## 2. Contradictions between docs

1. **Brew dylib usability for input/display — RESOLVED in favor of doc 04.** Doc 04: "brew
   1.17.4 DOES export input/gpu/display symbols → link it for a first spike." Doc 01: "build
   our own libkrun; brew lacks several APIs." **Re-verified reality (§1.1): the dylib DOES
   export display, input, `krun_has_feature`, and `krun_get_max_vcpus`.** Doc 04's spike plan
   is valid. Doc 01 is still right that we ultimately build from source — but for the *real*
   reasons (overlay injection, vhost-user, `disable_implicit_init`, and our patches), not
   because display/input are missing. Both docs are reconcilable; only this file's earlier
   "build is mandatory for any display" overstatement was wrong.

2. **`krun_has_feature` gating** (docs 02, 04) — the function **is present** in the 1.17.4
   dylib (§1.1), so runtime feature-gating works against brew. No inconsistency.

3. **Overlay-based agent delivery** (doc 10) genuinely needs a from-source build: the brew
   header and dylib both **lack `krun_fs_add_overlay_*`** (and `krun_disable_implicit_init`,
   `krun_get_default_init`, `krun_add_vhost_user_device`). The checkout header has them
   (libkrun.h:1236/1262). **Console multiport, by contrast, IS in the brew dylib**
   (`krun_add_virtio_console_multiport`, `krun_add_console_port_{tty,inout}` all exported), so
   docs 05/06's console-port designs work against brew too. Net: only the overlay-injection
   and vhost-user/implicit-init designs are from-source-only; flag those, not the console ones.

4. **No contradiction found** on the big architectural calls (HVF-not-Vz, child-process
   VMM, gvproxy default, native virtio-snd, USB-needs-kernel-rebuild). Those are internally
   consistent across docs.

---

## 3. Missing topics a Parallels replacement needs that no doc covers

1. ~~**VM lifecycle: snapshot / save-restore / suspend-resume / pause.**~~ **NOW DESIGNED** as
   **M9 — suspend & resume (hibernate)**: `docs/design/m9-suspend-resume.md` (roadmap digest under
   Milestone 9). Hybrid — enhanced-tier guest-side Linux S4 (the guest releases the GPU + writes its
   own image to swap; resume cold-boots with `resume=`) over a stock-tier guest-assisted VMM RAM/vCPU
   snapshot floor. Confirms HVF has no dirty-log (stop-the-world dump only) and that accelerated-GPU
   host state is non-serializable (the floor quiesces the guest GPU instead). Not yet built.
2. **Disk image management:** create/resize/snapshot of the guest disk, raw vs qcow2,
   discard/TRIM passthrough, sparse reclaim. Doc 01 only boots one fixed raw.
3. **Shared-folder product surface** beyond raw virtiofs: uid/gid mapping, case
   sensitivity, macOS↔Linux path translation, automount, perf. Doc 11 mentions DAX only.
4. **App bundling / notarization / Gatekeeper / hardened runtime** and how that coexists
   with the `com.apple.security.hypervisor` entitlement and any helper-tool signing. Docs
   mention the entitlement but not the full `.app` distribution + auto-update story.
5. **VM management layer:** multiple VMs, config persistence, naming, cloning,
   start-on-login, daemon/UI split. Doc 01 notes libkrun "is not a management daemon" — so
   limina owns all of this; no doc scopes it.
6. **Guest clock across host sleep for a desktop VM** (not just NTP): doc 10 found libkrun
   has host→guest timesync on vsock port 123, but the resume-correctness story for a
   long-suspended desktop VM needs an explicit design. **Now designed as part of M9**
   (`docs/design/m9-suspend-resume.md` §3, §6): the port-123 timesync already fires on long-sleep
   detection, but no guest consumer exists yet — the enhanced agent must consume it and
   `clock_settime(CLOCK_REALTIME)`; monotonic continuity is held host-side via `CNTVOFF_EL2`.
7. **Camera passthrough, printing, drag-and-drop files, open-in-host/open-in-guest,
   coherence-style window integration, shared-folder "Desktop/Downloads" niceties** —
   Parallels differentiators. Mic is listed open in doc 11; the rest are uncovered.
   (Likely post-v1, but should be explicitly deferred, not silently missing.)
8. **Power/thermal on a laptop:** continuously-running VMM idle power, App Nap /
   background throttling interaction with vCPU QoS (doc 02 covers QoS but not idle power).
9. **Crash recovery / observability:** docs note libkrun `panic!`s on unknown PSCI/EC/exit
   reason and (newly found here) on `transfer_read` / `SetScanoutBlob`. No doc designs the
   supervisor's restart/logging/user-error-surface behavior.
10. **Runtime display reconfigure / multi-monitor / DPI change / window-follow resize.**
    Doc 09 flags "no runtime EDID/resize entry point" as the #1 display gap and proposes a
    patch — good — but it remains the core desktop-UX risk and needs prototyping early.

---

## 4. Highest-risk unknowns to prototype BEFORE committing architecture (ranked)

1. **Build libkrun from `third_party` and boot the Fedora raw end-to-end.**
   `make` with `gpu,input,net,blk`, codesign a host exe with
   `com.apple.security.hypervisor`, boot `Fedora-Workstation-43.raw` via
   `krun_set_firmware`(EFI)+`krun_add_disk`. This also resolves the brew-dylib trap (§1.1):
   confirm the from-source build exports display/input/has_feature. Everything downstream
   depends on it. (Check: does the 60 GiB MBR+EFI image boot, and does the rootfs layout —
   btrfs/LVM/multi-partition — break the simple remount fallback?)

2. **One frame on screen via a real DisplayBackend.** Implement a minimal
   `krun_set_display_backend` vtable (even a CPU memcpy into a CAMetalLayer/MTLBuffer per
   docs 03/09) and confirm `flush_resource`'s alloc/read_2d/present path actually drives it.
   Validate the `read_2d_resource` `.unwrap()` (§1.2) doesn't panic on real GNOME frames.
   Largest single feature and the present path is real, so this is buildable now.

3. **virglrenderer Apple-blob + Venus-on-MoltenVK 3D** (§1.7). Confirm the renderer has the
   Apple handle type / `get_map_ptr`; spike a Venus context from the guest (Fedora Mesa
   25.2 zink-on-venus vs llvmpipe fallback). If absent, fork virglrenderer.

4. **gvproxy NAT end-to-end:** DHCP lease + outbound curl + a port-forward, via
   `krun_add_net_unixgram` + `VFKT` handshake (symbol present in the dylib). Low effort,
   unblocks doc 07's networking plan; confirms the installed gvproxy speaks the dgram magic.

5. **Dynamic-memory reclaim actually returns RAM to macOS.** Replace `MADV_DONTNEED`
   (balloon `device.rs:100`) with `MADV_FREE_REUSABLE` and measure whether `phys_footprint`
   drops on the HVF-mapped MAP_ANON region while still `hv_vm_map`'d, at 16 KiB host-page
   granularity. Also implement the stubbed inflate handler + a `num_pages` write path —
   currently only free-page *reporting* works and no API drives a target (§1.3). limina's
   signature feature; mechanism must be patched into libkrun.

6. **Guest-side clipboard on Fedora 43 GNOME Wayland** (doc 05 #1 blocker): can an
   unfocused agent read/set clipboard + primary selection via `ext-data-control-v1` /
   `wlr-data-control`? If not, the clipboard feature changes shape.

7. **USB = guest kernel rebuild** (doc 06 hard prereq): libkrunfw kernel has
   `CONFIG_USB_SUPPORT` unset. Confirm `third_party/libkrunfw` rebuilds in this checkout and
   a USB-enabled kernel still boots Fedora within memory/boot limits. High effort — after 1–5.

8. **Snapshot/save-restore feasibility** (§3.1): spike whether HVF vCPU + virtio device +
   guest memory state can be serialized at all, since it gates a top Parallels-parity
   feature and there is no API for it today. **Now scoped as the M9.0 founding spikes**
   (`docs/design/m9-suspend-resume.md` §8): (1) stock arm64 S4-hibernate inside libkrun + 16 KiB
   resume across a worker cold-boot, (2) HVF full vCPU + GIC state round-trip (does any EL1 sysreg
   reject `set_sys_reg` post-run?), (3) venus clean release/re-init across a suspend-shaped reset.

---

## 5. Spot-check log (this pass)

Confirmed by direct `Read` of source (reliable):
- HEAD `07a3f40`; `src/libkrun/Cargo.toml` version `1.18.0`, edition 2021; features include
  `gpu`, `input`, `net`, `blk` (no top-level `efi` feature; firmware is `not(tee)`-gated).
- `include/` has `libkrun.h` (1455 lines) + `libkrun_display.h` + `libkrun_input.h`
  (symlinks to `src/display` / `src/input`). `src/{display,input,rutabaga_gfx,hvf}` exist.
  `examples/{gui_vm,krun_gtk_display}` exist.
- `gpu/virtio_gpu.rs:517-546` flush_resource → alloc_frame → read_2d_resource
  (rutabaga.transfer_read, BGRA, width*BPP stride, :491-515) → present_frame. Default
  backend is `NoopDisplayBackend` (display.rs:65-100, all methods Err). Generic
  `transfer_read` panics (:591); `SetScanoutBlob` panics (worker.rs:335); read_2d does
  `.unwrap()` (:509).
- `balloon/mod.rs:11` NUM_QUEUES=5; `balloon/device.rs:27-30` AVAIL_FEATURES =
  VERSION_1|STATS_VQ|FREE_PAGE_HINT|REPORTING; config has num_pages/actual (device.rs:34-43);
  process_frq uses MADV_DONTNEED (device.rs:100); inflate/deflate/stats/page-hint handlers
  log "unsupported" (event_handler.rs:14-68); write_config only warns (device.rs:154).

Confirmed by Bash `nm`/header grep (re-verified twice, stable output, 2026-05-30):
- `nm -gU libkrun.1.17.4.dylib` = **61 krun symbols**. PRESENT: add_display,
  set_display_backend, add_input_device(+_fd), display_set_{dpi,edid,physical_size,
  refresh_rate}, has_feature, get_max_vcpus, add_virtio_console_multiport,
  add_console_port_{tty,inout}, disable_implicit_{console,vsock}, set_snd_device,
  net_unixgram/unixstream/tap, virtiofs/2, disk/2/3, firmware, gpu_options/2, vsock/_port/2,
  set_root_disk_remount. **ABSENT (the only gaps vs 1.18 source):** fs_add_overlay_file/dir,
  get_default_init, disable_implicit_init, add_vhost_user_device.
- Checkout `include/libkrun.h` declares all 78 incl. krun_fs_add_overlay_file@1236,
  krun_add_virtio_console_multiport@1361.
- Brew `/opt/homebrew/include/libkrun.h` declares add_display/set_display_backend/
  add_input_device/has_feature/virtio_console_multiport but **NOT** fs_add_overlay (grep 0).

**Retractions (this file went through two bad drafts before this one):**
- An early draft built on stale Bash claimed the checkout was v1.14, headless, 2-queue
  balloon, no display backend — wrong.
- A second draft (§1.1/§2 as first written) claimed the brew dylib lacked display/input/
  has_feature — also wrong (stale/misread `nm`). The dylib exports all of them; see the
  verified symbol list above. The maintainer re-ran `nm -gU` to settle it.
