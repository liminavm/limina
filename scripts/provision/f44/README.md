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
| mesa | F44 **mesa SRPM** (26.0.x) | venus present-fix `patches/mesa/0009,0010` (+ zink `0001`) | versionlocked |
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
- **mutter `0001`** (cogl #32 stencil-clip degrade) touches `cogl-framebuffer.c` / clutter,
  which churns across GNOME 49→50 — most likely to need a rebase onto mutter 50.x.
- **kernel `patches/linux/0001-0003`** (drm/virtio scanout) may already be upstream in F44's
  kernel; they're applied tolerantly (skipped if they don't apply).

## ⚠️ The venus *desktop* on F44 also needs the host side

These in-guest builds produce correct guest RPMs, but a *working accelerated GNOME desktop* on
F44 additionally depends on **KosmicKrisp** (the host Vulkan-on-Metal driver, built on macOS —
`patches/kosmickrisp/`, `docs/drivers/kosmickrisp.rst`). As of the committed tree, KK patches
`0002`/`0003` make the **F43 (mutter-49.5)** desktop render and stay up, but the README's TODO
lists an open `kk_encoder.c:299` render-pass-restart assert that **mutter-50 (F44)** triggers.
So: with these RPMs the guest boots the 16k kernel and venus enumerates, but the F44 desktop is
only as healthy as the host KK side. Confirm the current state of that KK tree before expecting
a clean F44 venus desktop. The 16k kernel + venus mesa are useful regardless; mutter is staged
and lights up when the host side is resolved.
