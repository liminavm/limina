# Guest images

The source of truth for the guest disk images limina develops and tests against (Fedora unless a
section says otherwise):
what each one is, which **tier** it exercises, whether it's pristine or modified, and how it's
produced/refreshed. All images live in the repo root and are **gitignored** (`*.raw`, `*.raw.xz`)
— they're large (10–22 GB real on disk) and reproducible, so they're never committed.

> Memory note: this file supersedes the image inventory that used to live only in agent memory
> (`limina-fedora-access`), which drifted out of date. Update **this file** when the image set
> changes.

## Conventions

- **Never boot a pristine image directly.** A writable root is required to reach a desktop, and we
  keep the pristine copy untouched. Boot a **CoW clone** instead: `cp -c SRC.raw CLONE.raw` is an
  instant APFS copy-on-write (shared blocks, no extra space until written). The run scripts
  (`run-fedora-window.sh`, `run-enhanced.sh`) clone automatically.
- **APFS CoW means `du` lies.** Clones share unchanged blocks, so the per-file "real" sizes
  double-count shared data; deleting a clone only frees the blocks it *uniquely* owns. Don't expect
  freed space to equal the listed size.
- The worker (`limina-vmm`) must be codesigned for HVF before any boot: `crates/limina-vmm/sign.sh debug`.
- **Release selector + naming.** The set is mirrored per Fedora release under a uniform scheme
  `Fedora-Workstation-<REL>.<role>.raw`, roles: `vanilla` (pristine), `accessible` (stock base +
  user/autologin), `stock.test` (frozen stock-tier L2 snapshot), `enhanced` (venus base),
  `enhanced.test` (frozen enhanced-tier L2 snapshot). **`LIMINA_FEDORA_REL=43|44`** picks the
  release for the run scripts (`run-fedora-window.sh`, `run-enhanced.sh`,
  `run-venus-window.sh`) and the L2 harness (`crates/limina-test`); per-image overrides
  (`LIMINA_TEST_DISK`, `LIMINA_TEST_DISK_ENH`, `LIMINA_TEST_DISK_BASELINE`) still win. This is the
  template for future releases (F45): produce the five roles, flip the selector.
  **The L2 harness default moved `43` → `44` on 2026-08-15** (the run scripts still default to 43).
  F44 is the family the guest components are built for, and the F43 pair had drifted a release
  behind (task #31), so the suite was certifying stale guests — concretely, F43's 6.12 kernel has
  no `uinput`, which `spice-vdagentd` treats as fatal, so the stock-tier clipboard cannot work
  there at all. Pass `LIMINA_FEDORA_REL=43` to run the old family deliberately.

## The two tiers (see `CLAUDE.md`)

- **Basic / stock baseline** — an unmodified-shaped Fedora guest on its own kernel via the EFI path.
  Must boot and be usable, **degraded**: software-2D display (no 3D capset advertised → GNOME renders
  in llvmpipe), no venus, no dynamic memory, no USB. This is the floor the whole upgrade path stands on.
- **Enhanced** — our custom 16 KiB kernel + venus + guest components (`limina-agent`,
  clipboard bridge). Unlocks accelerated 3D, zero-copy scanout, clipboard, etc. **Additive** — layered
  onto a basic guest, never a precondition for it.

## Component versions (canonical — link here, don't restate)

**The single source of truth for guest component versions.** Memory files and other docs should
*link to this table* rather than restate numbers — a stale "mesa 25.3.6" once propagated into three
memories before anyone noticed. Verify by reading an image's rpmdb directly (loop-mount the btrfs
root offline → `btrfs restore -r 256` the `root` subvol → `rpm --dbpath … -q`), or in a booted
guest with `rpm -q`. Last verified by the r22 installer's own `rpm -q` in each booted F44 enhanced guest 2026-09-02, and the dogfood row read off the running dev VM the same day. All three F44 enhanced images boot `7.1.8-limina16k.4` as their permanent default, each confirmed by a default (un-armed) boot.

