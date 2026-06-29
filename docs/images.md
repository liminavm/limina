# Guest images

The source of truth for the Fedora guest disk images limina develops and tests against:
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
  `enhanced.test` (frozen enhanced-tier L2 snapshot). **`LIMINA_FEDORA_REL=43|44` (default `43`)**
  picks the release for the run scripts (`run-fedora-window.sh`, `run-enhanced.sh`,
  `run-venus-window.sh`) and the L2 harness (`crates/limina-test`); per-image overrides
  (`LIMINA_TEST_DISK`, `LIMINA_TEST_DISK_ENH`, `LIMINA_TEST_DISK_BASELINE`) still win. This is the
  template for future releases (F45): produce the five roles, flip the selector.

## The two tiers (see `CLAUDE.md`)

- **Basic / stock baseline** — an unmodified-shaped Fedora guest on its own kernel via the EFI path.
  Must boot and be usable, **degraded**: software-2D display (no 3D capset advertised → GNOME renders
  in llvmpipe), no venus, no dynamic memory, no USB. This is the floor the whole upgrade path stands on.
- **Enhanced** — our custom 16 KiB kernel + venus + guest components (mutter fix, `limina-agent`,
  clipboard bridge). Unlocks accelerated 3D, zero-copy scanout, clipboard, etc. **Additive** — layered
  onto a basic guest, never a precondition for it.

## Component versions (canonical — link here, don't restate)

**The single source of truth for guest component versions.** Memory files and other docs should
*link to this table* rather than restate numbers — a stale "mesa 25.3.6" once propagated into three
memories before anyone noticed. Verified 2026-06-27 by reading each image's rpmdb directly
(loop-mount the btrfs root offline → `btrfs restore -r 256` the `root` subvol → `rpm --dbpath … -q`).

| Tier / images | Kernel | Page | Mesa | Mutter | GNOME Shell |
|---|---|---|---|---|---|
| **F43 stock** (`vanilla`, `accessible`, `stock.test`) | `6.17.1-300.fc43` | 4 KiB | `25.2.4-2.fc43` | `49.1-1.fc43` | `49.1` |
| **F43 enhanced** (`enhanced`, `enhanced.test`) | `limina-kernel-16k-6.12.0` *(co-installed beside stock `6.17.1`)* | 16 KiB | `26.2.0-1.limina.fc43` | `49.6-1.limina.fc43` | `49.1` *(stock, unbumped)* |
| **F44 stock** (`*.raw`, `*.boot.raw`) | `6.19.10-300.fc44` | 4 KiB | `26.0.3-4.fc44` | `50.0-1.fc44` | `50.0` |

Notes: enhanced **mesa + kernel** are pinned to *our* version and `dnf versionlock`ed; enhanced
**mutter** is rebuilt from a newer upstream 49.x point release (`49.6`) carrying our patches and runs
over the **stock GNOME Shell `49.1`** (same `libmutter-49` ABI). F44 is currently *blocked* as an
enhanced target by the GNOME 49→50 mutter/cogl scanout regression (see the `limina-enh-delivery`
memory). The enhanced 16 KiB kernel ships as RPM `limina-kernel-16k` (BLS entry beside stock).

## Images

### Fedora 44 — mirrored image set (in progress, started 2026-06-29)

Mirrors the F43 five-role layout (see the release selector in Conventions); select with
`LIMINA_FEDORA_REL=44`. Built natively in-guest from Fedora's own F44 SRPMs + a minimal limina
delta (`scripts/provision/f44/`, `scripts/provision/make-accessible.sh`).

| Image | Role | Status |
|---|---|---|
| `Fedora-Workstation-44.vanilla.raw` (+ `.xz`) | **Pristine** F44 Workstation aarch64 (official `…44-1.7.aarch64.raw.xz`; Fedora-built → SELinux labels intact, EFI-boots *enforcing* with no relabel loop). Clone source only. | ✅ renamed from `…44.raw` |
| `Fedora-Workstation-44.accessible.raw` | **Stock base**: vanilla + gnome-initial-setup (`claude`) + pubkey + autologin + NOPASSWD sudo + `vulkan-tools` + no-idle-lock gschema + console args + relabel-clear (`make-accessible.sh`). Promoted from the existing `44.boot.raw` (already had user/ssh/autologin/sudo). | ✅ built 2026-06-29 |
| `Fedora-Workstation-44.stock.test.raw` | **Stock-tier L2 image** — frozen CoW snapshot of `accessible` (`DEFAULT` for `LIMINA_FEDORA_REL=44`; also the seated baseline-3D vehicle). | ✅ built; `efi_boots_to_userspace` GREEN 2026-06-29 |
| `Fedora-Workstation-44.enhanced.raw` | **Enhanced base** — `accessible` + `scripts/provision/f44/build-all.sh` → `install-enhanced.sh` (F44 SRPMs + minimal delta: 16k kernel, venus mesa 26.0.x, patched mutter 50.x). Building it is the experiment that grounds the mutter-50/KK question. | pending (patch rebases + desktop validation) |
| `Fedora-Workstation-44.enhanced.test.raw` | **Enhanced-tier L2 image** — frozen CoW snapshot of `enhanced`. | pending |

`Fedora-Workstation-44.boot.raw` is the **pre-accessible** image (stock F44 + `claude`/autologin,
software-2D floor pixel-verified 2026-06-20); running `make-accessible.sh` on it produces
`accessible.raw`. `f44-edk2-build.raw` — **RETIRED 2026-06-25** (`images-staging-delete/`, expires
2026-07-02): the EDK2 firmware build moved to the unified `limina-build` container image (below).

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
