# M10 — Multiple disks + ISO/CD-ROM

## STATUS: DESIGNED (2026-06-30) — not started

Design for attaching more than one disk to a limina guest (persistent data disks, a
rescue/target pairing, and read-only ISO media), plus the boot-ordering question that ISO
*installs* raise. Grounded in a full read of the disk stack and an adversarial critique pass;
citations are in the **Map citations** footer (re-verify before editing).

The headline finding: **everything below the CLI is already multi-disk capable.** The worker
models disks as `Vec<DiskSpec>` and loops over them; libkrun stores block devices in a
`VecDeque`, attaches each with its own MMIO window + IRQ + FDT node, and already supports
read-only (`VIRTIO_BLK_F_RO`) and qcow2 (imago). The *only* single-disk assumption lives in the
two CLIs (`crates/limina` and `crates/limina-vmm`), which hardcode `--disk: Option<PathBuf>` plus
a global `--read-only`. So Phase 1 is mostly a CLI + arg-plumbing change.

But two things that *look* free are not, and the critique surfaced them as load-bearing:
**(a)** *which disk boots* is a firmware (EDK2 BDS) decision that attach order does **not**
control; **(b)** a multi-disk VM interacts badly with M9 snapshots —
adding a disk renumbers the MMIO/IRQ of the *trailing* vsock+net devices, so the disk set must
be identity-stable for snapshot/restore, and nothing today enforces or even records that.

---

## 1. Goal & motivation

Parallels-parity for a daily driver:

- **N persistent data disks** at boot (separate `vdb`, `vdc`, … beside the boot disk),
  read-write, surviving reboot. *Near-term need.*
- **Create a fresh data disk** ("add a 50 GB disk") — today impossible: limina only *attaches*
  existing images (no creation logic anywhere in the product crates).
