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
- **A delivery reaches the images it was pointed at, and nothing else.** "Applied to the enhanced
  images" means a release's four canonical files in the image directory — `enhanced.raw`,
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
- **Release selector + naming.** The set is mirrored per Fedora release as
  `Fedora-Workstation-<REL>.<role>.raw` (roles in §Images). **`LIMINA_FEDORA_REL`** picks the
  release for the L2 harness (`crates/limina-test`), the run scripts (`run-fedora-window.sh`,
  `run-enhanced.sh`, `run-venus-window.sh`), `make-tpm-test-image.sh` and `perf-compute.sh`; **the
  default is `45`**. Per-image overrides (`LIMINA_TEST_DISK`, `LIMINA_TEST_DISK_ENH`, …) still win.
  The default must be a family the guest components are built for: a suite on an older one
  certifies stale guests (an F43 kernel without `uinput` once failed the vdagent clipboard test
  for no fault of ours). The dogfood guest is on the same release, so a local repro of a dogfood
  report runs on the default images. A new release is §Fedora 45's procedure with its own number.

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
guest with `rpm -q`; the GPU drivers by `/opt/limina/<wire>/MESA_REV` and `dkms status`. The
enhanced rows are what the payload installs (r51) and what `deliver-payload.sh`
verifies on each image (agent hashes, the drivers' `MESA_REV`, no limina mesa RPM or
driver-selection file, the 16k kernel's virtio-gpu resolving to DKMS's module). The F45 rows were
read with `rpm -q`, `dkms status` and `MESA_REV` in clones of the images 2026-11-24, the kernel on
the images themselves 2026-11-27; the dogfood row on the dev VM 2026-11-27. Every enhanced image
boots `7.3.6-limina16k` as its permanent default.
The synoik image never reaches `multi-user.target` (its `plymouth-quit-wait` hangs), so
`limina-kernel-promote.service` never fires there: after its trial boot, promote by hand with the
unit's own two steps (`grubby --set-default=/boot/vmlinuz-<KREL>`, then disable the unit).

