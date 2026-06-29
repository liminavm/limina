# Dogfooding: migrating a Parallels Fedora VM into limina

Goal: take an **existing stock Fedora desktop VM running in Parallels** (on "Mac A") and
run it under **limina** on a second Apple-Silicon machine ("Mac B", the build/dev Mac's
sibling), as a daily driver, to surface real-world weaknesses.

This is a *migration of the real, data-bearing disk* (not a fresh install) so we exercise
the actual migration story and feel every rough edge. The three workstreams below form one
pipeline:

```
[Mac A: Parallels]                       [Mac B: only limina.app]
  prep guest IN-PLACE   ── convert ──▶   copy raw disk + copy limina.app
  (virtio_mmio!)         (qemu-img)              │
                                                 ▼
                                        basic-tier daily driver  ──▶  enhanced upgrade
                                        (--window --disk)              (guest-tools payload
                                                                        over --share)
```

The biggest *potential* trap is an initramfs detail (§1.1) — but, verified empirically, it is
usually a **non-issue**; what matters is knowing the relevant module is `virtio_mmio`, **not**
the `virtio_pci` that every generic Parallels→KVM guide tells you to add.

## Decisions (and why)

| Decision | Choice | Why |
|---|---|---|
| Migrate the real disk vs. fresh install + rsync `/home` | **Migrate the real disk** | Highest-fidelity dogfooding; forces us to live the (currently undocumented) import story. Fall back to fresh-install only if `gdisk` shows the disk is MBR/BIOS (won't EFI-boot). |
| Fedora 44 now vs. switch to F43 | **F44, basic tier now** | Stock F44 is a verified limina baseline (`docs/images.md`). Enhanced (16k/venus) on F44 is **blocked** by the GNOME 49→50 mutter/cogl regression — so basic-tier is the daily driver, and enhanced-tier work proceeds separately (§4 + the F44 enhanced build prep). |
| First-boot observability | **Windowed GOP + `--console` serial** | A migration boot is exactly when you want eyes on GRUB + early KMS, and the serial log catches a dracut-shell drop. |
| SELinux relabel | **Migrate as-is (Enforcing); relabel only if it loops** | A Parallels Fedora install is a real Fedora install with labels intact; preemptive permissive needlessly weakens a daily driver. |

---

## 1. Prepare the guest — *inside Parallels, before converting* (Mac A)

> All of §1 runs **inside the booted guest in Parallels**, where the kernel and tooling
> are available. Do **not** defer any of it to after the disk has moved.

### 1.0 Survey the disk
```bash
sudo lsinitrd /boot/initramfs-$(uname -r).img | grep -i virtio   # expect virtio_mmio present (generic initramfs)
lsblk -f
cat /etc/fstab
cat /boot/loader/entries/*.conf
cat /etc/default/grub
blkid                                                            # record the root UUID
```

### 1.1 Verify `virtio_mmio` is in the initramfs (it almost certainly already is)
limina presents **all** virtio over **virtio-MMIO** (device-tree), with **no virtio-PCI
transport at all** (`third_party/libkrun/src/devices/src/fdt/aarch64.rs:321`,
`compatible="virtio,mmio"`). The reflex from generic Parallels→KVM guides is to "add
`virtio_pci`" — that is the **wrong** module here.

**Measured empirically** (a live stock-kernel Fedora guest booted under limina, 2026-06-29):
Fedora builds `virtio_mmio` as a **module** (`CONFIG_VIRTIO_MMIO=m`, not built-in) **but ships a
generic `--no-hostonly` initramfs that includes it** — which is precisely why stock Fedora boots
in limina at all (it binds `/dev/vda` over the MMIO transport). So a normally-installed Fedora
guest needs **nothing here**.

The *only* risk: if the Parallels VM's initramfs was regenerated **hostonly** (dracut drops
modules for absent hardware, and Parallels exposes no MMIO devices), `virtio_mmio` could have
been dropped. So **verify, don't pre-emptively rebuild**:
```bash
sudo lsinitrd /boot/initramfs-$(uname -r).img | grep virtio_mmio   # a hit ⇒ done, nothing to do
```
Only if it's **missing**, regenerate generic (NOT `virtio_pci`):
```bash
sudo dracut --regenerate-all --force --no-hostonly
sudo lsinitrd /boot/initramfs-$(uname -r).img | grep virtio_mmio   # confirm present now
```