- **Rescue / migration pairing** — boot a rescue rootfs and attach a broken/target disk
  (`docs/roadmap.md:855-873`; ties into the migrated-guest root-mount work, task #5).
- **Read-only ISO media** — mount an installer/data `.iso` inside the guest.
- **Booting an installer ISO** — run an OS installer from an ISO onto a target disk.

Non-goals this milestone: runtime hot-add / media change (no virtio-mmio hotplug — §6.3); a
SCSI/ATAPI CD-ROM device model (unnecessary — §5.1); **online disk grow/resize** (deferred —
§11); device-node passthrough (`/dev/diskN`) as a disk backend (deferred — §9).

## 2. The decision (committed choices, up front)

1. **Multiple disks need no libkrun patch.** Make both CLIs accept a repeatable
   `--disk PATH[:ro][:create=SIZE]`, assign each a unique `block_id`; the existing worker loop
   and libkrun `VecDeque` do the rest.
2. **An ISO is a read-only virtio-blk disk** (`--cdrom PATH` ≡ `--disk PATH:ro`). No
   CD-ROM/ATAPI/SCSI model — none exists, none is needed; the guest mounts ISO9660 off a
   read-only `/dev/vdX`. (Mounting an ISO is Phase 1; *booting/installing from* it is §5.2.)
3. **Both shipping tiers boot via EFI/GRUB/BLS with `root=UUID=`, so attaching data disks/ISO
   cannot shift root — and root selection does NOT control which disk boots (that's decision 5).**
   - **Stock tier**: Fedora's own kernel, dracut initramfs, BLS entry → `root=UUID=`.
   - **Enhanced tier (PRODUCT)**: the `limina-kernel-16k` RPM installs via `kernel-install`, so
     **dracut builds an initramfs and writes a BLS entry with the distro-standard `root=UUID=`**
     — it boots firmware→GRUB→BLS exactly like stock (root-critical drivers are also `=y`, so the
     initramfs isn't *load-bearing* for the mount, but it is present and does the UUID resolution).
     This corrects an earlier draft that wrongly said the enhanced tier has no initramfs; that is
     only true of the **dev/test direct-kernel vehicle** below.
   - **Dev/test direct-kernel path (NOT the product)**: the L1 harness and the `run-*.sh` dev
     loops direct-boot a built-in-driver kernel with **no initramfs** and a positional
     `root=/dev/vda3` (`Boot::Kernel`/`Boot::KernelDisk`, `lib.rs:176-199,518-522`). A
     no-initramfs kernel's built-in resolver can't translate a *filesystem* `UUID=`, only
     `PARTUUID=`/`PARTLABEL=`/`/dev/<dev>`/`MAJ:MIN`. We **keep this path working** (it's load-
     bearing for L1/L2 tests and venus dev), but for multi-disk robustness it should move off the
     positional `root=/dev/vda3` to `root=PARTUUID=` (a test-harness hardening item — §4.2, §10
     Phase 0 — not a product constraint).
   - Guest `/dev/vdX` naming follows attach order in practice (decision below / §4.1), but for
     persistence the guest's own `fstab`/bootloader should key on UUID/LABEL — standard practice,
     and what both product tiers already do.
4. **Stable per-disk identity (`block_id` → guest serial) is a small, additive libkrun patch,**
   Phase 2 (needed once M9 snapshots / a persisted VM-config want to key on a stable handle). For
   Phase 1, `block_id` is **positional** (`disk0`=boot, `disk1`, …), collision-free by
   construction, **stable across reboot only**. Keep `disks[0].id == "root"` so any pre-M10
   single-disk snapshot stays restorable.
5. **Boot-device selection is the only genuinely hard part, and attach order does not solve it.**
   The firmware (stock EDK2 BDS) auto-enumerates *every* ESP it finds; limina supplies **no**
   `BootOrder` and **no** persisted NVRAM/varstore. So with **two or more bootable disks** (the
   rescue pairing, a second OS, a migrated disk that carries its own ESP, or an installer ISO)
   the guest **may boot the wrong disk's bootloader** — before any `root=` is consulted. Levers:
   - **(5a-i)** Attach only one bootable medium at a time (e.g. the ISO alone for an install;
     the target is a data disk the installer writes). Zero firmware work — **Phase 3**.
   - **(5a-ii)** Interactive firmware/GRUB menu pick — now viable because the GOP firmware has a
     working keyboard at the menu (the EFI ConIn / VirtioKeyboardDxe work just shipped) —
     **Phase 3**.
   - **(5b)** Host-controlled `BootOrder` via a baked EFI varstore — net-new EDK2 work, **deferred
     Phase 3b**; the only path to deterministic unattended multi-bootable-disk boots.
6. **Phase 1 is CLI-only, but multi-disk + M9 snapshots need a disk manifest that nobody owns
   yet** (§6.2). Until that lands, **named snapshot/restore of a multi-disk VM is gated/blocked**
   — taking it on Phase 1's transient argv risks a silent mis-restore.
7. **Image lifecycle:** Phase 1 adds `--disk PATH:create=SIZE` (create a sparse raw if absent,
   then attach) and documents the `truncate -s SIZE file.raw` host workaround; the guest
   partitions/`mkfs`es it. Grow/resize deferred (§11).
8. **Concurrent-attach safety:** take a host advisory **exclusive lock** (`flock`/`O_EXLOCK`) on
   each *writable* backing file at open, released on worker exit; `:ro` disks may share. Prevents
   the classic "same image read-write in two VMs → corruption" footgun.

## 3. Current state — what already works

| Layer | Multi-disk? | Evidence |
|---|---|---|
| libkrun resources | ✅ `BlockBuilder.list: VecDeque`, `push_back` per disk | `vmm/src/vmm_config/block.rs:46-61`, `resources.rs:355-358` |
| libkrun attach | ✅ `attach_block_devices` iterates the list, one MMIO+IRQ each | `builder.rs:2448-2463`, `device_manager/hvf/mmio.rs:106-134` |
| libkrun FDT | ✅ one `virtio_mmio@<addr>` node per device, sorted by addr | `devices/src/fdt/aarch64.rs:419-444` |
| Read-only | ✅ O_RDONLY + `VIRTIO_BLK_F_RO` advertised | `devices/src/virtio/block/device.rs:241-244,299-301` |
| qcow2 / vmdk | ✅ imago backend dispatches on `ImageType` (limina hardcodes Raw) | `block/device.rs:258-282`, `block/mod.rs:38-60` |
| Worker | ✅ `disks: Vec<DiskSpec>` + `for disk in &spec.disks { add_disk }` | `limina-vmm/src/config.rs:188-189`, `krun/mod.rs:132-134,327-346` |
| **Supervisor CLI** | ❌ `--disk: Option<PathBuf>` + global `--read-only` | `limina/src/main.rs:51-57,287-293` |
| **Worker CLI** | ❌ `--disk: Option<PathBuf>`, collapses to one `id:"root"` | `limina-vmm/src/main.rs:57-63,221-229` |
| **Image creation** | ❌ none — limina only attaches existing files | (only test-only `cp -c` at `limina-test/src/lib.rs:2040-2049`) |

The shared IRQ pool is **128 on aarch64** (`arch/src/aarch64/layout.rs:75,78`). The concern is
**not** exhausting it — it's that block devices sit *mid-stack* (§6.2), so each added disk
renumbers the IRQ/MMIO of the devices attached after it. (x86_64 has only 11 IRQs — noted for the
future port.)

## 4. Disk model, ordering & stable identity

### 4.1 `--disk` order controls device order — host-deterministic, guest reliable in practice
**The order of `--disk` options is the attach order, and limina controls it end-to-end.** First
`--disk` → first in the worker's `Vec<DiskSpec>` → `BlockBuilder` `push_back` (a `VecDeque`) →
`attach_block_devices` iterates in that order → `register_mmio_device` hands out a strictly
increasing MMIO base + IRQ per attach (`mmio.rs:106-134`) → the FDT explicitly **sorts
virtio-mmio nodes by address** (`fdt/aarch64.rs:438`), so node order == address order == attach
order. **The host-side layout is fully deterministic** (verified in source, high confidence):
the first `--disk` is the lowest-addressed virtio-blk node, the second the next, etc.

On the **guest** side, Linux assigns `/dev/vdX` by walking the FDT virtio-mmio nodes in order and
allocating a virtio-blk IDA index per probe (0→`vda`, 1→`vdb`, …). Under the kernel's **default
synchronous probe** this follows FDT/address order, so **first `--disk` = `vda`, second = `vdb`**
holds reliably in practice. It is a probe-*order convention*, not a hard kernel ABI (async probe,
if ever enabled, could race the IDA and swap names), so: limina **can and does rely on `--disk`
order** for device order and the expected `vdX` mapping; anything that must *persist* a binding
(guest `fstab`, the boot disk's `root=`) should still key on UUID/LABEL/PARTUUID — standard
practice. A two-disk boot test (§10 Phase 1) confirms the mapping empirically. (Note: disks don't
start at the lowest virtio address — balloon/rng/console/gpu/input/fs are attached first — so the
*relative* order among disks is what's guaranteed, not a fixed absolute address for `vda`.)

### 4.2 Root selection (see decision 3)
- **Both shipping tiers** (stock + enhanced PRODUCT) boot via GRUB/BLS with `root=UUID=` and a
  dracut initramfs, so attaching data disks/ISO **cannot** shift root.
- **The dev/test direct-kernel path** (no initramfs) uses positional `root=/dev/vda3` today; a
  no-initramfs kernel can't resolve a *filesystem* `UUID=`, so for multi-disk robustness it should
  move to `root=PARTUUID=` (kernel-native, no initramfs needed). This was the actual shape of the
  migrated-guest root-mount failure (task #5) — on the *dev direct-boot* path, not the product.
- **Test-harness hardening (Phase 0):** the test scaffolding hardcodes positional roots that a
  two-disk test would exercise — `crates/limina-test/src/lib.rs:518-522` (`root=/dev/vda3 …`),
  `scripts/run-venus-window.sh:82`, `scripts/run-enhanced.sh:44`, and the single-disk prep boot in
  `scripts/prepare-efi-image.sh:78`. Switch these to `root=PARTUUID=` so a two-disk test can't
  pass/fail nondeterministically on a probe-order race. (These are all dev/test direct-kernel
  cmdlines — the shipped tiers already use BLS `root=UUID=`.)

### 4.3 Stable guest identity needs a small libkrun patch (Phase 2)
`block_id` is used only as the host-side MMIO map key (`builder.rs:2456-2459`); it is **not**
exposed to the guest. The guest serial (`VIRTIO_BLK_T_GET_ID`) is derived from the host file's
`st_dev+st_rdev+st_ino` (`device.rs:109-136`) — opaque, and **not stable across an APFS `cp -c`
clone or a move** (inode/device change), which is exactly the M9.4 clone path. To give each disk
a stable handle (`/dev/disk/by-id/virtio-<block_id>`), patch `Block::new`/`build_disk_image_id` to
use `block_id` as the serial. Minimal, upstreamable, additive (without it the inode serial still
works; Fedora keys on filesystem UUID anyway, so stock boot is unaffected).

## 5. ISO/CD-ROM & boot-device selection (load-bearing)

### 5.1 ISO = read-only virtio-blk (mounting is easy)
libkrun has **no** CD-ROM/ATAPI/SCSI model (grep finds only an inert x86 EDD boot-param field).
This is not a gap: ISO9660 is a read-only filesystem, mountable off a read-only `/dev/vdX`. So
`--cdrom file.iso` ≡ `--disk file.iso:ro`. We lose only eject/media-change semantics — moot,
since there's no hotplug (the media set is fixed at boot anyway). RO is enforced by the O_RDONLY
host fd + the advertised `VIRTIO_BLK_F_RO`; a guest write fails at the host syscall →
`VIRTIO_BLK_S_IOERR` (there's no explicit write-guard in `block/worker.rs:231-240`, which is fine
— a compliant guest honors the feature bit). **Mounting an ISO read-only is Phase 1.**

### 5.2 Booting/installing FROM an ISO — the boot-order problem (Phase 3)
limina supplies **no** EFI `BootOrder` and **no** persisted NVRAM/varstore; boot-device choice is
stock EDK2 BDS auto-enumeration (`EfiBootManagerConnectAll`), re-run from scratch each boot
(`build-krun-efi.sh` only patches console/GOP/keyboard/TPL — no boot-order patch). **Attach order
does not map to boot priority**, so with two+ bootable disks the firmware may launch the wrong
ESP. To install from an ISO:

- The ISO must be EFI-bootable (hybrid/El-Torito with `EFI/BOOT/BOOTAA64.EFI`).
- **(5a-i, Phase 3)** Attach only the ISO as the bootable medium; the install target is a data
  disk. No firmware change. Confirm BDS reliably boots a sole EFI ISO before promising installs.
- **(5a-ii, Phase 3)** Interactive firmware/GRUB menu pick — relies on the shipped keyboard-at-GRUB.
- **(5b, deferred)** Bake an EFI varstore + write `BootOrder`/`BootXXXX` so the host can say "boot
  the ISO first, once." Net-new EDK2 work; the only deterministic unattended-install path.

## 6. Lifecycle: reboot, snapshot, hotplug (load-bearing)

### 6.1 Reboot = relaunch — disk *args* survive for free
The supervisor replays the same immutable arg vector on every relaunch (headless
`supervisor.rs:233-259`; windowed clones `base_args` `main.rs:653-660`). `--disk` is pushed into
the once-built `args` in `main()`, so any number of disks ride reboot with no extra work —
**provided the disk list is built once in `main()`, never inside the relaunch loop** (the latter
would risk reorder → host-layout drift). Verified: no current code path drops or reorders it.
**Caveat the doc must keep:** a reboot *re-opens* every backing file; if the host deleted/renamed
a disk while the VM ran, the next boot fails to attach it and the **whole VM** fails to boot
(exit ≠ 125 ⇒ no relaunch), not just that disk. Consider a degrade-don't-die policy for non-boot
data disks (skip a missing data disk with a warning; only the boot disk is fatal).

### 6.2 Snapshot (M9) — the disk set is a whole-VM identity, and nobody owns it yet
M9's host-side snapshot serializes RAM + device state; on restore the worker **reopens block
backing files from its replayed `--disk` argv** (`m9:270`), **not** from the snapshot blob. Three
hazards the design must carry:

1. **No automatic guard against a changed disk set.** M9's schema is fail-closed only on schema
   *version* (CBOR magic + semver); it deliberately uses "QEMU-subsection-style optionality since
   the two tiers present different device sets" (`m9:80`) — i.e. it is *permissive* about
   differing device sets. So a disk-set drift is **not rejected**; it can **silently mis-restore**
   (device state applied to the wrong transports). **M10 must enforce disk-set identity itself**
   (a snapshot-time disk manifest — paths, order, `:ro`, `block_id` — checked on restore), and we
   should add an explicit M9 requirement that a device-count/identity mismatch be *rejected*, not
   subsection-skipped.
2. **Adding/removing a disk renumbers the TRAILING devices, breaking vsock + net on restore.**
   libkrun's fixed attach order is balloon → rng → console → gpu → input → fs → **block** → vsock
   → net (`builder.rs:976,1039,1056,1060,1072,1074,1086`), and each device takes the next
   MMIO window + IRQ. So a disk-set change shifts the MMIO base + IRQ of **vsock and net**, whose
   serialized `MmioTransport` + in-kernel GIC config (`m9:79-80`) are keyed to those exact values.
   A restore with a different disk count therefore corrupts the **control-plane/agent (vsock)** —
   which M9's resume UX depends on for re-HELLO + timesync — and the **NIC (net)**, not merely the
   disks' own mounts. Disk-set stability is a **whole-VM correctness requirement**.
   *Forward-looking fix:* when M9 snapshots ship, attach block devices **last** (after vsock/net),
   or reserve IRQ/MMIO slots, so the disk set can grow without renumbering the long-lived devices.
3. **Manifest ownership is unassigned.** Phase 1 is CLI-only with no persisted per-VM config
   (decision 6), so the disk set exists only as transient argv; restore needs that exact argv
   reconstructed, and neither M10 (as scoped) nor M9 persists it. ⇒ Either pull a minimal
   per-snapshot disk manifest into M10, or **block named snapshot/restore for multi-disk VMs until
   the deferred serde `VmSpec` lands** (`limina-vmm/src/config.rs:1-6`). M10 + M9 named snapshots
   converge on needing one persisted VM-config home.

**ISO-under-snapshot trap:** since there's no hotplug, an ISO attached when a snapshot is taken is
a *permanent* device in that snapshot and pins the `.iso` path for every future restore — but
install media is ephemeral (users delete it). A keepsake snapshot should be taken **after**
detaching install media (reboot without `--cdrom`); or the persisted set should exclude read-only
ISO devices.

**Clone/CoW:** a snapshot is not self-contained w.r.t. disk *bytes*; cloning a VM without copying
(APFS `cp -c`) its data disks makes two VMs share — and corrupt — one backing file. This obligation
must be **filed into M9.4** (add "CoW/copy data-disk backing files on clone" to M9.4's scope + Done
test — `m9:286-290` currently lists clone only as a VMGenID/entropy concern), not just noted here.

### 6.3 Hotplug — not supported, by design
No add/remove of devices (including disks) at runtime: all devices are attached once in
`build_microvm` before vCPUs run; virtio-mmio here has no hotplug registers. "Insert a CD" /
"attach a disk to a running VM" = reboot with it declared. (Runtime *mutation* exists for
already-attached devices — the balloon target and GPU re-modeset handles, `builder.rs:966-967` —
but that does not add/remove devices.) Disk hotplug would need a new libkrun virtio-transport
capability (large, speculative) — out of scope.

## 7. Two-tier mapping

| | Stock baseline (must boot) | Enhanced (additive) |
|---|---|---|
| Multiple disks | ✅ upstream-shaped libkrun — N virtio-blk devices | same |
| Root stays put with extra disks | ✅ `root=UUID=` (dracut + BLS) | ✅ `root=UUID=` (dracut + BLS — RPM kernel boots like stock) |
| Read-only / ISO mount | ✅ RO virtio-blk | same |
| Stable `/dev/disk/by-id` serial | inode-derived (opaque, not clone-stable) | `block_id` serial via the libkrun patch (§4.3) — additive |
| Boot the *right* disk (2+ bootable) | ⚠️ BDS auto-enumerates; no host control until §5b | same |
| Auto-mount data disks | manual (`fstab`/Files) | optional `limina-agent` convenience later |

The `block_id`→serial patch is a libkrun *mechanism* patch (carried in `patches/libkrun/`); it
applies in both tiers and degrades gracefully, so it doesn't break "stock boots on upstream-shaped
libkrun." Multiple disks themselves need **no** patch.

## 8. What we patch vs. reuse

- **Reuse (no patch):** the whole block path — `VecDeque` resource list, per-device
  attach/MMIO/IRQ/FDT, RO mode, qcow2 backend, the worker's `disks` vec + `add_disk` loop, the
  reboot arg-replay spine.
- **limina-only (no libkrun change):** repeatable `--disk`/`--cdrom` parsing in both binaries;
  positional `block_id` assignment; per-disk `read_only`; `is_file`-or-block-device existence
  validation (distinguishing ENOENT/EACCES) + duplicate-PATH detection; `--disk PATH:create=SIZE`
  image creation; the writable-backing-file `flock`; threading the vec through `base_args`;
  per-tier root id (UUID/PARTUUID) in image prep + harness.
- **Small libkrun patch (Phase 2):** `block_id` → virtio-blk serial.
- **Deferred libkrun/firmware (Phase 3b):** EFI varstore + host `BootOrder`.
- **Cross-milestone:** disk manifest for snapshots + "attach block last" (M9); CoW data disks on
  clone (M9.4).

## 9. CLI / API surface

```
--disk PATH[:ro][:create=SIZE]   # repeatable; first --disk is the boot disk.
--cdrom PATH                     # sugar for --disk PATH:ro (read-only ISO); appended after data disks.
--read-only                      # back-compat: the SUPERVISOR folds it into disks[0]:ro at arg-emit
                                 #   time (so it applies to the FIRST disk only — documented in help);
                                 #   the worker then has a single source of truth (per-disk :ro).
```

- **Suffix grammar** (order-independent; reuses the codebase's exact-trailing-`:ro` convention,
  `main.rs:889-892`): a recognized suffix is stripped only when it matches exactly (`:ro`,
  `:create=SIZE`, later `:qcow2`); everything before is the path. Interior colons in a path are
  fine; a path literally ending in `:ro` is the un-escapable edge — documented, not solved (same
  limitation `--share` has). Multi-suffix order (`PATH:ro:create=10G` vs `PATH:create=10G:ro`) is
  accepted either way.
- **Validation** (new code; *not* `parse_share`'s — that checks `is_dir` for directories): each
  `--disk` path must exist and be a **regular file or a block device** (existence check distinguishes
  ENOENT from EACCES with a clear message), unless `:create=SIZE` is given (then create a sparse
  raw of SIZE if absent; error if present-but-wrong-size). Duplicate-PATH detection is a new
  path-keyed `HashSet` at the supervisor call site (`--share` dedups by *tag*, not path).
- **Worker contract:** the supervisor emits one `--disk PATH[:ro]` per entry, in declared order;
  the worker builds one `DiskSpec` per arg with a positional `block_id` (`disk0`=boot/`"root"`,
  `disk1`, …) — collision-free by construction.
- **Format** is **auto-detected** (Phase 4, shipped): the worker sniffs the 4-byte magic
  (`detect_image_type`, `krun/mod.rs`) and passes the right `ImageType` to libkrun — no `:qcow2`
  suffix needed. This supersedes the planned explicit suffix and removes the footgun of opening a
  qcow2 as raw. (`:create=SIZE` still makes a *raw*; qcow2 *creation* via `--disk` is out of scope.)
- **macOS reachability:** Phase-1 CLI inherits the launching terminal's TCC grants (the worker is
  ad-hoc signed with only the hypervisor entitlement, `hvf-entitlements.plist`), so terminal-run
  works. The eventual `.app` will need TCC handling (user-selected paths / security-scoped
  bookmarks / `files.user-selected.read-write`) for data disks under protected or external
  volumes — flagged for the app-bundle work, not Phase 1.
- Examples:
  - `limina --disk fedora.raw --disk data.raw` → boot disk + a data disk.
  - `limina --disk fedora.raw --disk new.raw:create=50G` → creates+attaches a blank 50 GB disk
    (guest then partitions/`mkfs`es it).
  - `limina --disk rescue.raw --disk broken.raw:ro` → rescue boots; target attached read-only.
    (⚠️ if `broken.raw` is bootable, BDS may pick *it* — see §5.2; attach it `:ro` and prefer the
    sole-bootable-medium lever.)
  - `limina --disk fedora.raw --cdrom Fedora.iso` → ISO mountable read-only inside the guest.
  - Install: `limina --cdrom installer.iso --disk target.raw:create=40G` + sole-ISO / menu-pick.

## 10. Phased rollout (RED-first, bisectable)

> **As-built (Phase 0 + Phase 1 landed 2026-06-30):** commits add repeatable `--disk` to both
> CLIs (`feat(m10): repeatable --disk …`) and the harness `data_disks` support + the L2 RED test.
> One scope adjustment from the plan below: the **PARTUUID cmdline switch was deferred**, not
> done — the RED test runs on the stock **Firmware/BLS** path (`root=UUID`, the shipping-tier
> path), so it never touches the dev direct-kernel `root=/dev/vda3`; and that dev path is
> single-disk in every current use, so its positional root can't race. The PARTUUID hardening is
> only worth doing if/when the *dev direct-kernel* path is itself run multi-disk, and it needs
> per-image PARTUUID resolution (a real chore — hardcoding a UUID would break boots). Tracked in
> §11.
>
> **As-built (Phase 2 partial, landed 2026-06-30):** `feat(m10): Phase 2 — stable disk identity …`.
> *Done:* the stable-identity libkrun patch (`patches/libkrun/0038`) — virtio-blk serial = the
> positional `block_id`, so the guest gets `/dev/disk/by-id/virtio-root` (vda) / `virtio-disk1`
> (vdb), clone/move-stable; and `--cdrom PATH` supervisor sugar (read-only `--disk`, appended after
> the data disks), with the disk/cdrom forwarding extracted into a unit-tested `build_disk_args`.
> `tests/disks.rs` asserts the by-id mapping + its reboot-stability (RED→GREEN proven by toggling
> 0038). *Deferred:* the disk-set **manifest for multi-disk snapshots** (decision 6 / §6.2) — it has
> no consumer until M9 (not started), so it's filed as an M9 cross-dependency rather than built on
> Phase-2 argv. The `--cdrom` runtime path (a guest read-only mount) is the same `:ro` virtio-blk
> Phase 1 already ships; full *boot/install from* an ISO is Phase 3.
>
> **As-built (Phase 4 landed 2026-06-30):** `feat(m10): Phase 4 — qcow2 data disks …`. libkrun
> already opens qcow2 via imago; the worker hardcoded `ImageType::Raw`. Now `detect_image_type`
> (`krun/mod.rs`) sniffs the magic and passes the right `ImageType` — **auto-detect, no `:qcow2`
> suffix** (a deviation from the plan, and an improvement: no silent-corruption footgun). L2 test
> `qcow2_data_disk_reads_writes_and_survives_reboot` uses `/sys/block/vdb/size` as the discriminator
> (a `qemu-img` qcow2 is 64 MiB virtual / ~200 KiB physical, so the guest sees 64 MiB only if opened
> as qcow2) — RED→GREEN proven; + an L0 magic-detection test. Backing chains open via imago. qcow2
> *creation* via `--disk` stays out of scope (`:create` makes a raw).
>
> **As-built (Phase 3a landed 2026-06-30):** `feat(m10): Phase 3a — boot from an installer ISO …`.
> The §11 "only real unknown" is **resolved — with zero code.** An EFI-bootable aarch64 ISO attached
> as the sole disk (`--cdrom`, so it is `vda` read-only) wins firmware BDS out of the box: our GOP
> EDK2 firmware already carries the El Torito + FAT driver stack (`PartitionDxe`/`EnhancedFatDxe`/
> `VirtioBlkDxe`), self-discovers the embedded ESP, and chainloads `\EFI\BOOT\BOOTAA64.EFI` → GRUB.
> Spiked with `Fedora-Server-netinst-aarch64-43-1.6.iso` on both the debug- and release-GOP firmware
> (`spikes/m10-iso-boot/`, RESULTS.md): the full chain is in the serial log — `PartitionDxe: El Torito
> standard found` → `Installed Fat filesystem` → `FSOpen '\EFI\BOOT\BOOTAA64.EFI' Success` →
> `BdsDxe: starting Boot0001 … CDROM(…)/\EFI\BOOT\BOOTAA64.EFI` → `GRUB version 2.12` — and the GOP
> scanout PNG shows the same Fedora installer menu (two independent channels). Left to auto-boot it
> even loaded the installer **kernel+initrd off the ISO** (ISO9660 mount + Anaconda media-check on
> `/dev/vda`). Regression guard: `tests/disks.rs::boots_efi_iso_to_bootloader` boots the ISO as the
> sole disk and asserts the firmware reaches GRUB on the console (serial `wait_for`; SKIPs when the
> gitignored ISO/GOP firmware is absent, like the other image-gated L2s). **Remaining M10:** only
> Phase 3b (deferred — host-managed `BootOrder` via a baked EFI varstore, for scripted/unattended
> installs and multi-bootable-disk determinism; a productization concern, not a boot blocker).

**Phase 0 — harness prerequisite (no user-visible feature).** The test harness was single-disk:
the `Boot` enum carried one `disk: PathBuf` and the arg builder emitted exactly one `--disk`
(`limina-test/src/lib.rs`). *Done:* a `data_disks: Vec<DataDisk>` field on `GuestConfig` (additive
to any boot mode, not the `Boot` enum) + `with_blank_data_disk`/`with_data_disk` builders that
emit extra `--disk` args after the boot disk (blank disks created sparse in scratch; writable
existing images cow-cloned first). *Deferred:* the `root=/dev/vda3`→`root=PARTUUID=` switch (see
the as-built note).

**Phase 1 — N read-write data disks + creation + RO mount (the daily-driver need).** *Done:*
- Both CLIs: repeatable `--disk PATH[:ro][:create=SIZE]`; positional `block_id` (`disks[0]`
  stays `"root"`); `--read-only` folded into `disks[0]:ro` by the supervisor; `is_file`-or-block
  validation + ENOENT/EACCES messages + dup-PATH detection; `:create=SIZE` sparse-raw creation;
  writable-file `flock`; the vec rides `base_args` (built once) for windowed + reboot replay.
- RED-first (on the **L2 stock Firmware/BLS** guest — `mkfs` needs a full guest, not the
  virtiofs-rooted L1; and BLS is where `root=UUID` lives): `crates/limina-test/tests/disks.rs`
  boots with a blank second disk, **confirms it enumerates as `vdb`** (size-matched, so the right
  disk landed there — validating §4.1's ordering claim empirically), is read-write, that root
  stays on `vda`, and that an mkfs+mount+write **survives a guest reboot**. Plus L0 unit tests for
  the `--disk` suffix parser, `:create` (sparse/idempotent/refuse-resize), the positional-id
  scheme, and a real `flock`-conflict test.

**Phase 2 — `--cdrom` sugar + stable identity.**
- `--cdrom` convenience (trivial). libkrun patch: `block_id` → virtio serial. Disk manifest for
  snapshots (decision 6 / §6.2) — or formally gate named snapshots for multi-disk VMs.
- RED-first: `/dev/disk/by-id/virtio-<block_id>` resolves to the right disk and is stable across
  a reboot; a snapshot of a 2-disk VM restores correctly (or is cleanly refused if the set
  changed). (RO/ISO mounting itself ships in Phase 1 — it's free given the device support.)

**Phase 3 — boot/install from an ISO.**
- 3a (**SHIPPED 2026-06-30, zero code**): the sole-ISO and interactive-menu levers (§5.2; relies on
  shipped keyboard-at-GRUB). RED-first done: a known EFI-bootable aarch64 ISO booted as the sole
  disk reaches the ISO's bootloader on both serial *and* GOP evidence (and on into the installer
  kernel). The GOP firmware's El Torito + FAT stack already does this; `--cdrom` (Phase 2) supplies
  the attach. Guard: `tests/disks.rs::boots_efi_iso_to_bootloader`; spike `spikes/m10-iso-boot/`.
- 3b (deferred): EFI varstore + host-written `BootOrder` for scripted installs / multi-bootable
  determinism.

**Phase 4 — formats + (out of scope) hotplug/resize.**
- Plumb `DiskSpec.format` → `ImageType::Qcow2`. RED-first: a qcow2 data disk reads/writes; backing
  chains open. Hotplug stays out of scope (§6.3); online grow/resize deferred (§11).

## 11. Open questions & risks

- **Boot-device selection (§5.2)** — ~~the only real unknown~~ **RESOLVED for the sole-ISO case
  (Phase 3a, 2026-06-30):** a sole EFI-bootable aarch64 installer ISO *does* win BDS — verified end
  to end (El Torito → ESP → BOOTAA64.EFI → GRUB → installer kernel), no code needed, guarded by
  `tests/disks.rs::boots_efi_iso_to_bootloader` (spike `spikes/m10-iso-boot/RESULTS.md`). Only
  multi-bootable determinism (two+ bootable disks, scripted/unattended installs) remains open — that
  needs **3b** (a baked EFI varstore + host-written `BootOrder`), still deferred.
- **Root selection (§4.2) — PARTUUID switch DEFERRED.** Both shipping tiers boot BLS `root=UUID=`
  (no concern). Only the **dev/test direct-kernel** path uses positional `root=/dev/vda3`
  (`lib.rs:518-522`, `run-venus-window.sh:82`, `run-enhanced.sh:44`, `prepare-efi-image.sh:78`),
  and it is single-disk in every current use, so its positional root can't race today; the L2 RED
  test runs on the Firmware/BLS path and never touches it. Moving these to `root=PARTUUID=` is only
  needed if the dev direct-kernel path is itself run multi-disk, and it requires resolving each
  image's real PARTUUID (hardcoding one would break boots) — so it's deferred until something
  actually needs it. (This positional fragility — on the *dev direct-boot* path — was the shape of
  the migrated-guest root-mount failure, task #5.)
- **Disk ordering (§4.1)** — host order is source-verified deterministic; guest `vdX` naming is a
  reliable-in-practice probe-order convention. Confirm `--disk #2 → vdb` empirically in the
  Phase-1 two-disk boot test (the one piece not provable from source alone).
- **Snapshot ↔ disk-set identity (§6.2)** — needs the manifest + "attach block last" + stable
  `block_id` (Phase 2) before M9 named snapshots ship for multi-disk VMs; M9.4 clone must CoW data
  disks. Track as M9 cross-dependencies (file them into m9).
- **Config home** — Phase 1 is CLI-only; M10 + M9 named snapshots both want the deferred serde
  `VmSpec`. Decide whether Phase 2 introduces it.
- **Online grow/resize** — deferred: grow the host backing file offline + guest-side resize;
  capacity is fixed at attach (`krun/mod.rs:334-343`). Document the workaround when implemented.
- **`--read-only` first-disk-only semantics** — surface clearly in help to kill the
  global-looking surprise.

## Map citations (point-in-time; re-verify before editing)
- Supervisor CLI/plumbing: `crates/limina/src/main.rs:51-57,224,287-293,294-310,448-452,523,653-660,889-892`,
  `crates/limina/src/supervisor.rs:104-111,233-259`; entitlements `crates/limina-vmm/hvf-entitlements.plist`, `sign.sh`.
- Worker CLI → libkrun: `crates/limina-vmm/src/main.rs:57-63,221-229`,
  `crates/limina-vmm/src/config.rs:1-6,10-19,166-172,188-189`, `crates/limina-vmm/src/krun/mod.rs:122-134,327-346`.
- libkrun block device + builder + attach order: `third_party/libkrun/src/vmm/src/vmm_config/block.rs:27-61`,
  `resources.rs:355-358`, `builder.rs:966-967,976,1039,1056,1060,1072,1074,1086,2448-2463`,
  `device_manager/hvf/mmio.rs:78-134`, `devices/src/virtio/block/device.rs:109-136,241-301`,
  `block/mod.rs:38-60`, `block/worker.rs:231-304`, `devices/src/fdt/aarch64.rs:307-444`,
  `arch/src/aarch64/layout.rs:75-78`.
- Lifecycle/reboot/snapshot: `patches/libkrun/0023-*.patch`, `crates/limina/src/supervisor.rs:29-33,57-75`,
  `docs/design/m9-suspend-resume.md:79-80,270-271,286-290`.
- ISO/boot-order/firmware: `scripts/build-krun-efi.sh:111-243`, `scripts/prepare-efi-image.sh:77-79,90-93`.
- Enhanced PRODUCT boots BLS `root=UUID=` (NOT no-initramfs): `scripts/provision/f44/build-kernel-rpm.sh:83,141-142,152-155`,
  `scripts/build-kernel-rpm.sh:5-12,36-37,158-162`, `scripts/provision/install-enhanced.sh:18-20,172,209-215,240-242,246-249,368-451`,
  `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh:46-49`, `docs/images.md:60`, `docs/roadmap.md:421-426,505-511`.
- Dev/test no-initramfs direct-kernel path (kept, but not the product): `crates/limina-test/src/lib.rs:176-199,420,464,484-540,1007-1036`,
  `scripts/build-test-kernel.sh:56,76-78`, `scripts/run-venus-window.sh:80-84`, `scripts/run-enhanced.sh:42-46`,
  `scripts/prepare-efi-image.sh:77-79`. `--initramfs` plumbing (unused in-repo): `crates/limina/src/main.rs:39-41,234-244`,
  `crates/limina-vmm/src/main.rs:38-44,211-219`, `crates/limina-vmm/src/krun/mod.rs:241-263`.
- Disk ordering (host-deterministic): `crates/limina-vmm/src/krun/mod.rs:132,334`, `resources.rs:356`,
  `vmm_config/block.rs:46,57`, `builder.rs:2455,2054`, `device_manager/hvf/mmio.rs:106,130`, `fdt/aarch64.rs:321,419,438`.
- Two-tier: `CLAUDE.md:49-72`. Roadmap M10: `docs/roadmap.md:855-873`.

## Cross-references
- `docs/design/m9-suspend-resume.md` — snapshot/restore + clone (disk-set identity, attach-block-last, CoW).
- `docs/roadmap.md` M10 — the milestone entry this design fills in.
- The EFI ConIn / VirtioKeyboardDxe work (keyboard at GRUB) — enables the interactive ISO boot-pick lever (§5.2).
- Task #5 (migrated-guest root-mount) — the live instance of the positional-`/dev/vda` fragility §4.2 fixes.
