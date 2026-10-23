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
- **A delivery reaches the images it was pointed at, and nothing else.** "Applied to all four F44
  enhanced images" means the four canonical files in the image directory — `enhanced.raw`,
  `enhanced.test.raw`, `enhanced.synoik.raw`, `enhanced.kde.raw`. A clone taken
  outside this tree (a rig, a spike, a sibling checkout's `harness/vm/disks/`) is a **fork from
  the moment it is copied**: nothing tracks it, no later `deliver-payload.sh` finds it, and this
  document cannot see it. So the Component versions table describes the canonical images only.
  **Before trusting a version on any clone, read it in the guest** — `cat /opt/limina/*/MESA_REV`,
  `uname -r`, `limina-agent --version` — rather than inferring it from this table or from the clone's mtime.
  Bring a stale one forward with `scripts/provision/deliver-payload.sh <payload> <clone>` like any
  other image.

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
- **Enhanced** — our custom 16 KiB kernel + limina's GPU drivers (beside the distro's Mesa) + guest
  components (`limina-agent`, clipboard bridge). Unlocks accelerated 3D, zero-copy scanout, clipboard, etc. **Additive** — layered
  onto a basic guest, never a precondition for it.

## Component versions (canonical — link here, don't restate)

**The single source of truth for guest component versions.** Memory files and other docs should
*link to this table* rather than restate numbers — a stale "mesa 25.3.6" once propagated into three
memories before anyone noticed. Verify by reading an image's rpmdb directly (loop-mount the btrfs
root offline → `btrfs restore -r 256` the `root` subvol → `rpm --dbpath … -q`), or in a booted
guest with `rpm -q`; the GPU drivers by `/opt/limina/<wire>/MESA_REV` and `dkms status`. The F44
enhanced row is what payload r42 installs and `deliver-payload.sh` verifies on each image (agent
hashes, the drivers' `MESA_REV`, no limina mesa RPM or driver-selection file, the 16k kernel's
virtio-gpu resolving to DKMS's module). The dogfood row was read with `uname -r`, `rpm -q`, `dkms
status` and the vendor tree's `MESA_REV` on the dev VM 2026-10-14. All four F44 enhanced images boot `7.1.13-limina16k.2` as their permanent default. The synoik image never reaches `multi-user.target` (its `plymouth-quit-wait` hangs), so `limina-kernel-promote.service` never fires there: after its trial boot, promote by hand with the unit's own two steps (`grubby --set-default=/boot/vmlinuz-<KREL>`, then disable the unit).

| Tier / images | Kernel | Page | Mesa | Mutter | GNOME Shell |
|---|---|---|---|---|---|
| **F44 stock** (`vanilla`, `stock.test`) | `6.19.10-300.fc44` | 4 KiB | `26.0.3-4.fc44` | `50.0-1.fc44` | `50.0` |
| **F44 stock + freeworld VA** (`accessible`, `stock.test`) | `6.19.10-300.fc44` | 4 KiB | `26.1.8-1.fc44` + `mesa-va-drivers-freeworld-26.1.8-1.fc44` | `50.0-1.fc44` | `50.0` |
| **F44 enhanced** (`enhanced`, `enhanced.test`, `enhanced.synoik`, `enhanced.kde`) | `limina-kernel-16k-7.1.13-2` (+ `-devel`; virtio-gpu is limina's DKMS module `852836da0d9d`) | 16 KiB | **stock** `26.2.5-1.fc44` (distro-synced by the installer), plus the vendor drivers under `/opt/limina/5` (`[kosmickrisp-guest]` `1b9049a75d7`) | `50.1-1.limina.fc44` | `50.0` (stock) |
| **F44 dogfood deployment** (the user's dev VM) | `limina-kernel-16k-7.1.13-2` (+ `-devel`; default `7.1.13-limina16k.2`, `7.1.13-1` and `7.1.9-1` kept as fallbacks; virtio-gpu is limina's DKMS module `7889c378f3d2`) | 16 KiB | **stock** `26.2.4-1.fc44`, plus the vendor drivers under `/opt/limina/4` (KosmicKrisp `0144c30fc8c`) | n/a: synoik | n/a: synoik |

Two facts the table cannot show:

- **The guest agents are not RPMs and so are not in the table.** All four F44 enhanced images
  (`enhanced`, `enhanced.test`, `enhanced.synoik`, `enhanced.kde`) carry **`limina-agent` 0.6.2**
  and **`limina-agent-session` 0.1.4**, installed to `/usr/local/bin` with their units (payload
  **r42**). The dogfood guest:
  - is current on `limina-agent`: 0.6.2, the r39 binary installed by hand 2026-10-23, with 0.6.1
    kept beside it as `limina-agent.0.6.1.bak`;
  - is current on `limina-agent-session`: 0.1.4, the r38 binary installed by hand 2026-10-22, with
    0.1.3, 0.1.2 and 0.1.1 kept beside it as `limina-agent-session.0.1.3.bak` / `.0.1.2.bak` /
    `.0.1.1.bak`.

  0.1.4 lets only the uid holding seat0 (`ACTIVE_UID` in logind's seat file) report the
  arrangement, and re-sends when the seat changed between polls. 0.1.3 reads idle inhibitors from
  every source the desktop offers (the compositor through ext-idle-notify-v1, gnome-session,
  PowerDevil) and reports idle inhibited when any of them says so; 0.1.2 read gnome-session alone. Earlier helper changes still in effect:
  - 0.1.2 started the idle-inhibit relay (`idleinhibit`, the host's keep-the-display-awake
    signal), and shuts a host channel down rather than dropping one half of it;
  - 0.1.1's drop-after-accept backoff (r27) stops a host that drops a channel within 5 s of
    opening it from turning into a connect storm.
  Check the helper with `limina-agent-session --version`. 0.6.2 added the guest's file refault
  counter and page size to the memory report, which the host's balloon policy gives memory back
  on when the guest thrashes its page cache; 0.6.1 added
  the CPU utilisation and stall rates the host's vCPU grow rule needs; 0.6.0 added the
  `powerprofile` capability (the GNOME power-mode toggle reaching host policy); 0.5.0 added `vcpu`,
  the floor for dynamic vCPU offlining. Check the version in a guest with `limina-agent --version`, which is also the
  fastest way to tell a stale image from a fresh one.

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

Enhanced **Mesa is the distro's**, unlocked and updated like any package; limina's GPU drivers sit
beside it under `/opt/limina/<wire>/` and drive every virtio-gpu (`docs/graphics.md` §5.1). The
16k kernel is `installonly`, never locked; the **stock** kernel packages are `dnf versionlock`ed so
a stock update cannot take the boot default.

### Hardware decode on the stock tier

Fedora builds mesa `-Dvideo-codecs=all_free`, and the VA frontend refuses H.264/HEVC in
`vl_codec.c` *before* the driver is consulted — so no host advertisement can reach a stock guest.
RPM Fusion's `mesa-va-drivers-freeworld` is the route a real Fedora user takes, and libva already
probes `/usr/lib64/dri-freeworld/` ahead of `/usr/lib64/dri/`. `scripts/provision/install-freeworld-va.sh
<image>...` installs it in place (CoW backup, boot, install, verify, poweroff). The enhanced tier
needs none of this: limina's VA driver carries H.264/HEVC and libva finds it first through
`LIBVA_DRIVERS_PATH` (§Delivery).

Applied 2026-08-31 to `Fedora-Workstation-44.accessible.raw` and
`Fedora-Workstation-44.stock.test.raw`; verified on both that libva opens
`/usr/lib64/dri-freeworld/virtio_gpu_drv_video.so` and that H.264 Constrained-Baseline/Main/High,
HEVC Main and VP9 Profile 0 are advertised as `VAEntrypointVLD`.

**It drags stock mesa forward as a side effect.** freeworld is built against the current Fedora
mesa, so installing it upgraded these two images from `26.0.3-4.fc44` to `26.1.8-1.fc44`. They are
therefore no longer byte-identical to a fresh Workstation install — which matters when one of them
is standing in as the compatibility floor.

## Delivery

The enhanced tier is a **payload**, `payload/limina-guest-tools-f44-rNN.tar.zst` (gitignored),
which `install-enhanced.sh` applies inside a running stock or enhanced guest. The guest keeps its
distro's Mesa; nothing of the distro's is replaced, locked or shadowed.

**Current payload: `payload/limina-guest-tools-f44-r42.tar.zst`** — `limina-kernel-16k-7.1.13-2`
(+ `-devel`), the GPU drivers `/opt/limina/5` at `[kosmickrisp-guest]` `1b9049a75d7` with DKMS
`limina-virtio-gpu` `852836da0d9d` (series 6.19, 7.1, 7.2), `limina-agent` 0.6.2,
`limina-agent-session` 0.1.4. Applied to all four F44 enhanced images (`.bak-pre-r42.raw` CoW
backups). Its `manifest.txt` lists the exact contents and hashes.

What a payload contains (`scripts/package-enhanced.sh` writes it):

- the `limina-kernel-16k` RPM, its `-devel` (DKMS builds the GPU module against it) and
  `srpms/limina-kernel-16k-source.tar.gz`, the kernel's source reference;
- `limina-gpu-drivers/`: the vendor drivers under `drivers/opt/limina/<wire>/`, the virtio-gpu
  module as one DKMS package carrying a source tree per supported kernel series, and the drivers'
  own `install.sh` (`guest/gpu-drivers/install.sh`, which also works alone on any distro);
- `limina-agent` and `limina-agent-session` (musl static, cross-built on the host) and their units;
- `install-enhanced.sh` and `manifest.txt`.

What the installer does, each step additive and idempotent (partial states are normal):

0. **migrates a guest off the RPM model** where it finds one: lifts the mesa versionlocks, removes
   the local dnf repo (`/etc/yum.repos.d/limina-guest-tools.repo`) and
   `/etc/environment.d/90-limina-zink.conf`, and `dnf distro-sync 'mesa-*'` returns `.limina` mesa
   to the distro's builds (needs the network; without it our mesa stays, unlocked, until a re-run);
1. installs the 16k kernel and its `-devel` with `rpm -i`, beside stock and the previous enhanced
   kernel, and versionlocks the stock kernel packages;
2. runs `limina-gpu-drivers/install.sh` (the drivers under `/opt/limina/<wire>/`, linked into
   glvnd's EGL vendor directory, `/etc/vulkan/icd.d/limina-venus.json` and the implicit layer,
   libgbm's backend directory, and `LIBVA_DRIVERS_PATH` in `/etc/environment.d/90-limina-va.conf`)
   and builds the virtio-gpu module for the 16k kernel explicitly, then its initramfs;
3. removes the `clipboard@limina` gnome-shell extension where present;
4. installs the agents and their units (SELinux-relabeled);
5. arms GRUB's **one-shot trial** of the 16k kernel — reaching the desktop auto-promotes it to the
   permanent default — and keeps the previous kernels as fallbacks.

**Building a payload**, from the repo root:

```sh
cargo xtask enhanced                  # 16k kernel RPM + -devel → target/enhanced-rpms/kernel
scripts/build-gpu-drivers.sh          # vendor drivers from [kosmickrisp-guest] → target/gpu-drivers
scripts/package-gpu-drivers.sh        # + the DKMS module → target/gpu-drivers/limina-gpu-drivers
NOTE="what changed" scripts/package-enhanced.sh rNN   # → payload/limina-guest-tools-f44-rNN.tar.zst
```

Rebuild only what changed: when the kernel did not move, skip its step and let `package-enhanced.sh`
take the previous output from `target/enhanced-rpms/kernel` (`KERNEL_DIR` points elsewhere). A
revision is never rebuilt in place. Then
deliver it — `scripts/provision/deliver-payload.sh <payload> <image>...` (backup, boot, install,
verify, poweroff, per image) — and update §Component versions. `scripts/provision/deliver-metal-transport.sh`
installs the GPU drivers alone (no kernel, no agents) onto an image such as `metal.test`.

**Deploy host-first.** The drivers under `/opt/limina/<wire>/` speak wire version `<wire>` to the
host, and venus and virgl assume the host renderer pinned beside them in
`third_party/manifest.toml`: update the app before (or with) the payload.

Copying a payload into a guest is never the slow part of a delivery: 440 MB took under 5 s over
the slowest NAT limina has had.

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
- **`~/rpmbuild/RPMS` is rpmbuild's accumulating output directory.** A build that copies it
  wholesale into an uncleaned `$OUT` ships RPMs it did not produce (one payload carried four
  kernels). `build-kernel-rpm.sh` collects by exact NEVR and cleans `$OUT`, and
  `package-enhanced.sh` refuses a kernel directory holding more than one kernel RPM.
- **GRUB prints `bli.c:set_loader_device_part_uuid:123:unable to determine partition UUID of boot
  device` on every Fedora boot, on any firmware.** Fedora's aarch64 images carry an MBR (DOS)
  partition table, and GRUB's `bli` module, which publishes `LoaderDevicePartUUID` for systemd,
  reads only GPT entries: it fails with "not a GPT partition table" and reports it under the
  generic message. Harmless; the cost is that systemd's GPT auto-discovery has no boot partition.
- **The accessible-derived images ship `org.gnome.shell disable-user-extensions=true`** (origin
  unknown, not our provisioning). On such guests any extension-based tier is blocked. Consider
  clearing it in `make-accessible.sh` at the next respin.
- **A confident code comment is a claim with a timestamp.** synoik's *"INVALID is refused at
  allocation rather than papered over"* read as a live design constraint; it was stale — written
  when LINEAR was forced everywhere — and taking it as current nearly produced a kernel patch to
  defend a decision that had already been abandoned.
- **Stale images cost a day.** Any rebuild of a guest-side component must flow into (a) the payload
  (§Delivery) and (b) a `deliver-payload.sh` pass over the enhanced images, then into the table
  above. On 2026-07-02 every "identical" local repro of a dogfood crash silently ran a guest two
  deliveries behind.

## Images

### Fedora 44 — mirrored image set (in progress, started 2026-06-29)

Mirrors the F43 five-role layout (see the release selector in Conventions); select with
`LIMINA_FEDORA_REL=44`. The stock roles come from Fedora's own image through
`scripts/provision/make-accessible.sh`; the enhanced roles add the payload (§Delivery).

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

- **Base**: CoW clone of `Fedora-Workstation-44.enhanced.test.raw` (16 KiB pages), so it carries
  the enhanced GPU drivers like any enhanced image (GL on virgl, Vulkan on venus, from
  `/opt/limina`).
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
  Reboot the guest, and verify an env var (e.g. `LIBVA_DRIVERS_PATH`) at
  `/proc/<compositor-pid>/environ`, not in the file.

Run it like any enhanced image: `cargo xtask run --disk Fedora-Workstation-44.enhanced.synoik.raw`
(the worker log is per-disk — `/tmp/limina-worker-<disk>.log` — and `/tmp/enhanced-efi-kk-worker.log` is a symlink to whichever VM booted last; `LIMINA_BOOT_LOG=<path>` still overrides).

#### `Fedora-Workstation-44.enhanced.kde.raw` — the KDE Plasma image (added 2026-10-03)

The image for anything that behaves differently under KDE: KWin's Wayland protocols, PowerDevil,
Plasma's own D-Bus services. Built to verify the session helper's idle-inhibit sources
(`spikes/idle-inhibit-sources/`).

- **Base**: CoW clone of `Fedora-Workstation-44.enhanced.test.raw` plus
  `dnf install plasma-desktop plasma-workspace-wayland powerdevil kwin konsole mpv` (Plasma 6.7,
  KWin 6.7.5), with `swayidle` and `wayland-utils` as probes. GDM stays the display manager:
  `/var/lib/AccountsService/users/claude` has `Session=plasma`, so autologin comes up in Plasma.
  The GNOME session is still installed; switch back by editing that file.
- **KWin composites with OpenGL** on virgl (zink on KosmicKrisp). It needs a worker with virglrs
  `96a7b3d` and libkrun `e493221a` or later: an older one never answers a GL query that was not
  ready when first asked, KWin blocks on its render-time query after the first page flip, and
  Plasma never finishes starting (`spikes/kde-kwin-gl-hang/`). `KWIN_COMPOSE=Q` in
  `/etc/environment` falls back to QPainter for an older worker; under QPainter, mpv's GPU video
  outputs abort, so use `mpv --vo=wlshm` there.
- **Test conveniences, not KDE defaults**:
  - screen autolock and lock-on-resume are off (`kscreenlockerrc`), as is turning the display off
    when idle (`powerdevilrc`). An idle test otherwise ends at the lock screen after five minutes,
    and nothing can map a window behind it;
  - `/etc/firefox/policies/policies.json` allows audible autoplay, because Firefox inhibits idle
    only for audible video.

Run it like any enhanced image: `cargo xtask run --disk Fedora-Workstation-44.enhanced.kde.raw`.
Plasma's Wayland socket is `wayland-0`.

L2 tests boot it through `GuestConfig::seated_efi_kde_from_env` (`LIMINA_TEST_DISK_KDE`
overrides): `tests/kde_session.rs` (KWin composites with OpenGL on virgl and the desktop renders)
and `kde_vrend_world_survives_snapshot_restore` in `tests/vrend_session_restore.rs`. The tests
boot it read-only, so the image's own state is what they test — keep it free of anything that
changes how the session comes up.

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
the *base*: build the F45 enhanced image first (the 16k kernel RPM with `FEDORA_REL=45`, a payload,
§Delivery), then point the clone at that instead of `enhanced.test.raw`. Expect the
dep list to be the drift point — it mirrors `synoik.spec.rpkg`'s `BuildRequires` by hand
(the `.rpkg` macros don't expand outside an rpkg checkout, so `dnf builddep` isn't usable),
so re-check it against the spec when the base moves.

#### Rebuilding `enhanced.raw` from the accessible base (validated 2026-07-05)

`install-enhanced.sh` delivers the payload but deliberately does **NOT** resize the disk (it must also
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

# 4. Clean-poweroff, then deliver the current payload (§Delivery).
ssh -p <PORT> claude@127.0.0.1 'sudo systemctl poweroff -i'
scripts/provision/deliver-payload.sh payload/limina-guest-tools-f44-rNN.tar.zst \
  Fedora-Workstation-44.enhanced.raw

# 5. Reboot: GRUB takes the installer's ONE-SHOT trial into the 16k kernel; reaching the
#    desktop auto-promotes it to the permanent default. Verify venus (seated GNOME + Mesa
#    render lines in the worker log), then clean-poweroff again.
LIMINA_DISK=Fedora-Workstation-44.enhanced.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh &

# 6. Reclone the frozen L2 test snapshot from the quiesced (powered-off) image.
cp -c Fedora-Workstation-44.enhanced.raw Fedora-Workstation-44.enhanced.test.raw
```

| Image | Role | Status |
|---|---|---|
| `Fedora-Workstation-44.vanilla.raw.xz` (only the `.xz` is kept; `xz -dk` to materialise) | **Pristine** F44 Workstation aarch64 (official `…44-1.7.aarch64.raw.xz`; Fedora-built → SELinux labels intact, EFI-boots *enforcing* with no relabel loop). Clone source only. | ✅ renamed from `…44.raw` |
| `Fedora-Workstation-44.accessible.raw` | **Stock base**: vanilla + gnome-initial-setup (`claude`) + pubkey + autologin + NOPASSWD sudo + `vulkan-tools` + no-idle-lock gschema + console args + relabel-clear (`make-accessible.sh`). Promoted from the existing `44.boot.raw` (already had user/ssh/autologin/sudo). | ✅ built 2026-06-29 |
| `Fedora-Workstation-44.stock.test.raw` | **Stock-tier L2 image** — frozen CoW snapshot of `accessible` (`DEFAULT` for `LIMINA_FEDORA_REL=44`; also the seated baseline-3D vehicle). | ✅ built; `efi_boots_to_userspace` GREEN 2026-06-29 |
| `Fedora-Workstation-44.enhanced.raw` | **Enhanced base** — `accessible` grown to 40 G + the current payload through `install-enhanced.sh` (§Delivery; versions in §Component versions). **✅ FINALIZED 2026-06-29**: seated GNOME, WebGL 5000-fish ~60fps on venus→KK→Metal (5-signal+pixel verified); mutter 0003 rebased to 50.1 (`ext_data_control_manager` live in `libmutter-18`); limina-agent (native gnu) active+connected; relabel-clean; build cruft removed. Kernel kept Fedora-config **with debug symbols** (no strip — ~7 GiB modules, slower boot, by choice). Now also carries the **L2 test tooling** (glmark2 + apitrace/`eglretrace` GL replay + `/opt/gfxreconstruct/bin/gfxrecon-replay` VK replay) — folded into `make-accessible.sh` going forward; the enhanced *delivery* (`install-enhanced.sh`) does **not** ship these, so a migrated daily-driver guest stays clean. **Respun 2026-07-04 to kernel `7.1.2-limina16k` + mesa `26.1.3-3` (dogfood parity — see the respin note above); versions in this row are the 2026-06-29 baseline.** **REBUILT FRESH 2026-07-05** from `accessible` per the procedure above (the prior `enhanced.raw`/`.test.raw` had accumulated bad state — the 16k kernel failed its `/boot/efi` mount and dropped to the rescue BLS entry; a clean clone+install booted `7.1.2-limina16k` with `/boot/efi` mounted, venus seated on the new KK). | ✅ finalized 2026-06-29; respun 2026-07-04; rebuilt 2026-07-05 |
| `Fedora-Workstation-44.enhanced.test.raw` | **Enhanced-tier L2 image** — frozen CoW snapshot of `enhanced` (`seated_efi_fedora_from_env` for `LIMINA_FEDORA_REL=44`). Refresh: `cp -c Fedora-Workstation-44.enhanced.raw Fedora-Workstation-44.enhanced.test.raw`. **Recloned 2026-07-05 from the fresh rebuild** (see the `enhanced.raw` note). | ✅ **L2 GREEN 7/7 2026-06-29** (venus×3 + replay×3 + reset; replay tooling baked in); recloned 2026-07-05 |
| `Fedora-Workstation-44.tpm.test.raw` | **vTPM L2 image** — CoW clone of `stock.test` with the TPM consumers installed (tpm2-pkcs11 + tools, clevis, tpm2-openssl, opensc, openssl, ssh-tpm-agent v0.9.0 checksum-pinned), nothing else: no `tss` membership and no TPM state (`tpm_fedora_from_env`, `LIMINA_TEST_DISK_TPM`). Make it with `scripts/provision/make-tpm-test-image.sh`; refresh by deleting it and running that again after `stock.test` changes. | ✅ built 2026-10-07 |
| `Fedora-Workstation-44.metal.test.raw` | **Native-context transport L2 image** — `stock.test` (stock kernel `6.19.10-300.fc44`, 4 KiB) + limina's vendor GPU drivers as a guest gets them, from `scripts/provision/deliver-metal-transport.sh` (the payload's own installer): the drivers under `/opt/limina/<wire>/`, found through `/etc/glvnd/egl_vendor.d/10_limina-<wire>.json`, `/etc/vulkan/icd.d/limina-<wire>.json` and libgbm's `virtio_gpu_gbm.so`, and the extended virtio-gpu module through DKMS (`/lib/modules/<krel>/extra/`, initramfs rebuilt). Nothing in the environment selects them: booted with the transport, gnome-shell and GL clients run on it and Vulkan clients see KosmicKrisp first (an implicit layer hides venus's devices behind it); booted without it, current drivers serve the device through our virgl and venus (the `340ed84bf1b` tree in the status cell serves only the transport and leaves the guest stock without it). A client whose `VK_DRIVER_FILES` leaves ours out gets the system's drivers either way. Also carries the perf battery's tooling (`make-accessible.sh`'s glmark2, apitrace and `/opt/gfxreconstruct`, built with `-DGFXRECON_ENABLE_OPENXR=OFF` since gfxreconstruct's OpenXR wants X11 headers the recipe lacks, plus `vkmark`), so both GPU transports are measured on one 4 KiB guest. The worker offers the extension with `--gpu-metal` (a managed VM's `display.metal_transport`) or `LIMINA_GPU_METAL=1`. Refresh the drivers: `scripts/build-gpu-drivers.sh`, `scripts/package-gpu-drivers.sh`, then the delivery, which upgrades in place. | ✅ vendor drivers delivered to `metal.test` from r42's GPU-driver package: `/opt/limina/5` from `[kosmickrisp-guest]` `1b9049a75d7`, module `limina-virtio-gpu/852836da0d9d` in the root and the initramfs (series 6.19, 7.1 and 7.2). These drivers decline a host speaking another wire VERSION's transport and serve that device through virgl and venus. Backup of the previous state: `.bak-pre-metal.raw`. |
| `Fedora-Workstation-44.enhanced.kde.raw` | **KDE Plasma image** — `enhanced.test` + Plasma 6.7 from the F44 repos, autologin into Plasma, KWin on GL compositing (needs the query fix, see its section). See its section above. | ✅ built 2026-10-03; idle-inhibit paths verified |

**All five F44 images above plus `enhanced.synoik` boot with a zero GRUB menu timeout**
(`scripts/provision/trim-boot-delays.sh`, applied 2026-09-16; `make-accessible.sh` bakes the same
step into a base rebuilt from vanilla, and every CoW refresh inherits it). The frozen goldens never
record `boot_success`, so Workstation's menu auto-hide never engaged and every EFI boot sat 5 s in
the menu. The menu is therefore never drawn on these images (GRUB skips it at timeout 0, so the
boot tests key on its `Booting \`Fedora …'` line instead); a pristine `vanilla` clone is the
vehicle when you actually need to reach it. Measured on `stock.test`, spawn to sshd: 15.4 s →
10.6 s.

**zram swap stays on every image, on purpose.** A kernel without the zram module — every
`--kernel`-injected test kernel — waits the full 45 s `dev-zram0.device` job timeout in sysinit
(50 s to sshd instead of 6 s), but the fix is `systemd.zram=0` on the inject command line
(`GuestConfig::enhanced_fedora_from_env`, the fringe inject scripts), not the image: a stock guest
has zram swap, and the balloon guards are written against that — with it removed, `balloon_burst`'s
3 GiB burst OOM-killed at chunk 6.

`Fedora-Workstation-44.boot.raw` (the pre-accessible image) and the decompressed
`Fedora-Workstation-44.vanilla.raw` were **deleted 2026-09-02**; `accessible.raw` supersedes the
first, and `xz -dk Fedora-Workstation-44.vanilla.raw.xz` regenerates the second when a pristine
clone source is needed.

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

## The virglrs harness rig (`harness/vm/disks/`)

The virglrs harness records its corpora from a real guest, so its rig keeps its own copies of
three images. They are ordinary CoW clones of the canonical files here and are **forks from the
moment they are copied**, by the rule under Conventions: no `deliver-payload.sh` finds them, and
the Component versions table above does not describe them. Read a version in the booted rig guest
rather than inferring it.

| Rig disk | Cloned from | What it is for |
|---|---|---|
| `Fedora-Workstation-44.enhanced.synoik.raw` | `enhanced.synoik` | the venus corpora — synoik starts at boot |
| `Fedora-Workstation-44.enhanced.test.raw` | `enhanced.test` | enhanced-tier GNOME, for the venus-from-clients corpora |
| `Fedora-Workstation-44.stock.test.raw` | `stock.test` | classic virgl and the video path |

`harness/vm/make-rig.sh` in virglrs makes them, and nothing about how they are *built* lives
there: an image is produced and refreshed exactly as this document says, and the rig only clones
the result. To stand one up, or to bring a stale rig clone forward:

```bash
cd ~/Projects/virglrs/harness/vm
./make-rig.sh --disks-only          # cp -c from the canonical images in the limina checkout
# a stale clone is brought forward like any other image:
# limina/scripts/provision/deliver-payload.sh <payload> <the rig clone>
```

**Only recording a new corpus needs these.** virglrs's replay layers are VM-free — every pinned
score in `harness/replay/fixtures/` replays from a stored corpus with no guest, no disk and no
GPU-side boot — so a checkout with no images can still run and regress the whole replay suite.
That is why the corpora are published as release assets and these images are not: 75 GB that this
document already explains how to rebuild, against ~100 MB that cannot be rebuilt at all, because a
recording of a guest session is not reproducible.

## The unified build image (`limina-build:fc<FEDORA_REL>`, Fedora 44)

Every **Linux** build runs in one container image — `scripts/build-image/Containerfile`, built on first
use by `scripts/build-image.sh` (rebuild with `FORCE=1`). It bakes the union of all build deps
(rpmbuild, kernel toolchain, meson/ninja + `builddep mesa`/`builddep mutter`, gfxreconstruct's cmake/xcb/X11/wayland set), so the
per-script `dnf install` is gone and builds start instantly. The Fedora release is the `FEDORA_REL`
build arg, **44 by default** — it is the enhanced tier's target, so the RPMs link the sonames of the
release they install onto. Moving the whole toolchain is `FEDORA_REL=45 FORCE=1 scripts/build-image.sh`;
the tag is spelled once, in `build-image.sh`, and consumers source it for `$LIMINA_BUILD_IMAGE`.
Consumers: `build-enhanced-kernel` (`cargo xtask enhanced`, which runs
`provision/f44/build-kernel-rpm.sh` — the same script a booted guest runs), `build-test-kernel`,
`build-testcomp`, `build-gfxreconstruct`. Each mounts its own persistent source/cache `container volume` (the image
carries the toolchain; the volume carries source + incremental state). **Exceptions** (correctly NOT on
this image): the macOS-native builds (`build-app`, `build-hvf-trap-probe`,
`build-test-guest`) emit Mach-O, not Linux; and `build-dbus-guest` stays on Alpine — it extracts a *musl*
dbus for the musl L1 guest, which a glibc image can't produce. Requires Rosetta (Apple `container`'s
BuildKit needs it); install once with `softwareupdate --install-rosetta --agree-to-license`.

Boot the baseline (bare `limina` — the default coexist device advertises venus, which a stock
4 KiB guest can't use and Mesa **degrades gracefully to `kms_swrast`/llvmpipe**, so the desktop
comes up in software regardless; pass `--gpu-software-2d` to force the clean software path with no
venus probing):
```bash
target/debug/limina --window --firmware target/firmware/efi.fd \
  --disk Fedora-Workstation-44.accessible.raw --cpus 4 --ram-mib 6144 --net
```

### Fedora 43 — retired

The F43 image set (`vanilla` + `.xz`, `accessible`, `stock.test`, `enhanced`, `enhanced.test`) was
deleted on 2026-09-02 to reclaim disk; F44 has been the dev, dogfood and test family since the
2026-08-15 default flip, and nothing in the harness or scripts names an F43 image any more
(`LIMINA_FEDORA_REL=43` would need the set rebuilt from a fresh Fedora download, per the F44
procedure above). The `limina-build` container image below is unrelated and still current.

## Credentials

- **F43 family:** user `claude`, password `claudiusrobotus`; the host's default pubkey is in
  `claude`'s `authorized_keys` (passwordless `ssh -o BatchMode=yes`), and `claude` has passwordless
  `sudo`. `sshd` enabled by default.
- **F44 `accessible.raw` (and every clone of it):** `claude` user (password `claudiusrobotus`), autologin on. `sshd` was enabled
  post-setup (`sudo systemctl enable --now sshd`). The host pubkey is in `authorized_keys`
  (passwordless `ssh -o BatchMode=yes`), and `claude` has NOPASSWD sudo
  (`/etc/sudoers.d/90-claude-nopasswd`) — matching the F43 dev convenience. Stock 4 KiB kernel
  (`6.19.x-300.fc44.aarch64`), `getconf PAGE_SIZE` = 4096.

## SSH access

Boot with `--net` (vmnet shared with a user-mode NAT to fall back on, no root) and SSH into the
guest. The supervisor logs the **exact command** at startup — read the port from it, don't assume
2222:

```
guest SSH forward ready: ssh -p N <user>@127.0.0.1
```

The worker forwards `127.0.0.1:<PORT> → guest:22` through limina-netd, whichever side the NIC is on
(`docs/design/multi-vm-networking.md` §3.5). The host `<PORT>` **auto-allocates from 2222 upward**
(the first free loopback port), so it's 2222 for a lone VM but 2223+ when 2222 is already taken. Pin
it with `--ssh-port <1024-65535>` (requires `--net` or `--net-netd`; errors if the port is busy). Run
**two or more VMs at once** by leaving `--ssh-port` off on each (each grabs the next free port — read
each VM's own startup log) or by pinning distinct ports. The worker log is the host-side network
oracle: `network: …` lines say which side the NIC is on and why, and with
`RUST_LOG=warn,limina_netd=info` netd logs its leases and DNS answers.

Wait for the SSH banner (~10–15s post-boot), then (substitute the logged port for `<PORT>`):
```bash
ssh -p <PORT> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1
```
user `claude` / password `claudiusrobotus`, passwordless sudo. The full operational SSH recipe +
harness builders (`GuestConfig::with_net` / `with_ssh_port`) live in the `limina-fedora-access`
agent memory.

A run started over ssh or from a script should use `--detach` (`docs/dev-onboarding.md` §3, *Run
unattended*): it returns at once with the supervisor's pid, and its log — `<disk>.limina.log` by
default — carries the same `guest SSH forward ready` line, so `scripts/wait-guest-ssh.sh` waits on
it unchanged. Give the script the printed pid as its third argument and it asks that supervisor for
the port (`limina ssh-port <pid>`) instead of reading the log's last forward line.

### Root on a guest you hold no credentials for

systemd reads SMBIOS OEM strings of the form `io.systemd.credential:<name>=<value>` as system
credentials, and its `ssh.authorized_keys.root` credential becomes `/root/.ssh/authorized_keys`
at boot. So a host key can be handed to root without touching the image:

```bash
limina --firmware target/firmware/efi.fd --disk <clone.raw> --net \
  --smbios-oem-string "io.systemd.credential:ssh.authorized_keys.root=$(cat ~/.ssh/id_ed25519.pub)"
ssh -p <PORT> -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1
```

Verified 2026-10-10 on a clone of `Fedora-Workstation-44.stock.test.raw` with a key generated for
the run: root login worked, `systemd-creds --system list` showed `ssh.authorized_keys.root`, and
Fedora's default `PermitRootLogin prohibit-password` admits a key. The guest still has to run
`sshd`, which a fresh Fedora Workstation does not until it is enabled (the test images have it,
see *Credentials*). EFI boots only: `--smbios-oem-string` conflicts with `--kernel`, as libkrun
writes SMBIOS on the firmware path alone. The key lands in `/root/.ssh/authorized_keys`, which a
later boot without the string leaves in place.