### 1.2 Add a limina-visible console
On limina's EFI path the kernel cmdline is **GRUB-owned**; FDT bootargs are ignored, so
limina cannot inject `console=` (`docs/roadmap.md:222`, `scripts/prepare-efi-image.sh`). A
Parallels image typically has `console=tty0`/`hvc0`, so early boot would be **silent** on
limina. Add both the GOP framebuffer and the PL011 serial limina captures:
```bash
sudo grubby --update-kernel=ALL --args="console=tty0 console=ttyAMA0"
```

### 1.3 (Recommended) Remove Parallels Tools, then regenerate once more
Inert on limina (the `prl_*` modules find no Toolgate; GNOME is Wayland so `prl_vid` is
ignored), but cruft for a clean baseline:
```bash
sudo /usr/lib/parallels-tools/install --remove   # confirm the flag with --help
sudo dracut --regenerate-all --force --no-hostonly
```

### 1.4 Fix any literal device-node references (only if §1.0 found them)
limina exposes the disk as `/dev/vda`, so any `/dev/sdaX` in `/etc/fstab`,
`GRUB_CMDLINE_LINUX`, or the BLS entries won't resolve — replace with `UUID=` from `blkid`.
Stock Fedora is UUID-based and usually needs no edit. If you touched `/etc/default/grub`,
`sudo grub2-mkconfig -o /boot/grub2/grub.cfg`.

### 1.5 Full power-off (not suspend)
```bash
sudo systemctl poweroff
```
A suspended VM leaves the `.hds` non-quiescent and keeps memory-state files.

---

## 2. Convert + transfer (Mac A → Mac B)

```bash
# Merge snapshots into the base — qemu-img reads only the single top .hds and ignores
# DiskDescriptor.xml + the snapshot chain.
prl_disk_tool merge --hdd '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd'

# Convert the .hds DATA file (NOT the .hdd bundle / DiskDescriptor.xml) → raw.
qemu-img convert -f parallels \
  '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd/<Name>-0.hdd.0.{GUID}.hds' \
  -O raw Fedora-Workstation-44.migrated.raw

# Verify GPT + an EF00 ESP (Apple-Silicon Parallels Linux is UEFI/GPT). "MBR only" ⇒ legacy
# install that limina's EDK2 EFI path won't boot ⇒ fall back to fresh-install + rsync.
gdisk -l Fedora-Workstation-44.migrated.raw

# Transfer, preserving sparseness.
rsync -avz --sparse --progress Fedora-Workstation-44.migrated.raw <macB>:~/limina-disks/
```

---

## 3. Run limina.app on Mac B (basic tier)

The bundle is host-side self-contained: libkrun is statically linked in; the patched
virglrenderer + the KosmicKrisp/zink GL closure are vendored at `@rpath`; the GOP firmware is
in `Resources/`; and (as of the gvproxy fix) `gvproxy` is vendored at `Contents/MacOS/gvproxy`
so `--net` works without Homebrew. **Rebuild the app** (`scripts/build-app.sh`) on the dev Mac
to pick up the bundled gvproxy, then copy `target/limina.app` to Mac B.

```bash
# Transfer the .app with a tool that does NOT set quarantine (scp/rsync/USB/zip). If it ever
# gets AirDropped/downloaded instead, clear it once — sufficient for an ad-hoc-signed app:
xattr -dr com.apple.quarantine /path/to/limina.app

# De-risk first: prove the app launches on THIS Mac with a known-good disk, separating
# "app works here" from "my migrated disk works". (Grab the official Fedora aarch64 raw.xz.)
#   .../limina.app/Contents/MacOS/limina --window --disk Fedora-Workstation-44.raw

# First boot of the migrated disk — windowed (see GRUB/kernel), serial captured, no --net yet.
# NEVER boot the master raw; clone first (instant APFS CoW):
cp -c Fedora-Workstation-44.migrated.raw migrated-clone.raw
/path/to/limina.app/Contents/MacOS/limina \
  --window --disk migrated-clone.raw --console /tmp/migrated-serial.log
```

