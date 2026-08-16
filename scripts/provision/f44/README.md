# In-guest enhanced-tier builds for a Fedora 44 dogfood guest

These scripts build the limina **enhanced-tier** RPMs (16 KiB kernel, venus mesa, patched
mutter) **natively, inside a booted basic-tier Fedora 44 guest** — no Apple `container`, no
Rosetta, no macOS host involved. Run them after the guest's first basic boot; then
`install-enhanced.sh` consumes the result.

## Why in-guest (not the `container` builders under `scripts/`)

The macOS `container` builders (`scripts/build-{kernel,mesa,mutter}-rpm.sh`) hardcode the
`limina-build:fc43` image (Fedora **43**); their `FEDORA_REL=44` knob is a no-op (it only picks
the image tag). Building inside the F44 guest is what actually targets F44, and it gets the
right things for free:
- `rpmbuild` stamps `.fc44` as `%{?dist}` automatically;
- the binaries link **F44's** sonames (`libLLVM`, `libdisplay-info`, the gnome-shell/libmutter ABI);
- `dnf download --source` returns **F44's own** SRPMs;
- it's aarch64-native (no Rosetta).

## Source-of-truth: Fedora F44 SRPMs + a minimal limina delta

Each component is **Fedora's own F44 package + the smallest limina change**, not an upstream
fork:
| Component | Source | limina delta | Pinned? |
|---|---|---|---|
| kernel | Fedora F44 kernel **source + config** | `CONFIG_ARM64_16K_PAGES=y` (+ `patches/linux/*` if not already upstream) | versionlocked |
| mesa | F44 **mesa SRPM** (26.1.x) | the `patches/mesa-guest/` series (6 venus commits exported from the `liminavm/mesa` `limina-guest` fork branch — present-fix, 16-bit-unorm drop, stub-instance degrade, ICD TLS pin, ring-loss DEVICE_LOST, freelist capacity); zink rows retired with drop-guest-zink 2026-08-04 | versionlocked |
| mutter | F44 **mutter SRPM** (50.x) | `patches/mutter/0001-0003` rebased onto 50.x | NOT locked (tracks gnome-shell) |

Because the mesa/mutter builds rebuild the **same version** Fedora ships (just `+.limina`
Release + our patches), they replace stock cleanly — no soname mismatch. (Contrast the older
`scripts/build-mesa-rpm.sh`, which jumped to upstream 26.2 on an F43 that shipped 25.x and so
*had* to manage a soname swap.)

## Prerequisites in the guest
- The limina **repo present in the guest** (these scripts read `patches/` relative to
  themselves). Either `git clone` it, or share it from the host: `limina --share
  limina=/path/to/limina:ro` then `mount -t virtiofs limina-limina /mnt/limina`.
- **Networking** (`limina --net`) — the builds `dnf download --source`, `dnf builddep`, and
  clone kernel source.
- Disk + RAM: the kernel build is heavy (several GB, tens of minutes). Give the guest a roomy
  `--memory` and a writable, **persistent** disk (a throwaway CoW clone loses the result).
- `sudo` (the basic accessible guest has passwordless sudo).

## Run

```bash
# one shot: build all three + the agent, assemble a payload, print the install command
scripts/provision/f44/build-all.sh

# or individually (each writes RPMs under ~/limina-build/<component>/)
scripts/provision/f44/build-kernel-rpm.sh
scripts/provision/f44/build-mesa-rpm.sh
scripts/provision/f44/build-mutter-rpm.sh
```

Then, as root, install from the assembled payload and reboot:
```bash
sudo scripts/provision/install-enhanced.sh ~/limina-guest-tools
sudo reboot
```

## Patch-rebase risks (expect to iterate)
- **mesa `0009/0010`** (the venus WSI present-fix — *the* black-screen fix) were authored on
  mesa 26.1.0; F44 ships 26.0.x. The build adds them via the spec so a non-applying patch
  **fails the build loudly** (rather than silently skipping → black screen). If it fails,
  rebase the patch onto F44's mesa and re-run.
- **mutter: none since 2026-07-11** — the payload ships NO mutter, and since 2026-08-15 no
  gnome-shell extension either (`clipboard@limina` was retired with #37 step 4). The GNOME
  clipboard is stock `spice-vdagent`; stock mutter and stock gnome-shell both stay stock. (History: `0001`+`0002`+`0003` applied clean to mutter 50.1 on 2026-06-29;
  `0001`/`0002` were later retired as root-caused-elsewhere, `0003` is kept unshipped for
  ext-data-control experiments via the optional `build-mutter-rpm.sh`.)
- **kernel `patches/linux/0001-0003`** (drm/virtio scanout) may already be upstream in F44's
  kernel; they're applied tolerantly (skipped if they don't apply).

## ✅ The venus *desktop* on F44 — VALIDATED end-to-end (2026-06-29)

These in-guest builds produce the guest RPMs; a *working accelerated GNOME desktop* on F44 also
depends on **KosmicKrisp** (the host Vulkan-on-Metal driver, built on macOS —
`patches/kosmickrisp/`, `docs/drivers/kosmickrisp.rst`). The full F44 enhanced stack is **validated
working**: the 16k kernel + venus mesa + patched mutter 50.1 render the seated GNOME desktop at
~60fps (venus→KK→Metal), and the venus L2 suite is **GREEN 7/7** (venus×3 + replay×3 + reset). The
feared `kk_encoder.c:299` render-pass-restart assert that mutter-50 was thought to trigger **never
fired** — it was a mess-era myth: **no KK fix was needed**, and KK patches `0002`/`0003` (the F43
set) suffice for F44 too. The 16k kernel + venus mesa + patched mutter all light up together.
Known limitation: GLX/Xwayland apps present black on venus (the X11 kopper present path);
Wayland-native GL is fine.