| Tier / images | Kernel | Page | Mesa | Mutter | GNOME Shell |
|---|---|---|---|---|---|
| **F45 stock + freeworld VA** (`accessible`, `stock.test`, `tpm.test`, `metal.test`) | `7.3.4-300.fc45` (`7.2.11-300.fc45`, the release kernel, kept as the fallback; on `metal.test` virtio-gpu is limina's DKMS module `d1dc28435cd5`) | 4 KiB | `26.2.7-1.fc45` + `mesa-va-drivers-freeworld-26.2.7-1.fc45` | `51.1-1.fc45` | `51.1` |
| **F45 enhanced** (`enhanced`, `enhanced.test`, `enhanced.synoik`, `enhanced.kde`) | `limina-kernel-16k-7.3.6-1.fc45` (+ `-devel`; `7.3.5-1.fc45` and the stock kernels kept as fallbacks; virtio-gpu is limina's DKMS module `d1dc28435cd5`) | 16 KiB | **stock** `26.2.7-1.fc45` (+ freeworld), plus the vendor drivers under `/opt/limina/5` (`[kosmickrisp-guest]` `c0a873da495`) | `51.1-1.fc45` (stock) | `51.1` (stock) |
| **F45 dogfood deployment** (the user's dev VM, boots systemd-boot) | `limina-kernel-16k-7.1.13-2.fc44` (+ `-devel`; the default; `7.1.13-1` kept, `limina-kernel-16k-7.3.5-1.fc45` + `-devel` installed beside them; virtio-gpu is limina's DKMS module `d1dc28435cd5`) | 16 KiB | **stock** `26.2.7-1.fc45`, plus the vendor drivers under `/opt/limina/5` (`[kosmickrisp-guest]` `c0a873da495`) | `51.1-1.fc45` | `51.1` |

The F45 sessions on top: systemd `262.1-1.fc45` on every image; KWin and Plasma `6.8.3-1.fc45` on
`enhanced.kde`; synoik `9e5bf3ca` on `enhanced.synoik`.

Two facts the table cannot show:

- **The guest agents are not RPMs and so are not in the table.** Every enhanced image carries
  **`limina-agent` 0.6.2** and **`limina-agent-session` 0.1.4**, installed to `/usr/local/bin` with
  their units (payload r51, built from r42's sources). The
  dogfood guest:
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

- **The limina kernel branch carries three drm/virtio commits**: per-scanout rects exposed as
  suggested connector offsets (the guest half of the arrangement relay), and typing +
  context-attaching PRIME-imported dmabufs (without it every software-decoded video frame is
  untyped host-side — `docs/graphics.md` §3.2), both upstream candidates; and a revert of
  7.3's revert of dmabuf import into a 3D context, which upstream withdrew because vrend cannot
  take guest blobs while limina's renderer can (the upstreamable shape is a device capability
  gating that check). The
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

`accessible` carries it, and so every image made from it; verified there that libva opens
`/usr/lib64/dri-freeworld/virtio_gpu_drv_video.so` and that H.264 Constrained-Baseline/Main/High,
HEVC Main and VP9 Profile 0 are advertised as `VAEntrypointVLD`.

**The stock images track Fedora's updates, not the install media.** freeworld is built against
the current Fedora mesa, so it drags mesa forward whenever it is installed, and a stock baseline a
kernel behind decides what the stock-tier tests can cover (free page reporting is gated on the
guest kernel). `scripts/provision/update-stock.sh <image>...` brings an image to the current
packages in place (CoW backup, boot, `dnf upgrade`, trim, reboot onto the new kernel and verify).
Two traps it handles: GNOME Software downloads gigabytes of offline updates into
`/var/lib/dnf/offline` on every boot of the golden, which fills the 11 GiB root until btrfs has no
room for metadata and the transaction fails midway; and while the host is on AC, udisks2's `%post`
fails writing the battery's `uevent` (the SBS device NAKs the time-to-empty registers), which dnf5
reports as a failed transaction although every package installed — re-run the script to finish.
`tpm.test` is remade from the refreshed `stock.test` (`make-tpm-test-image.sh`), and `metal.test`
is updated in place by the same script. `vanilla` stays the pristine install.

## Delivery

The enhanced tier is a **payload**, `payload/limina-guest-tools-f<REL>-rNN.tar.zst` (gitignored;
`<REL>` is the release its kernel RPM is built for),
which `install-enhanced.sh` applies inside a running stock or enhanced guest. The guest keeps its
distro's Mesa; nothing of the distro's is replaced, locked or shadowed.

**Current payload**, with the GPU drivers `/opt/limina/5` at `[kosmickrisp-guest]`
`c0a873da495`, DKMS `limina-virtio-gpu` `d1dc28435cd5` (series 6.19 and 7.2 from stable tags, 7.1
and 7.3 from the `[linux]` fork, whose 7.3 sources also serve stock 7.3 kernels), `limina-agent`
0.6.2 and `limina-agent-session` 0.1.4; its `manifest.txt` lists the exact contents and
hashes:

- **F45: `payload/limina-guest-tools-f45-r51.tar.zst`**, `limina-kernel-16k-7.3.6-1.fc45`
  (+ `-devel`). Applied to the four F45 enhanced images (`.bak-pre-r51.raw` CoW backups); its GPU
  drivers alone to `metal.test`. Its GPU drivers are r50's unchanged, so the DKMS series line in
  its manifest names the fork's 7.3.5-based rev: the fork's `drivers/gpu/drm/virtio` is identical
  on both.

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
   kernel, and versionlocks the stock kernel packages. Our RPMs are unsigned, and rpm 6 (Fedora 45
   on) refuses unsigned packages by default, so they are verified by digest
   (`_pkgverify_level digest`);
2. runs `limina-gpu-drivers/install.sh` (the drivers under `/opt/limina/<wire>/`, linked into
   glvnd's EGL vendor directory, `/etc/vulkan/icd.d/limina-venus.json` and the implicit layer,
   libgbm's backend directory, the newest tree's `lib/` on the loader path
   (`/etc/ld.so.conf.d/limina-gpu-drivers.conf`), and `LIBVA_DRIVERS_PATH` in
   `/etc/environment.d/90-limina-va.conf`). The loader path is load-bearing: libgbm loads our
   backend through its link in the system's backend directory, where the backend's
   `$ORIGIN/..` RUNPATH cannot reach its `libgallium`, and a backend that fails to load makes
   libgbm fall back to the stock one without a word;
   and builds the virtio-gpu module for the 16k kernel explicitly, then its initramfs;
3. removes the `clipboard@limina` gnome-shell extension where present;
4. installs the agents and their units (SELinux-relabeled);
5. arms GRUB's **one-shot trial** of the 16k kernel — reaching the desktop auto-promotes it to the
   permanent default — and keeps the previous kernels as fallbacks.

**Building a payload**, from the repo root:

```sh
FEDORA_REL=45 cargo xtask enhanced    # 16k kernel RPM + -devel → target/enhanced-rpms/kernel
scripts/build-gpu-drivers.sh          # vendor drivers from [kosmickrisp-guest] → target/gpu-drivers
scripts/package-gpu-drivers.sh        # + the DKMS module → target/gpu-drivers/limina-gpu-drivers
NOTE="what changed" scripts/package-enhanced.sh rNN   # → payload/limina-guest-tools-f<REL>-rNN.tar.zst
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

### Fedora 45 — the image set

The default family (`LIMINA_FEDORA_REL=45`). Every role is built from Fedora's own release image
with nobody at the window, so the whole set can be remade from a download. The stock roles come
from `scripts/provision/make-accessible.sh`; the enhanced roles add the payload (§Delivery).

| Image | Role |
|---|---|
| `Fedora-Workstation-45.vanilla.raw.xz` | **Pristine** F45 Workstation aarch64, Fedora's `Fedora-Workstation-Disk-45-1.4.aarch64.raw.xz` (sha256 `07ab3377cee9d4bbc15e7203214594f8cfdb58a9a57e195aa0ff183cf81ea0a8`). Only the `.xz` is kept; `xz -dk` gives the clone source. Fedora-built, so its SELinux labels are intact and it EFI-boots enforcing. |
| `Fedora-Workstation-45.accessible.raw` | **Stock base**: vanilla + the `claude` user, autologin into a seated GNOME session, NOPASSWD sudo and sshd (`bootstrap-user.sh`), then `make-accessible.sh` (L2 test tooling, the vkstill toolchain, `gfxreconstruct` at `765c3d6`, zero GRUB timeout), `update-stock.sh` and `install-freeworld-va.sh`. 11 G root. |
| `Fedora-Workstation-45.stock.test.raw` | **Stock-tier L2 image**: a CoW clone of `accessible` (`fedora_from_env`; the seated baseline-3D vehicle too). |
| `Fedora-Workstation-45.tpm.test.raw` | **vTPM L2 image**: `stock.test` with the TPM consumers (tpm2-pkcs11 and tools, clevis, tpm2-openssl, opensc, openssl, ssh-tpm-agent checksum-pinned) and nothing else: no `tss` membership and no TPM state (`tpm_fedora_from_env`, `LIMINA_TEST_DISK_TPM`). Made by `scripts/provision/make-tpm-test-image.sh`; remake it after `stock.test` changes. |
| `Fedora-Workstation-45.metal.test.raw` | **Native-context transport L2 image**: `stock.test` grown to 20 G (dracut runs out of room rebuilding the initramfs at the base's size) + `vkmark`, then limina's vendor GPU drivers as a guest gets them, by `scripts/provision/deliver-metal-transport.sh`: the drivers under `/opt/limina/<wire>/`, found through `/etc/glvnd/egl_vendor.d/10_limina.json`, `/etc/vulkan/icd.d/limina-<wire>.json`, libgbm's `virtio_gpu_gbm.so` and the loader path (`/etc/ld.so.conf.d/limina-gpu-drivers.conf`), and the extended virtio-gpu module through DKMS, in the root and the initramfs. Nothing in the environment selects them: booted with the transport (`--gpu-metal`, a managed VM's `display.metal_transport`, or `LIMINA_GPU_METAL=1`), gnome-shell and GL clients run on it and Vulkan clients see KosmicKrisp first; booted without it, the same drivers serve the device through our virgl and venus. Carries the perf battery's tooling, so both GPU transports are measured on one 4 KiB guest. |
| `Fedora-Workstation-45.enhanced.raw` | **Enhanced base**: `accessible` grown to 40 G + `vkmark` and `fio`, then the payload (§Delivery; versions in §Component versions). The 16k kernel keeps Fedora's config with debug symbols, which is why it needs the room. |
| `Fedora-Workstation-45.enhanced.test.raw` | **Enhanced-tier L2 image**: a CoW clone of the powered-off `enhanced` (`seated_efi_fedora_from_env`). |
| `Fedora-Workstation-45.enhanced.synoik.raw` | **Vulkan compositor image**: a CoW clone of the powered-off `enhanced` + synoik as the session (below). |
| `Fedora-Workstation-45.enhanced.kde.raw` | **KDE Plasma image**: a CoW clone of the powered-off `enhanced` + Plasma as the autologin session (below). |

Every image boots with a zero GRUB menu timeout (`make-accessible.sh`, and every CoW clone
inherits it): the goldens never record `boot_success`, so Workstation's auto-hide would never
engage and every EFI boot would sit 5 s in the menu. The boot tests key on GRUB's
`Booting \`Fedora …'` line instead; a `vanilla` clone is the vehicle when you need the menu.

**zram swap stays on every image, on purpose.** A kernel without the zram module (every
`--kernel`-injected test kernel) waits the full 45 s `dev-zram0.device` job timeout, but the fix is
`systemd.zram=0` on the inject command line (`GuestConfig::enhanced_fedora_from_env`), not the
image: a stock guest has zram swap, and the balloon guards are written against it.

#### Building the set

All host commands from the repo root; every boot is the default vehicle with networking, and
`scripts/wait-guest-ssh.sh <worker-log> <timeout> <boot-pid>` gives its port.

1. **`accessible`.** Decompress the release image into `Fedora-Workstation-45.accessible.raw`. A
   fresh Workstation image has no user and no running sshd, so boot it once with root's keys and an
   sshd drop-in as systemd credentials (`--smbios-oem-string
   io.systemd.credential.binary:ssh.authorized_keys.root=<base64 keys>` and
   `io.systemd.credential.binary:systemd.unit-dropin.multi-user.target=<base64 of
   "[Unit]\nWants=sshd.service\n">`, passed through `LIMINA_EXTRA_ARGS`; the binary form, because
   neither value survives the vehicle's word splitting as text) and run
   `ssh -p <PORT> root@127.0.0.1 'bash -s' < scripts/provision/bootstrap-user.sh`. Reboot (GDM
   leaves its initial-setup greeter only when it next starts and finds a user), then run
   `make-accessible.sh` as `claude`, and power off. Then `scripts/provision/update-stock.sh` and
   `scripts/provision/install-freeworld-va.sh` on it.
2. **`stock.test`**: `cp -c` of `accessible`. **`tpm.test`**:
   `scripts/provision/make-tpm-test-image.sh`.
3. **`metal.test`**: `cp -c` of `stock.test`, `qemu-img resize -f raw … 20G`, then in the guest
   `growpart /dev/vda 3 && btrfs filesystem resize max /` (from `cloud-utils-growpart`) and
   `dnf install vkmark`; power off and run `deliver-metal-transport.sh` on it.
4. **`enhanced`**: `cp -c` of `accessible`, resized to 40 G and grown the same way, with
   `dnf install vkmark fio`; power off and run `deliver-payload.sh <payload> <image>`. Boot it once
   to the desktop: GRUB takes the installer's one-shot trial of the 16k kernel and reaching the
   desktop promotes it. Then **`enhanced.test`**, **`enhanced.synoik`** and **`enhanced.kde`** are
   each a `cp -c` of the powered-off `enhanced`.
5. **The session images**: boot `enhanced.synoik` (or `enhanced.kde`), copy its session script in
   and run it as `claude`, reboot to check the session, power off.

`cp -c` is load-bearing throughout: it CoW-clones instantly, and an image boots in place.

#### Perf tooling

The enhanced and metal images carry the whole measurement battery, so a perf pass needs no ad-hoc
installs (which perturb the thing being measured, and once made a missing binary read as a driver
fault):

| tool | provenance | used by |
|---|---|---|
| `glmark2` | Fedora (`make-accessible.sh`) | `glmark2-wayland-venus`, `glmark2-display-*`, the ledger backend guard |
| `apitrace` (`eglretrace`) | Fedora (`make-accessible.sh`) | `gl-replay-venus`, `gl-replay-llvmpipe` |
| `vkmark` | Fedora, the **distro** binary: compare it only against `vkmark-default-venus` rows, never `vkmark-3scene-venus` | `vkmark-default-venus` |
| `fio` | Fedora | virtio-blk path numbers |
| `gfxrecon-replay` | `/opt/gfxreconstruct`, built in the guest by `make-accessible.sh` at upstream `765c3d6` (`GFXR_REV`) with `-j2`, since `-j4` OOMs a 4 GiB guest | `vk-replay-venus-headless`, and the L2 `venus_vk_replay_matches_lavapipe_reference` |

**Pin gfxreconstruct by commit, and move the pin with the test.** Its release tags lag main far
behind; a tag build replays perfectly but prints `Replay FPS` where `venus_replay.rs` greps for
`Measured FPS`, so the test fails on wording with nothing wrong underneath. `perf-ledger.sh` aborts
rather than drop the `vk-replay` row when the binary is missing (`LIMINA_PERF_SKIP_VK=1` overrides
deliberately). The tests replay with `--wsi headless`, so the in-guest build needs neither XCB nor
Wayland.

#### `enhanced.synoik` — the Vulkan compositor image

The canonical image for anything that needs a **Vulkan compositor**. A whole class of host bugs is
only reachable when the compositor imports client dmabufs through Vulkan/venus; mutter composites
with GL, so under mutter those paths are never exercised and every run is a **false negative**
(the vrend/KK stride shear in `spikes/vrend-stride-2026-08-14` is the worked example).

`scripts/provision/fedora/install-synoik-session.sh` runs in the guest and is idempotent (re-run it
to update synoik): it installs the build deps (synoik's `BuildRequires`, mirrored by hand because
the `.rpkg` spec's macros don't expand outside an rpkg checkout, plus `glslang`, which the spec
omits), clones or updates `~/synoik`, builds it and calls synoik's own `install-test-session.sh`,
the source of truth for the `org.gnome.Shell@user.service` drop-in and the private GSettings
schema dir. It also installs the helper's `LIMINA_CLIPBOARD_IGNORE_VDAGENT=1` drop-in (§Clipboard).
GDM's autologin brings the normal GNOME session up on synoik. Iterate by rebuilding in the guest
and logging out and in: the unit runs whatever is at `target/release/synoik`.

Two traps, each of which has cost a run:

- **synoik's Wayland socket is `wayland-1`, not `wayland-0`** (gdm holds `-0`). A client with a
  hardcoded `WAYLAND_DISPLAY=wayland-0` never connects, so nothing is imported and every
  measurement reads clean. Discover the socket (`ls /run/user/1000/wayland-*`).
- **Restarting GDM does not re-read `/etc/environment.d`**: the systemd user manager survives it.
  Reboot, and read an env var at `/proc/<compositor-pid>/environ`, not in the file.

Run it like any enhanced image: `cargo xtask run --disk Fedora-Workstation-45.enhanced.synoik.raw`.

#### `enhanced.kde` — the KDE Plasma image

The image for anything that behaves differently under KDE: KWin's Wayland protocols, PowerDevil,
Plasma's own D-Bus services. `scripts/provision/fedora/install-kde-session.sh` runs in the guest as
`claude` and is idempotent: it installs Plasma (`plasma-desktop plasma-workspace-wayland powerdevil
kwin konsole mpv`, with `swayidle` and `wayland-utils` as idle-inhibit probes) and makes it the
autologin session through AccountsService (`Session=plasma`). GDM stays the display manager and
GNOME stays installed; set the session back to switch.

- **KWin composites with OpenGL**: on zink over the Metal transport when the host offers it, on
  virgl (zink on KosmicKrisp host-side) when it does not. The virgl path needs a worker with virglrs
  `96a7b3d` and libkrun `e493221a` or later: an older one never answers a GL query that was not
  ready when first asked, KWin blocks on its render-time query after the first page flip and
  Plasma never finishes starting (`spikes/kde-kwin-gl-hang/`).
- **Test conveniences, not KDE defaults**: no screen autolock, no lock on resume and no display-off
  when idle (an idle test otherwise ends at the lock screen, and nothing can map a window behind
  it); Firefox may autoplay audible media, because it inhibits idle only for audible video.

L2 tests boot it through `GuestConfig::seated_efi_kde_from_env` (`LIMINA_TEST_DISK_KDE`
overrides), always without the Metal transport so that KWin is on virgl: `tests/kde_session.rs`
and `kde_vrend_world_survives_snapshot_restore` in `tests/vrend_session_restore.rs`. They boot it
read-only, so the image's own state is what they test: keep it free of anything that changes how
the session comes up. Plasma's Wayland socket is `wayland-0`.

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

## The unified build image (`limina-build:fc<FEDORA_REL>`)

Every **Linux** build runs in one container image — `scripts/build-image/Containerfile`, built on first
use by `scripts/build-image.sh` (rebuild with `FORCE=1`). It bakes the union of all build deps
(rpmbuild, kernel toolchain, meson/ninja + `builddep mesa`/`builddep mutter`, gfxreconstruct's cmake/xcb/X11/wayland set), so the
per-script `dnf install` is gone and builds start instantly. The Fedora release is the `FEDORA_REL`
build arg, **45 by default**, the release of the test images and the dogfood guest: the RPMs link
the sonames of the release they install onto (`FORCE=1 scripts/build-image.sh` rebuilds an image). The tag is spelled once, in `build-image.sh`, and
consumers source it for `$LIMINA_BUILD_IMAGE`.
Consumers: `build-enhanced-kernel` (`cargo xtask enhanced`, which runs
`provision/fedora/build-kernel-rpm.sh` — the same script a booted guest runs), `build-test-kernel`,
`build-testcomp`, `build-gfxreconstruct`. Each mounts its own persistent source/cache `container volume` (the image
carries the toolchain; the volume carries source + incremental state). **Exceptions** (correctly NOT on
this image): the macOS-native builds (`build-app`, `build-hvf-trap-probe`,
`build-test-guest`) emit Mach-O, not Linux; and `build-dbus-guest` stays on Alpine — it extracts a *musl*
dbus for the musl L1 guest, which a glibc image can't produce. Requires Rosetta (Apple `container`'s
BuildKit needs it); install once with `softwareupdate --install-rosetta --agree-to-license`.

Boot the baseline (bare `limina` — the default coexist device advertises venus, and a stock 4 KiB
guest uses it: measured 2026-10-30 on a `stock.test` clone with Fedora's Mesa 26.2.5, `vulkaninfo`
lists `Virtio-GPU Venus` and guest zink runs on it, while GL defaults to virgl; pass
`--gpu-software-2d` to force the clean software path with no venus probing):
```bash
target/debug/limina --window --firmware target/firmware/efi.fd \
  --disk Fedora-Workstation-45.accessible.raw --cpus 4 --ram-mib 6144 --net
```

### Retired families

F43's and F44's whole sets are deleted, to reclaim disk. Nothing in the harness or scripts names them; a family
is rebuilt from a fresh Fedora download by §Fedora 45's procedure.

## Credentials

Every image built from `accessible`: user `claude`, password `claudiusrobotus`, autologin on,
NOPASSWD sudo, `sshd` enabled, and the host's public keys in `authorized_keys` (passwordless
`ssh -o BatchMode=yes`), all from `scripts/provision/bootstrap-user.sh`. Stock-tier images run
Fedora's 4 KiB kernel (`getconf PAGE_SIZE` = 4096); enhanced ones the 16 KiB one.

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