- Expect a software-rendered GNOME desktop on virtio-gpu (Wayland; venus is advertised then
  degrades to `kms_swrast`/llvmpipe on a stock guest — it does not hang).
- **If GPU init aborts** (the one cross-machine unknown — the KK/Metal stack was only ever
  exercised on the M1 Max / macOS 26.5 dev Mac): relaunch with `--gpu-software-2d` for a
  degraded-but-usable 2D desktop, then investigate KK separately.
- Add networking once it's known-good: `--net` (reads the SSH port from the log:
  `guest SSH forward ready: ssh -p N claude@127.0.0.1` — don't assume 2222).

### Boot triage
- **GRUB + kernel, then hangs / dracut emergency shell** → initramfs still lacks
  `virtio_mmio` (the #1 failure). Recover via the Fedora rescue entry or a live clone, chroot,
  re-run the §1.1 regen.
- **Black, no GRUB** → check `/tmp/migrated-serial.log` (ttyAMA0); silence means the §1.2
  `console=` args didn't take.
- **Reboot-loops on SELinux relabel** → inside the guest set `SELINUX=permissive` in
  `/etc/selinux/config` + `touch /.autorelabel`, boot once to relabel, confirm
  `/.autorelabel` is gone.
- Sanity: `otool -L .../limina.app/Contents/MacOS/limina-vmm | grep virgl` must show
  `third_party/virgl-prefix` (the silent software-2D degrade trap).

---

## 4. Enhanced tier (later)

Once the basic tier is a stable daily driver, the enhanced components (16k kernel, venus
mesa, patched mutter, limina-agent) layer in. Delivery is an **out-of-band, versioned
`limina-guest-tools` payload** (the RPM sets + `install-enhanced.sh` + the prebuilt
`limina-agent` + a manifest) pushed into the guest over **virtiofs `--share`** — which needs
no network and no gvproxy and works on a stock guest with no agent yet:

```bash
limina --window --disk <persistent-guest>.raw --share guest-tools=~/limina-guest-tools:ro
# in the guest, as root:
mkdir -p /media/guest-tools && mount -t virtiofs limina-guest-tools /media/guest-tools
sudo /media/guest-tools/install-enhanced.sh /media/guest-tools   # kernel+mesa+mutter+agent, then reboot
```

`install-enhanced.sh` now also installs `limina-agent` (+ unit + flat-pointer gschema
override) and runs `restorecon`, so the whole upgrade rides the one offline channel — stage
those three files into the payload alongside the RPMs.

> **F44 caveat:** enhanced mesa+kernel are fine for F44, but the patched **mutter** for F44
> (GNOME 50) hits the unresolved GNOME 49→50 cogl/scanout regression. Building the F44
> enhanced components in-guest is tracked separately (see the F44 enhanced build prep work).
> Until that's resolved, enhanced-tier dogfooding of venus on F44 is degraded; an F43 guest is
> the fallback for exercising the full enhanced experience.

---

## Weaknesses this surfaces

Tracked in `docs/hardening-backlog.md` → "Dogfooding / Parallels migration". Highlights:
1. No Parallels-import tooling — the `virtio_mmio` prep is undocumented and a footgun. *(this doc is step one)*
2. ✅ `gvproxy` now bundled (`--net` worked only with Homebrew before).
3. No guest-tools distribution path — enhanced tier can't be built/installed from the app alone (the F44 in-guest build prep addresses the build half).
4. ✅ Agent install folded into `install-enhanced.sh` (+ `restorecon`) — was a separate SSH flow.
5. No payload↔guest version manifest check → ABI-mismatch risk.
6. KK/Metal never tested cross-machine (`--gpu-software-2d` is the fallback).
7. F44 enhanced tier blocked (mutter 49→50).
