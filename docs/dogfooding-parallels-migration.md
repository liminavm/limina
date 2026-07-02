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
| Fedora 44 now vs. switch to F43 | **F44, basic tier first** | Stock F44 is a verified limina baseline (`docs/images.md`). The basic tier is the bootstrap substrate, so we start there and layer enhanced on top. F44 enhanced (16k kernel + venus mesa + patched mutter 50.1) is now **validated end-to-end** (2026-06-29, `docs/images.md`) — the feared GNOME 49→50 mutter/cogl regression did **not** reproduce — so the enhanced upgrade (§4) is available, not a blocker. |
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
transport at all** (`third_party/libkrun/src/devices/src/fdt/aarch64.rs:322`,
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
Stock Fedora is UUID-based and usually needs no edit. On Fedora/BLS the *running* kernel
cmdline lives in `/boot/loader/entries/*.conf`, so fix per-entry kernel args with
`sudo grubby --update-kernel=ALL --args=…` / `--remove-args=…` (or edit the BLS `.conf`
directly); `grub2-mkconfig -o /boot/grub2/grub.cfg` alone may not propagate a
`GRUB_CMDLINE_LINUX` change to the actual boot entries. (`/etc/fstab` is unaffected by this.)

### 1.5 Full power-off (not suspend)
```bash
sudo systemctl poweroff
```
A suspended VM leaves the `.hds` non-quiescent and keeps memory-state files.

---

## 2. Convert + transfer (Mac A → Mac B)

> **Prereqs on Mac A:** `brew install qemu gptfdisk` — `qemu-img` (qemu) and `gdisk` (gptfdisk)
> are **not** part of base macOS. `prl_disk_tool` ships with Parallels Desktop. Also keep disk
> headroom: the converted raw is sparse but its *logical* size equals the full virtual disk
> (`ls -l` shows the scary full size; `du -h` shows real allocation), and it coexists with the
> still-present `.hdd` bundle until you clean up.

```bash
# Merge the snapshot chain into the base FIRST. Merge is destructive (it deletes all snapshots)
# and needs the VM fully powered off (§1.5). Merging is what guarantees a SINGLE data .hds —
# qemu-img reads exactly the .hds you name and ignores DiskDescriptor.xml + the chain (it has no
# concept of the 'top' of a chain, so a stale delta .hds left behind would convert silently).
prl_disk_tool merge --hdd '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd'

# Discover the post-merge data .hds (the filename pattern is Parallels-version-dependent), then
# sanity-check it BEFORE the full convert:
ls -lS '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd/'*.hds       # convert the single/largest
qemu-img info -f parallels '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd/<the>.hds'

# Convert the .hds DATA file (NOT the .hdd bundle / DiskDescriptor.xml) → raw.
qemu-img convert -f parallels \
  '/Users/<you>/Parallels/<Name>.pvm/<Name>-0.hdd/<the>.hds' \
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
to pick up the bundled gvproxy, then copy `target/Limina.app` to Mac B.

```bash
# Transfer the .app with a tool that does NOT set quarantine (scp/rsync/USB/zip). If it ever
# gets AirDropped/downloaded instead, clear it once — sufficient for an ad-hoc-signed app:
xattr -dr com.apple.quarantine /path/to/limina.app
# Ad-hoc-signed: if macOS still refuses the first launch, right-click the app → Open once (or
# approve it in System Settings → Privacy & Security) to whitelist it.

# De-risk first: prove the app launches on THIS Mac with a known-good disk, separating
# "app works here" from "my migrated disk works". Grab the official Fedora aarch64 raw.xz and
# decompress it first — a .raw.xz can't be booted as-is:
#   xz -d Fedora-Workstation-44.raw.xz
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
  `guest SSH forward ready: ssh -p N <user>@127.0.0.1` — substitute your guest's username for
  `<user>`, and don't assume 2222).
- macOS permissions on Mac B: pointer capture / mouselook (`Cmd-Ctrl-G`) needs **Accessibility**
  permission (System Settings → Privacy & Security → Accessibility); until granted, the first
  mouselook attempt silently does nothing.

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
  `@rpath/libvirglrenderer.1.dylib`, and `Contents/Frameworks/libvirglrenderer.1.dylib` must
  exist (the silent software-2D degrade trap). The `third_party/virgl-prefix` absolute path is
  the *dev* worker (`target/debug/limina-vmm`) form — never present in a bundle.

---

## 4. Enhanced tier (later)

Once the basic tier is a stable daily driver, the enhanced components (16k kernel, venus
mesa, patched mutter, limina-agent) layer in. Delivery is an **out-of-band, versioned
`limina-guest-tools` payload** (the RPM sets + `install-enhanced.sh` + the prebuilt
`limina-agent` + a manifest) pushed into the guest over **virtiofs `--share`** — which needs
no network and no gvproxy and works on a stock guest with no agent yet.

**Build the payload once** in a **separate, disposable F44 guest** — *not* the daily driver, since
the build leaves multi-GB of RPMs, sources and (debug-symbol) artifacts behind. Get the limina repo
into that guest (git clone over `--net`, or `--share` it from the host), then run
`scripts/provision/f44/build-all.sh` — it self-installs its build deps (`dnf builddep` + `cargo`)
and assembles `~/limina-guest-tools`: the kernel/mesa/mutter RPMs + the prebuilt `limina-agent` + its
unit + the flat-pointer gschema override + `install-enhanced.sh` + a manifest. Building in an F44
guest from F44's **own** SRPMs is what keeps the mesa/mutter RPM versions matched to your
daily-driver guest (no soname mismatch). Then ship `~/limina-guest-tools` to the daily-driver guest
over `--share`:

```bash
limina --window --disk <persistent-guest>.raw --share guest-tools=$HOME/limina-guest-tools:ro
# in the guest, as root:
mkdir -p /media/guest-tools && mount -t virtiofs limina-guest-tools /media/guest-tools
sudo /media/guest-tools/install-enhanced.sh /media/guest-tools   # kernel+mesa+mutter+agent, then reboot
```

`install-enhanced.sh` now also installs `limina-agent` (+ unit + flat-pointer gschema
override) and runs `restorecon`, so the whole upgrade rides the one offline channel — stage
those three files into the payload alongside the RPMs.

> **F44 enhanced tier — VALIDATED (2026-06-29).** The full stack works on F44: the 16k kernel,
> venus mesa (`26.1.3-1.limina`, F44 SRPM + venus patches), and patched mutter (`50.1-1.limina`,
> `patches/mutter/0001`+`0002`+`0003`). venus renders the seated GNOME desktop at ~60fps
> (venus→KK→Metal) and the venus L2 suite is **GREEN 7/7**; the feared GNOME 49→50 cogl/scanout
> "regression" / `kk_encoder.c:299` assert did **not** reproduce on the clean stack — no F43
> fallback needed.
>
> **Known limitation:** GLX/Xwayland apps present **black** on venus (the X11 kopper present
> path — rendering works, presentation doesn't); Wayland-native GL (Firefox WebGL, etc.) is fine.

---

## Weaknesses this surfaces

Tracked in `docs/hardening-backlog.md` → "Dogfooding / Parallels migration". Highlights:
1. No Parallels-import tooling — the `virtio_mmio` prep is undocumented and a footgun. *(this doc is step one)*
2. ✅ `gvproxy` now bundled (`--net` worked only with Homebrew before).
3. No guest-tools distribution path — `scripts/provision/f44/` now builds AND validates the full enhanced payload in-guest end-to-end (2026-06-29); the remaining gap is that `limina.app` still has no built-in channel to deliver or build it, so you assemble the payload out-of-band.
4. ✅ Agent install folded into `install-enhanced.sh` (+ `restorecon`) — was a separate SSH flow.
5. No payload↔guest version manifest check → ABI-mismatch risk.
6. KK/Metal never tested cross-machine (`--gpu-software-2d` is the fallback).
7. ✅ F44 enhanced tier validated end-to-end (2026-06-29) — was thought blocked by a GNOME 49→50 mutter/cogl regression; it did **not** reproduce on the clean stack (16k + venus + patched mutter 50.1, L2 7/7). Open limitation: GLX/Xwayland apps present black on venus (Wayland-native GL works).