| Tier / images | Kernel | Page | Mesa | Mutter | GNOME Shell |
|---|---|---|---|---|---|
| **F44 stock** (`*.raw`, `*.boot.raw`, `stock.test`) | `6.19.10-300.fc44` | 4 KiB | `26.0.3-4.fc44` | `50.0-1.fc44` | `50.0` |
| **F44 stock + freeworld VA** (`accessible`, `stock.test`) | `6.19.10-300.fc44` | 4 KiB | `26.1.8-1.fc44` + `mesa-va-drivers-freeworld-26.1.8-1.fc44` | `50.0-1.fc44` | `50.0` |
| **F44 enhanced** (`enhanced`, `enhanced.test`, `enhanced.synoik`) | `limina-kernel-16k-7.1.8-4` | 16 KiB | `26.1.8-9.limina.fc44` | `50.1-1.limina.fc44` | `50.0` (stock) |
| **F44 dogfood deployment** (the user's dev VM + upgraded clones) | `limina-kernel-16k-7.1.9-1` (running `7.1.9-limina16k`) | 16 KiB | `26.1.8-9.limina.fc44` | **stock** `50.3-3.fc44` | `50.3` (stock) |
| **F43 stock** (`vanilla`, `accessible`, `stock.test`) | `6.17.1-300.fc43` | 4 KiB | `25.2.4-2.fc43` | `49.1-1.fc43` | `49.1` |
| **F43 enhanced** (`enhanced`, `enhanced.test`) | `limina-kernel-16k-6.12.0` | 16 KiB | `26.1.5-1.limina.fc43` | `49.6-1.limina.fc43` | `49.1` (stock) |

Two facts the table cannot show:

- **The guest agents are not RPMs and so are not in the table.** All three F44 enhanced images
  carry **`limina-agent` 0.5.0** and `limina-agent-session`, installed to `/usr/local/bin` with
  their units (payload **r22**, delivered 2026-09-02; r22 is a host-side repack of r21 with only
  the agent swapped — every kernel and mesa RPM is byte-identical). 0.5.0 is the release that added
  the `vcpu` capability, so it is the floor for dynamic vCPU offlining on the enhanced tier; check
  it in a guest with `limina-agent --version`, which is also the fastest way to tell a stale image
  from a fresh one.

- **The limina kernel branch carries two drm/virtio commits**, both upstream-now candidates:
  per-scanout rects exposed as suggested connector offsets (the guest half of the arrangement
  relay), and typing + context-attaching PRIME-imported dmabufs (without it every
  software-decoded video frame is untyped host-side — `docs/graphics.md` §3.2). The
  `mm/page_reporting` freezable-workqueue commit was absorbed upstream at the v7.1.8 rebase, and
  the two DRM format/modifier commits were deliberately dropped (see §"Stock virtio-gpu formats"
  below). We would still build the kernel ourselves with no delta at all, for the **16 KiB page
  size and the config** — which is the whole reason it exists (venus, `docs/graphics.md` §3.3).
- **Mutter is stock from the guest's next distro update onward.** We stopped shipping a patched
  mutter on 2026-07-11: an `.limina` release loses to any stock bump, and the event that displaced
  it also proved stock mutter needs none of our rendering patches. Images that still contain
  `50.1-1.limina` are fine — the ladder absorbs either state.

Enhanced **mesa + kernel** are pinned to our version and `dnf versionlock`ed. Enhanced mutter,
where it still exists, is the target distro's mutter SRPM rebuilt with our patches over that
release's stock GNOME Shell (same `libmutter-NN` ABI).

### Hardware decode on the stock tier

Fedora builds mesa `-Dvideo-codecs=all_free`, and the VA frontend refuses H.264/HEVC in
`vl_codec.c` *before* the driver is consulted — so no host advertisement can reach a stock guest.
RPM Fusion's `mesa-va-drivers-freeworld` is the route a real Fedora user takes, and libva already
probes `/usr/lib64/dri-freeworld/` ahead of `/usr/lib64/dri/`. `scripts/provision/install-freeworld-va.sh
<image>...` installs it in place (CoW backup, boot, install, verify, poweroff).

Applied 2026-08-31 to `Fedora-Workstation-44.accessible.raw` and
`Fedora-Workstation-44.stock.test.raw`; verified on both that libva opens
`/usr/lib64/dri-freeworld/virtio_gpu_drv_video.so` and that H.264 Constrained-Baseline/Main/High,
HEVC Main and VP9 Profile 0 are advertised as `VAEntrypointVLD`.

**It drags stock mesa forward as a side effect.** freeworld is built against the current Fedora
mesa, so installing it upgraded these two images from `26.0.3-4.fc44` to `26.1.8-1.fc44`. They are
therefore no longer byte-identical to a fresh Workstation install — which matters when one of them
is standing in as the compatibility floor.

## Delivery

The enhanced tier is delivered as RPMs that **replace stock at `/usr`**, not as a sysext overlay —
the rationale is a mesa soname collision and is written up in `docs/graphics.md` §5.1.

**Current payload: `payload/limina-guest-tools-f44-r19.tar.zst`** (r19, 2026-09-01: kernel
`limina-kernel-16k-7.1.8-4` unchanged, mesa `26.1.8-7.limina` — a video decode target is now ONE
composite resource named by its planar format with its plane resources chained behind it, instead
of one unrelated resource per plane. Only a composite create names a planar format, which is what
a host-side planar allocation keys off, so this is what lets the host back a whole frame with a
single two-plane IOSurface and sample it without a re-read. Gated on the host advertising
`VIRGL_CAP_V2_VIDEO_PLANAR_TARGET`, so a guest on an older host silently keeps the per-plane form.
Measured on VP9 (`spikes/vt-vp9-decode/vp90-2-09-aq2.webm`, 107 frames): 12 composite targets,
each one two-plane EGL-bound IOSurface, 214 plane writebacks all landing, nothing refused, and
frame checksums identical to the software decoder — which covers the writeback into guest memory,
not the IOSurface the screen samples: those are two copies of every frame, and a checksum reaches
only the first. The delivered `-5` guest on the same host keeps the old shape and is
checksum-identical too. Applied with the real `install-enhanced.sh` to all
three F44 enhanced images (`.bak-pre-r19.raw` CoW backups), both agent hashes verified; the kernel
short-circuited as already installed and the permanent default stayed `7.1.8-limina16k.4`, so no
trial boot is owed. Previous: r18, 2026-08-31: kernel
`limina-kernel-16k-7.1.8-4` unchanged, mesa `26.1.8-5.limina` — virgl refuses to export a dmabuf
smaller than the image it describes. A decode target's guest BO is a one-page stub (4096 bytes at
every resolution, `alloc_size = 1`), so an exported fd named a page for a multi-megabyte surface
and GStreamer's dmabuf uploader walked off it: GNOME Videos rendered black and a bare
`vaXXXdec ! glimagesink` died with SIGBUS. Refusing makes the caller negotiate system memory —
one frame copy per frame, and it works. Measured in `spikes/va-dmabuf-size`: both codecs now put
60 buffers into `glimagesink` with zero errors and the decoder's caps are plain `video/x-raw`,
which is what proves renegotiation rather than a survived mapping. **The refusal cannot reach
anything but video**: `virgl_can_copy_transfer_from_host` excludes `VIRGL_BIND_SHARED`, so
Wayland buffers, EGL images and mutter's scanout are sized from their own layout and are never
short. **Stock Fedora mesa reproduces the bug byte for byte** (`26.0.3-4.fc44`, 4 KiB pages), so
this is upstream virgl behaviour and the stock tier is fixed only by upstreaming. **The base moves
26.1.7 → 26.1.8** here — Fedora's SRPM had moved and r18 takes it, unlike r17 which held the pin;
all 12 patches applied clean and 21 VideoToolbox sessions reported hardware with none in software.
Applied with the real `install-enhanced.sh` to all three F44 enhanced images
(`.bak-pre-r18.raw` CoW backups), both agent hashes verified; the kernel short-circuited as
already installed and the permanent default stayed `7.1.8-limina16k.4`, so no trial boot is owed.
Previous: r17, 2026-08-30: kernel
`limina-kernel-16k-7.1.8-4` unchanged, mesa `26.1.7-4.limina` — virgl no longer offers
three-plane 4:2:0 (YV12/IYUV) as a decode target. ffmpeg picks a VA surface format by exact
match against `sw_pix_fmt`, so I420 and YV12 both outscored NV12 and the last advertised won;
Firefox refused the I420 surface and decoded in software, making hardware decode advertised,
selected and silently unused. Measured in a stock F44 guest: ffmpeg picked `0x30323449` (IYUV)
before and `0x3231564e` (NV12) after, and Firefox then imports NV12 zero-copy. **The base is
held at 26.1.7 while Fedora's repos have moved to 26.1.8** — deliberate, so this differs from
r16's mesa by our one patch and nothing else; the versionlock keeps it pinned. Applied with the
real `install-enhanced.sh` to all three F44 enhanced images (`.bak-pre-r17.raw` CoW backups),
both agent hashes verified. **Deploy host-first**: this mesa is what makes hardware decode
actually engage, so a host without the plane-index fix (virglrenderer `2e0f1eff`) will read a
plane index as a layer range and poison the render context. Previous: r16, 2026-08-23: kernel
`limina-kernel-16k-7.1.8-4` and mesa `26.1.7-3.limina` — the guest half of the video-import fix.
drm/virtio now records `blob_mem` on a PRIME-imported dmabuf and gives the dma-buf GEM funcs
`.open`/`.close`; the virgl winsys reports the blob kind on its cache-hit path and planar YUV is
answered from the host caps instead of being hard-rejected. Without the matching host
virglrenderer (`86741d53`) the payload alone changes nothing — see `docs/graphics.md` §3.2.
Applied with the real `install-enhanced.sh` to all three F44 enhanced images
(`.bak-pre-r16.raw` CoW backups), both agent hashes verified; all three trial-booted and
promoted to `7.1.8-limina16k.4`. Previous: r15 (2026-08-21: a host-side
repack of r14 carrying `limina-agent-session` 0.1.1 — the claim-phase reconnect now writes its
HELLO before the `DisplayLayout` seed; the old order had the host drop the peer and the helper
reconnect without backoff until the worker died of `EMFILE`, see `docs/hardening-backlog.md`
§Guest-reachable aborts). Applied with the real `install-enhanced.sh` to all three F44 enhanced
images (`.bak-pre-r15.raw` CoW backups), the installed binary's sha256 verified against the build
(`e2d6c874…`) — the sequence `scripts/provision/deliver-payload.sh <payload> <image>...` now
scripts (backup → boot → install → verify both agent hashes → poweroff, per image); kernel and mesa short-circuited as already installed and the permanent default
stayed `7.1.8-limina16k.3`, so no trial boot was owed. Previous: r14 (2026-08-19: kernel
`limina-kernel-16k-7.1.8-3`, KREL `7.1.8-limina16k.3` — drm/virtio exposes each scanout's
`GET_DISPLAY_INFO` rect as the DRM `suggested X`/`suggested Y` connector properties AND
declares `hotplug_mode_update`, which compositors gate the offsets on: the guest half of the
host→guest arrangement relay, rig-verified end-to-end; each image trial-booted and
auto-promoted), r13 (2026-08-19, the
offsets without the gate — relayed values mutter never read), r12 (2026-08-19,
`limina-agent-session` multi-session arbitration — only the active seat session's helper
reports the arrangement), r11 (2026-08-18, zxdg_output_v1 logical rects for the
fractional-scale pointer mismap). What it contains:

- the `limina-kernel-16k` and mesa RPM sets, their srpms, and a **local dnf repo**
  (`payload/repo/`, built by `package-payload.sh` with `createrepo_c`; devel/tests in, debuginfo
  out). The installer drops it as `/usr/share/limina-guest-tools/repo` +
  `/etc/yum.repos.d/limina-guest-tools.repo`, so `dnf install mesa-libgbm-devel` resolves against
  our NEVRA instead of being "filtered out by exclude filtering" against the versionlock. Upgrades
  carry any installed `-devel`/`-tests` forward in the same transaction.
- `limina-agent` and `limina-agent-session` (musl static, cross-built on the host — no build guest
  needed for those two).
- `install-enhanced.sh` itself.

The installer also writes the driver selection to `/etc/environment.d/90-limina-zink.conf`
(`GALLIUM_DRIVER=virgl`, `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`, `VK_DRIVER_FILES=<venus ICD>`,
`VN_DEBUG=mem_budget`), sets GRUB's default to the 16 KiB kernel as a **one-shot trial** — reaching
the desktop auto-promotes it to permanent — and keeps the previous kernels as fallbacks.

**Deploy host-first.** The 0010-less guest mesa needs a host with KK ≥ `b778250986b` (real modifier
extension) and virglrenderer ≥ `0cc513fd` (verbatim modifier passthrough), i.e. the revs pinned in
`third_party/manifest.toml`. On an older host the guest sees no modifier extension and, with no
fabricated table to fall back on, the WSI takes the untested prime-blit path. Update the app before
(or with) the guest mesa.

A payload moves over gvproxy at ~90 MB/s — 440 MB lands in under 5 s. Copying it into a guest is
never the slow part of a delivery.

### Clipboard, as shipped

The `clipboard@limina` gnome-shell extension tier was **retired** (2026-08-15). The target shape is
`spice-vdagent` + `limina-agent-session` only: the helper withholds its `clipboard` capability while
a live `spice-vdagent` serves the session, so the two transports never both own the selection, and
reclaims it within one probe when vdagent goes away.

The **synoik image is the exception** and carries
`/etc/systemd/user/limina-agent-session.service.d/10-ignore-vdagent.conf` with
`LIMINA_CLIPBOARD_IGNORE_VDAGENT=1`. Measured reason: `xwayland-satellite` gives that session a
`DISPLAY=:0`, so vdagent starts and stays alive — but selections do not bridge. With the Wayland
selection owned (`wl-copy TOKEN`, confirmed by `wl-paste`), `xclip -o -selection clipboard` answers
*"There is no owner for the CLIPBOARD selection"*: vdagent's clipboard is X11-only and sees nothing
in the direction it must read for guest→host. **That is exactly the limit of a liveness probe** — it
proves vdagent reached an X server, not that the compositor bridges selections. Task #44 tracks the
real fix (selection bridging on the synoik/satellite side, or a functional probe that replaces the
per-image override).

### Stock virtio-gpu formats, and why synoik takes what it is given

Our two DRM format/modifier kernel commits were dropped on 2026-08-04 (tag
`limina/2026-08-04-modifiers` recovers them; rationale in `docs/upstreaming/ledger/linux.md`), and
`7.1.8-1` is the first binary built without them. The guest-visible effect, same mesa, kernel the
only variable:

| kernel | `DRM_CAP_ADDFB2_MODIFIERS` | primary plane formats |
|---|---|---|
| `7.1.6-2` (pre-drop binary) | 1 | XR24, AR24, XB24, AB24 |
| `7.1.8-1` (current) | 0 | XR24 only |

Upstream `virtio_gpu_formats[]` at v7.1.8 is literally `{ DRM_FORMAT_HOST_XRGB8888 }`.

A Vulkan compositor notices immediately: synoik allocates LINEAR through Vulkan, the stock plane
advertises the IMPLICIT/INVALID modifier, `XR24+LINEAR ∩ XR24+INVALID` is empty, and
`DrmCompositor::new` fails outright with *"No supported plane buffer format found"*. **The fix went
into synoik, not the kernel** (user's call): accept `INVALID` and take what the Vulkan allocation
provides. That is the better direction — it makes synoik work on **stock** virtio-gpu and removes a
kernel-patch dependency instead of reinstating one.

It is safe because of a property of *this* stack that should not be generalised: virtio-gpu has no
tiling, so its buffers are linear by construction, and the plane is not real hardware — scanout is
whatever CALayer does with an IOSurface the host already created and whose layout it already knows.
Nothing downstream infers layout from the modifier.

Guarded since 2026-08-15 by `crates/limina-test/tests/synoik_session.rs`, which **EFI-boots** the
synoik image (load-bearing: the injected-kernel seated path runs a 6.12 test kernel that still
advertises LINEAR and would be green regardless) and fails fast on the marker. Verified to
discriminate: RED in 26 s on the 7.1.8 image, GREEN on a pre-drop clone.

**Accepting INVALID gets the compositor up; a fourth fix is needed before a client buffer reaches
the plane at all** (reported by the synoik session, 2026-08-16). smithay's `DrmCompositor` gates
every plane promotion on `plane.formats.contains(&format)`, which on an implicit-modifier plane
compares a buffer naming LINEAR against a plane naming nothing and refuses everything — including
the **primary** plane, so it is not steerable by the caller (`try_assign_primary_plane` reads
`surface.plane_info()`, which the `planes:` argument to `DrmCompositor::new` cannot reach). synoik
carries a smithay fork patch matching on fourcc alone when every plane entry is INVALID.
**Consequence for us: a guest without that patch gets zero direct scan-out, so the venus present
path never engages for client buffers** — worth knowing before reading a trace from such a guest as
evidence about our present path.

## Traps that keep biting

- **An RPM `Release` bump does not let two builds of the same KREL coexist.** The bump is necessary
  for dnf to see a content change at the same version, but `limina-kernel-16k` is
  `installonlypkg(kernel)`, so dnf installs `-2` *beside* `-1` — and both own
  `/lib/modules/<KREL>/…`, producing hundreds of `conflicts with file from package`. The old
  package must come off first, and it is the running kernel: boot the previous fallback
  (`grubby --set-default /boot/vmlinuz-<old>`, reboot), `rpm -e limina-kernel-16k-<old>`, *then*
  run the installer. Bump `LOCALVERSION` instead only for throwaway probe kernels — KREL is the
  guest-visible identity.
- **`/boot` fills up, and `dnf remove` does not reclaim it.** The installer's 350 MiB preflight has
  refused an install at 242 MiB free. Removing a kernel package frees `/lib/modules` but leaves the
  vmlinuz + initramfs in `/boot` orphaned; `kernel-install remove <uname-r>` is what reclaims the
  ~100 MiB. Also note Fedora's installonly limit of 3 silently evicts your oldest fallback when a
  new kernel lands.
- **`~/rpmbuild/RPMS` is rpmbuild's accumulating output directory.** A payload build that copies it
  wholesale into an uncleaned `$OUT` ships RPMs neither build produced — one r9 payload carried four
  kernels and two mesa versions this way. The installer's multi-kernel guard would have refused it
  loudly, but nothing stopped it from being *built*. Both scripts now collect by exact NEVR and
  clean `$OUT`.
- **The accessible-derived images ship `org.gnome.shell disable-user-extensions=true`** (origin
  unknown, not our provisioning). On such guests any extension-based tier is blocked. Consider
  clearing it in `make-accessible.sh` at the next respin.
- **A confident code comment is a claim with a timestamp.** synoik's *"INVALID is refused at
  allocation rather than papered over"* read as a live design constraint; it was stale — written
  when LINEAR was forced everywhere — and taking it as current nearly produced a kernel patch to
  defend a decision that had already been abandoned.
- **Stale images cost a day.** Any rebuild of a guest-side component must flow into (a) the payload
  tarball and (b) an `install-enhanced.sh` pass over the enhanced images, then into the table
  above. On 2026-07-02 every "identical" local repro of a dogfood crash silently ran a guest two
  deliveries behind.

## Images

### Fedora 44 — mirrored image set (in progress, started 2026-06-29)

Mirrors the F43 five-role layout (see the release selector in Conventions); select with
`LIMINA_FEDORA_REL=44`. Built natively in-guest from Fedora's own F44 SRPMs + a minimal limina
delta (`scripts/provision/f44/`, `scripts/provision/make-accessible.sh`).

#### Baked-in perf tooling (added 2026-08-08)

`Fedora-Workstation-44.enhanced.raw` now carries the whole measurement battery, so a perf pass
needs no ad-hoc installs (which perturb the very thing being measured, and on 2026-08-08 caused a
`GUARD_FAIL` that read as a driver fault when it was only a missing binary):

| tool | provenance | used by |
|---|---|---|
| `glmark2` | Fedora `glmark2-2023.01^20250221gitcebbb63-3.fc44` | `glmark2-wayland-venus`, `glmark2-display-*`, the ledger backend guard |
| `apitrace` (`eglretrace`) | Fedora `apitrace-13.0-6.fc44` | `gl-replay-venus`, `gl-replay-llvmpipe` |
| `vkmark` | Fedora `vkmark-2025.01-3.20250123git2bf2ca7.fc44` | `vkmark-default-venus` — note this is the **distro** binary, so compare only against `vkmark-default-venus` rows, never the `vkmark-3scene-venus` ones |
| `fio` | Fedora | virtio-blk path numbers |
| `gfxrecon-replay` | **built in-guest** at `~claude/gfxreconstruct/build/tools/replay/`, upstream `765c3d6`; on the F44 `enhanced.raw` **and** `enhanced.test` images it is `/opt/gfxreconstruct`, cross-built on the host by `scripts/build-gfxreconstruct.sh` at the **same** `765c3d6` — on BOTH images so the documented reclone (`cp -c enhanced.raw enhanced.test.raw`) preserves it: a reclone from an enhanced.raw without it silently strips the fixture and `venus_vk_replay` fails with `gfxrecon-replay: No such file or directory` (bit the suite 2026-08-19; install = extract `target/test-guest/gfxreconstruct.tar.zst` into `/opt`) | `vk-replay-venus-headless`, and the L2 `venus_vk_replay_matches_lavapipe_reference` |

`gfxrecon-replay` is not packaged for Fedora; the build recipe (and its F44-specific dependency
set, without which OpenXR aborts cmake) lives in the header of `scripts/perf-ledger.sh`. Build
with **`-j2`** — `-j4` OOMs a 4 GiB guest. (The host-side container build in
`scripts/build-gfxreconstruct.sh` has the same appetite at 12 GiB and 8 CPUs: it needs `-j4`,
because each generated Vulkan/OpenXR encoder unit wants >2 GB in `cc1plus` and the default width
gets them OOM-killed.)

**PIN THE COMMIT, NOT THE TAG (2026-08-16).** `build-gfxreconstruct.sh` used to default to
`GFXR_TAG=latest`, which resolves against release *tags* — the newest is `v1.0.4`, far behind
main. That build replays perfectly (200 frames, exit 0) but ends its summary with `Replay FPS`,
while `crates/limina-test/tests/venus_replay.rs` greps for the current `Measured FPS`, so the test
fails on wording with nothing wrong underneath. The script now defaults to `765c3d6`, the commit
the goldens were built from. Move this pin and that grep together, never one alone. Since 2026-08-08 `perf-ledger.sh` **aborts** rather
than silently dropping the `vk-replay` row if this binary is missing, so if that fires, the guest
has drifted from this baseline (`LIMINA_PERF_SKIP_VK=1` overrides deliberately).

The toolchain install pulled a routine `glibc`/`libgcc` dependency upgrade into the base; the
versionlocked components are unaffected (mesa `26.1.5-8.limina.fc44`, kernel `7.1.6-limina16k`
both verified after the fact). A CoW safety copy was taken first as
`Fedora-Workstation-44.enhanced.raw.pre-perftools.bak`. **`enhanced.test.raw` was NOT recloned** —
the frozen L2 snapshot does not need perf tooling, and recloning it would churn the test baseline.

#### `Fedora-Workstation-44.enhanced.synoik.raw` — the synoik compositor image (added 2026-08-14)

**synoik updated to `2ef727c` on 2026-08-17** — carries the per-display config fixes ("An applied
scale belongs to the display it was applied to", "An off-ladder stored scale is honored, on
purpose", "An unresolved PNP id shows as GNOME shows it"). With them synoik matches mutter on the
connector-cycle path: one `<configuration>` stanza per host display, each display's own scale
re-applied on every migration, and the vendor rendered as `LMN` with a lowercase serial. Verified
on two physical displays in **both** `host` and `dynamic` modes (`spikes/display-identity-hotplug/`).

`efbb2b8` (2026-08-15) carried `808bfcd`/`9e4148a`, the implicit-modifier
scanout fix (task #39). Before it, this image was RED on the r9 7.1.8 kernel: the plane advertises
XR24+INVALID, synoik allocated XR24+LINEAR, the intersection was empty and no compositor took the
display. `synoik_session_reaches_a_rendered_desktop` is GREEN on it again (28 s).

**Mesa refreshed to `26.1.5-8.limina.fc44` on 2026-08-14** (same `install-enhanced.sh` pass as
`enhanced.raw` / `enhanced.test.raw`) — the CPU→GPU dmabuf coherency fix. This image is the
vehicle that reproduced it; `spikes/dmabuf-cpu-coherency/probe.c` runs clean here now, including
the first run after a guest boot.

The canonical image for anything that needs a **Vulkan compositor**. Built because a whole class
of host bugs is only reachable when the compositor imports client dmabufs through Vulkan/venus —
mutter composites with GL, so under mutter those paths are never exercised and every run is a
**false negative** (the vrend/KK stride shear below is the worked example). It replaces the
undocumented `nirirepro*` and `enhanced.testcomp` scratch images, which are deleted.

- **Base**: CoW clone of `Fedora-Workstation-44.enhanced.test.raw` (kernel `7.1.6-limina16k`,
  16 KiB pages), already on the supported enhanced env —
  `GALLIUM_DRIVER=virgl`, `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` (GL on vrend, Vulkan on venus).
- **synoik**: cloned from `github.com/kov/synoik` into `~claude/synoik` and built **in-guest**
  (`cargo build --release`). Installed as the session with the project's own script:
  `sudo TEST_USER=claude PROFILE=release scripts/install-test-session.sh`, which writes the
  `org.gnome.Shell@user.service` drop-in (`ExecStart=…/target/release/synoik --session`) and
  compiles synoik's schemas into the private `/usr/local/share/synoik/glib-2.0/schemas`.
  GDM autologin for `claude` was already on, so the **normal GNOME session comes up on synoik**.
- **Iterating**: rebuild in-guest and reboot (or log out/in) — the unit always runs whatever is at
  `target/release/synoik`. No reinstall step.
- **Extra build deps** beyond the spec's `BuildRequires`: **`glslang`** (for `glslangValidator`).
  The spec omits it and the build fails in `synoik-vk/build.rs` — worth an upstream fix.

Two gotchas that cost a run each, both of the same "verify, don't assume" shape:

- **synoik's Wayland socket is `wayland-1`, not `wayland-0`** (gdm holds `-0`). A client launched
  with a hardcoded `WAYLAND_DISPLAY=wayland-0` never connects, so nothing is imported and every
  measurement reads clean. **Discover the socket** (`ls /run/user/1000/wayland-*`).
- **Restarting GDM does not re-read `/etc/environment.d`** — the systemd *user manager* survives.
  Reboot the guest, and verify the driver env at `/proc/<compositor-pid>/environ`, not in the file.

Run it like any enhanced image: `cargo xtask run --disk Fedora-Workstation-44.enhanced.synoik.raw`
(the worker log is per-disk — `/tmp/limina-worker-<disk>.log` — and `/tmp/enhanced-efi-kk-worker.log` is a symlink to whichever VM booted last; `LIMINA_BOOT_LOG=<path>` still overrides).

##### Rebuilding it (and retargeting to F45)

The guest-side work is scripted: **`scripts/provision/f44/install-synoik-session.sh`**, which
runs **in the guest** and is idempotent (re-run it to update synoik). It installs the build
deps, clones/updates `~/synoik`, builds, and calls synoik's own `install-test-session.sh` —
never hand-roll the systemd drop-in, since the installer is the source of truth for it and for
the private GSettings schema dir.

Host-side bracket, from the repo root:

```sh
cp -c Fedora-Workstation-44.enhanced.test.raw Fedora-Workstation-44.enhanced.synoik.raw  # APFS CoW, instant
cargo xtask run --disk Fedora-Workstation-44.enhanced.synoik.raw                          # boots with --net
# read the auto-allocated port from the worker log: "guest SSH forward ready: ssh -p N ..."
scp -P <N> scripts/provision/f44/install-synoik-session.sh claude@127.0.0.1:
ssh -p <N> claude@127.0.0.1 './install-synoik-session.sh'
# then reboot the guest; power it off cleanly before using the image
```

`cp -c` is load-bearing: it CoW-clones 40 G instantly, and the image **boots in place**, so
always clone before a run you don't want persisted.

**For a Fedora 45 test target**, the script itself should carry over unchanged — it installs by
package name and builds from source, with nothing F44-specific in it. What needs re-deciding is
the *base*: build the F45 enhanced image first (kernel + mesa RPMs against F45 SRPMs, per this
directory's README), then point the clone at that instead of `enhanced.test.raw`. Expect the
dep list to be the drift point — it mirrors `synoik.spec.rpkg`'s `BuildRequires` by hand
(the `.rpkg` macros don't expand outside an rpkg checkout, so `dnf builddep` isn't usable),
so re-check it against the spec when the base moves.

#### Rebuilding `enhanced.raw` from the accessible base (validated 2026-07-05)

`install-enhanced.sh` delivers RPMs but deliberately does **NOT** resize the disk (it must also
run unmodified on a stock user's daily-driver guest). The enhanced **dev** image needs a bigger
disk than the 13.7 G accessible base — the `7.1.2-limina16k` kernel alone ships ~7 GiB of
unstripped debug modules, and there must be headroom for **in-guest builds** — so the grow is a
**manual pre-install step**. Full procedure (all host commands from the repo root):

```bash
# 1. Fresh CoW clone of the stock base + grow the virtual disk to 40 G (host).
rm -f Fedora-Workstation-44.enhanced.raw
cp -c Fedora-Workstation-44.accessible.raw Fedora-Workstation-44.enhanced.raw
qemu-img resize -f raw Fedora-Workstation-44.enhanced.raw 40G

# 2. Boot basic tier (stock kernel) with networking + ssh (read the port from the log).
LIMINA_DISK=Fedora-Workstation-44.enhanced.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &

# 3. In-guest: grow the btrfs root partition (vda3) into the new space, online.
ssh -p <PORT> claude@127.0.0.1 '
  sudo dnf install -y cloud-utils-growpart
  sudo growpart /dev/vda 3            # vda1=ESP vda2=/boot vda3=btrfs root
  sudo btrfs filesystem resize max /' # df / should now show ~38 G

# 4. Copy + install the current guest-tools payload, then clean-poweroff.
scp -P <PORT> target/guest-tools-7.1.2-mesa3/limina-guest-tools-f44.tar.zst claude@127.0.0.1:
ssh -p <PORT> claude@127.0.0.1 '
  tar --zstd -xf limina-guest-tools-f44.tar.zst
  sudo ./limina-guest-tools/install-enhanced.sh ~/limina-guest-tools
  sudo systemctl poweroff'

# 5. Reboot: GRUB takes the installer's ONE-SHOT trial into the 16k kernel; reaching the
#    desktop auto-promotes it to the permanent default. Verify venus (seated GNOME + Mesa
#    render lines in the worker log), then clean-poweroff again.
LIMINA_DISK=Fedora-Workstation-44.enhanced.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &

# 6. Reclone the frozen L2 test snapshot from the quiesced (powered-off) image.
cp -c Fedora-Workstation-44.enhanced.raw Fedora-Workstation-44.enhanced.test.raw
```

| Image | Role | Status |
|---|---|---|
| `Fedora-Workstation-44.vanilla.raw` (+ `.xz`) | **Pristine** F44 Workstation aarch64 (official `…44-1.7.aarch64.raw.xz`; Fedora-built → SELinux labels intact, EFI-boots *enforcing* with no relabel loop). Clone source only. | ✅ renamed from `…44.raw` |
| `Fedora-Workstation-44.accessible.raw` | **Stock base**: vanilla + gnome-initial-setup (`claude`) + pubkey + autologin + NOPASSWD sudo + `vulkan-tools` + no-idle-lock gschema + console args + relabel-clear (`make-accessible.sh`). Promoted from the existing `44.boot.raw` (already had user/ssh/autologin/sudo). | ✅ built 2026-06-29 |
| `Fedora-Workstation-44.stock.test.raw` | **Stock-tier L2 image** — frozen CoW snapshot of `accessible` (`DEFAULT` for `LIMINA_FEDORA_REL=44`; also the seated baseline-3D vehicle). | ✅ built; `efi_boots_to_userspace` GREEN 2026-06-29 |
| `Fedora-Workstation-44.enhanced.raw` | **Enhanced base** — `accessible` + `scripts/provision/f44/` builds (16k kernel `6.19.10-limina16k`, venus mesa `26.1.3-1.limina`, patched mutter `50.1-1.limina` w/ **all 3 patches** incl 0003 clipboard *(historical — mutter left the delivery 2026-07-11 and is stock going forward; see the note above)*, + `limina-agent`) → `install-enhanced.sh`. **✅ FINALIZED 2026-06-29**: seated GNOME, WebGL 5000-fish ~60fps on venus→KK→Metal (5-signal+pixel verified); mutter 0003 rebased to 50.1 (`ext_data_control_manager` live in `libmutter-18`); limina-agent (native gnu) active+connected; relabel-clean; build cruft removed. Kernel kept Fedora-config **with debug symbols** (no strip — ~7 GiB modules, slower boot, by choice). Now also carries the **L2 test tooling** (glmark2 + apitrace/`eglretrace` GL replay + `/opt/gfxreconstruct/bin/gfxrecon-replay` VK replay) — folded into `make-accessible.sh` going forward; the enhanced *delivery* (`install-enhanced.sh`) does **not** ship these, so a migrated daily-driver guest stays clean. **Respun 2026-07-04 to kernel `7.1.2-limina16k` + mesa `26.1.3-3` (dogfood parity — see the respin note above); versions in this row are the 2026-06-29 baseline.** **REBUILT FRESH 2026-07-05** from `accessible` per the procedure above (the prior `enhanced.raw`/`.test.raw` had accumulated bad state — the 16k kernel failed its `/boot/efi` mount and dropped to the rescue BLS entry; a clean clone+install booted `7.1.2-limina16k` with `/boot/efi` mounted, venus seated on the new KK). | ✅ finalized 2026-06-29; respun 2026-07-04; rebuilt 2026-07-05 |
| `Fedora-Workstation-44.enhanced.test.raw` | **Enhanced-tier L2 image** — frozen CoW snapshot of `enhanced` (`seated_fedora_from_env` for `LIMINA_FEDORA_REL=44`). Refresh: `cp -c Fedora-Workstation-44.enhanced.raw Fedora-Workstation-44.enhanced.test.raw`. **Recloned 2026-07-05 from the fresh rebuild** (see the `enhanced.raw` note). | ✅ **L2 GREEN 7/7 2026-06-29** (venus×3 + replay×3 + reset; replay tooling baked in); recloned 2026-07-05 |

`Fedora-Workstation-44.boot.raw` is the **pre-accessible** image (stock F44 + `claude`/autologin,
software-2D floor pixel-verified 2026-06-20); running `make-accessible.sh` on it produces
`accessible.raw`. `f44-edk2-build.raw` — **RETIRED 2026-06-25** (`images-staging-delete/`, expires
2026-07-02): the EDK2 firmware build moved to the unified `limina-build` container image (below).

### Debian — the stock encrypted-root guest

| Image | Role |
|---|---|
| `Debian-testing.luks.raw` | **Stock-tier Debian, encrypted root.** The only guest we have with LVM-on-LUKS, which makes it the vehicle for the pre-driver keyboard window: its passphrase prompt runs in the initramfs, where no stock generator ships `virtio_input`, so it is a *hard* test of the USB HID keyboard gadget rather than a nuisance (`docs/design/usb-hid-keyboard.md`). Carries **no limina guest components** — it is the non-Fedora stock-baseline vehicle. |

Installed 2026-08-23 from `debian-13.6.0-arm64-netinst`, then dist-upgraded trixie → **forky**
(`testing`) the same day; running `7.1.8+deb14.1-arm64`. Access is `claude` / `claudiusrobotus`
with passwordless sudo.

- **It tracks `testing`, deliberately, and must not be moved back to stable.** Suspend needs a
  guest kernel **≥ 6.17** for virtio-input's freeze to reset the device, and that fix was never
  backported to 6.12.y — so Debian stable can never suspend
  (`docs/design/m9.2-quiesced-snapshot.md`). `sources.list` points at `testing`/`testing-security`.
- **Every cold boot stops at the LUKS prompt and a human has to type the passphrase** — we do not
  hold it. Batch whatever you need per boot, and ask before rebooting.
- A trixie→forky dist-upgrade needs `-o Dpkg::Options::=--force-overwrite` (files legitimately move
  between packages across releases) and does **not** restart sshd; run it under `systemd-run` so it
  survives losing the ssh session.
- It ran as a managed VM at 6 vCPU / 8 GiB (`1024..8192` balloon, moderate reclaim), USB + battery
  + FIDO + fingerprint, NAT, windowed — the settings to recreate if it is wrapped in a `.liminavm`
  again. Boot it flat with `cargo xtask run --disk Debian-testing.luks.raw`.

## The unified build image (`limina-build:fc43`)

Every **Linux** build runs in one container image — `scripts/build-image/Containerfile`, built on first
use by `scripts/build-image.sh` (rebuild with `FORCE=1`). It bakes the union of all build deps
(rpmbuild, kernel toolchain, meson/ninja + `builddep mesa`/`builddep mutter`, edk2 + nasm/acpica + a
`-std=gnu17` ccwrap for edk2's K&R BaseTools, gfxreconstruct's cmake/xcb/X11/wayland set), so the
per-script `dnf install` is gone and builds start instantly. Consumers: `build-krun-efi`, `build-mesa-rpm`,
`build-mutter-rpm`, `build-kernel-rpm`, `build-test-kernel`, `build-mesa-zink`, `build-venus`,
`build-gfxreconstruct`. Each still mounts its own persistent source/cache `container volume` (the image
carries the toolchain; the volume carries source + incremental state). **Exceptions** (correctly NOT on
this image): the macOS-native builds (`build-app`, `build-virglrenderer`, `build-hvf-trap-probe`,
`build-test-guest`) emit Mach-O, not Linux; and `build-dbus-guest` stays on Alpine — it extracts a *musl*
dbus for the musl L1 guest, which a glibc image can't produce. Requires Rosetta (Apple `container`'s
BuildKit needs it); install once with `softwareupdate --install-rosetta --agree-to-license`.

Boot the baseline (bare `limina` — the default coexist device advertises venus, which a stock
4 KiB guest can't use and Mesa **degrades gracefully to `kms_swrast`/llvmpipe**, so the desktop
comes up in software regardless; pass `--gpu-software-2d` to force the clean software path with no
venus probing):
```bash
target/debug/limina --window --firmware target/krun-efi/KRUN_EFI.gop.fd \
  --disk Fedora-Workstation-44.boot.raw --cpus 4 --ram-mib 6144 --net
```

### Fedora 43 — dev & enhanced-tier images

| Image | Role |
|---|---|
As of the 2026-06-25 consolidation there are **two bases** (a stock one and an enhanced one), each
with a **clearly-named frozen test snapshot** the L2 suite boots. The old crufty `…raw` / `…test.raw`
dev images and the source-built `…dev-enh.raw` were retired to `images-staging-delete/` (expire
2026-07-02 — see that README).

| Image | Role |
|---|---|
| `Fedora-Workstation-43.vanilla.raw` | **Pristine** stock F43 Workstation aarch64 (mesa 25.2.4, mutter 49.1, 4 KiB kernel) — clone source only, no user. Boots to gnome-initial-setup. |
| `Fedora-Workstation-43.vanilla.raw.xz` | Compressed pristine F43 source. Re-decompress (`xz -dk`) to reset `…vanilla.raw` to factory — the cheap reset point (mirrors the F44 `.raw.xz`). |
| `Fedora-Workstation-43.accessible.raw` | **The STOCK base** (added 2026-06-25): a `…vanilla.raw` clone with gnome-initial-setup done (user `claude`), host pubkey in `authorized_keys`, **autologin**, **NOPASSWD sudo** (`/etc/sudoers.d/91-claude-nopasswd`), saved via a clean PSCI poweroff. Stays **stock** (mesa 25.2.4, kernel `6.17.1-300.fc43`, 4 KiB) — no `/opt` cruft. Carries two test-support tweaks that don't change the tier (2026-06-25): **`vulkan-tools`** installed (so `venus_enumerates_on_16k_kernel` can run `vulkaninfo` — Fedora Workstation doesn't ship it by default) and a system-wide **no-idle-screen-lock** gschema override (`/usr/share/glib-2.0/schemas/90-limina-no-idle-lock.gschema.override`: idle-delay 0, lock-enabled false, idle-activation-enabled false). The clone-source for `stock.test.raw`, the start point for enhanced-tier provisioning (`cp -c` → boot → `scripts/provision/install-enhanced.sh`), and the stock-tier (software + virgl) perf control. |
| `Fedora-Workstation-43.stock.test.raw` | **Stock-tier L2 test image** (`DEFAULT_TEST_DISK`) — a frozen CoW snapshot of `accessible.raw`. **MUST stay stock**: the EFI tests (`fedora_from_env`) boot its own stock Fedora kernel (the compatibility floor), and the venus tests (`enhanced_fedora_from_env`) boot it with an *external* 16 KiB kernel to prove **stock mesa's venus works on 16 KiB pages**. Refresh: `cp -c Fedora-Workstation-43.accessible.raw Fedora-Workstation-43.stock.test.raw`. |
| `Fedora-Workstation-43.enhanced.raw` | **The ENHANCED base** (RPM-delivered, tooled): an `accessible.raw` clone with `install-enhanced.sh` run — **16 KiB kernel** `6.12.0-limina16k+`, **mesa `26.2.0-1.limina`** (zink+venus at `/usr`, dnf-versionlocked), **patched mutter**, all as **RPMs replacing stock** ([[limina-enh-delivery]]). venus desktop pixel-verified; on-display glmark2 = **2784**. Now also carries the **L2 test tooling baked in**: `apitrace`/`eglretrace` (GL replay) + `/opt/gfxreconstruct/bin/gfxrecon-replay` (VK replay). Also carries the same system-wide **no-idle-screen-lock** override (2026-06-25) so the seated session never auto-locks during long tests. The clean *product* (no test tooling) is reproducible anytime via `accessible.raw` + `install-enhanced.sh`. |
| `Fedora-Workstation-43.enhanced.test.raw` | **Enhanced-tier L2 test image** (`seated_fedora_from_env`, override `LIMINA_TEST_DISK_ENH`) — a frozen CoW snapshot of `enhanced.raw`. The vehicle for `venus_replay` (seated venus GL+VK trace replay; all three replay paths smoke-verified). Refresh: `cp -c Fedora-Workstation-43.enhanced.raw Fedora-Workstation-43.enhanced.test.raw`. |

Boot the enhanced tier: `scripts/run-enhanced.sh [--window | --capture <png>]` (clones internally), or for
the seated-venus flow reuse the base without re-cloning:
`LIMINA_DISK=$PWD/Fedora-Workstation-43.enhanced.raw bash spikes/venus-draw-probe/boot-seated-efi.sh`.

## Credentials

- **F43 family:** user `claude`, password `claudiusrobotus`; the host's default pubkey is in
  `claude`'s `authorized_keys` (passwordless `ssh -o BatchMode=yes`), and `claude` has passwordless
  `sudo`. `sshd` enabled by default.
- **F44 `boot.raw`:** `claude` user (password `claudiusrobotus`), autologin on. `sshd` was enabled
  post-setup (`sudo systemctl enable --now sshd`). The host pubkey is in `authorized_keys`
  (passwordless `ssh -o BatchMode=yes`), and `claude` has NOPASSWD sudo
  (`/etc/sudoers.d/90-claude-nopasswd`) — matching the F43 dev convenience. Stock 4 KiB kernel
  (`6.19.x-300.fc44.aarch64`), `getconf PAGE_SIZE` = 4096.

## SSH access

Boot with `--net` (a supervised gvproxy user-mode NAT, no root) and SSH into the guest. The
supervisor logs the **exact command** at startup — read the port from it, don't assume 2222:

```
guest SSH forward ready: ssh -p N <user>@127.0.0.1
```

gvproxy forwards `127.0.0.1:<PORT> → guest:22` (the well-known MAC gives the guest the static `.2`
lease — see `docs/research/07-networking.md`). The host `<PORT>` **auto-allocates from 2222 upward**
(the first free loopback port), so it's 2222 for a lone VM but 2223+ when 2222 is already taken. Pin
it with `--ssh-port <1024-65535>` (requires `--net`; errors if the port is busy). Run **two or more
VMs at once** by leaving `--ssh-port` off on each (each grabs the next free port — read each VM's own
startup log) or by pinning distinct ports. `--net-log <file>` captures gvproxy's `-debug` packet log
(the host-side network oracle: DHCP/DNS/NAT).

Wait for the SSH banner (~10–15s post-boot), then (substitute the logged port for `<PORT>`):
```bash
ssh -p <PORT> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1
```
user `claude` / password `claudiusrobotus`, passwordless sudo. The full operational SSH recipe +
harness builders (`GuestConfig::with_net` / `with_ssh_port`) live in the `limina-fedora-access`
agent memory.
