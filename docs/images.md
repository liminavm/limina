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

## The two tiers (see `CLAUDE.md`)

- **Basic / stock baseline** — an unmodified-shaped Fedora guest on its own kernel via the EFI path.
  Must boot and be usable, **degraded**: software-2D display (no 3D capset advertised → GNOME renders
  in llvmpipe), no venus, no dynamic memory, no USB. This is the floor the whole upgrade path stands on.
- **Enhanced** — our custom 16 KiB kernel + venus + guest components (mutter fix, `limina-agent`,
  clipboard bridge). Unlocks accelerated 3D, zero-copy scanout, clipboard, etc. **Additive** — layered
  onto a basic guest, never a precondition for it.

## Images

### Fedora 44 — the clean baseline (added 2026-06-20)

| Image | Role |
|---|---|
| `Fedora-Workstation-44.raw` | **Pristine** Fedora 44 Workstation aarch64, decompressed from the official download (`Fedora-Workstation-Disk-44-1.7.aarch64.raw.xz`). Fedora-built → SELinux labels intact, so it EFI-boots *enforcing* with **no relabel loop** (unlike the F43 dev images). **Clone source only — never boot directly.** |
| `Fedora-Workstation-44.raw.xz` | Compressed pristine source. Re-decompress (`xz -dk`) to reset `…44.raw` to factory. Re-downloadable, kept as the cheap reset point. |
| `Fedora-Workstation-44.boot.raw` | CoW clone used to **validate the software-2D floor** (the vanilla GNOME desktop renders in llvmpipe with zero guest edits — pixel-verified 2026-06-20). User then ran gnome-initial-setup and enabled **autologin for `claude`**. This is the documented **vanilla-baseline-with-user** image: stock F44 kernel (4 KiB), software-2D, *no* limina enhancements. |
| `f44-edk2-build.raw` | **EDK2 firmware build VM** (CoW clone of `…44.boot.raw`, added 2026-06-22). Has a warm `~/edk2` checkout (slp/edk2 @ `krun-support`, submodules initialized) and the build deps installed (`gcc gcc-c++ make python3 git nasm acpica-tools libuuid-devel`). **Prefer this over the Apple `container` for firmware work** — a normal Linux filesystem gives a clean edit→build→repeat loop, whereas the container loses cross-`run` source edits to its build reset and silently skips recompiles on cross-`run` mtime skew (which cost real time during the windowed-hang root-cause). Build helpers on the VM: `~/guest-build-edk2.sh` (repo source: `scripts/build-krun-efi-vm.sh`) + `~/edk2-patch.py` (the verbatim limina platform patches from `scripts/build-krun-efi.sh`). **GCC 16 note:** BaseTools isn't actually prebuilt, and F44's GCC 16 turns slp/edk2's bundled Pccts K&R C into errors (C23 makes `()` mean `(void)`); the build script installs a `~/ccwrap` wrapper that appends `-std=gnu17 -Wno-error` to host C compiles so the build tools compile (firmware binary unaffected). |

Boot the **EDK2 build VM** headless. A build box needs no 3D, so `--gpu-software-2d` is the lightest
path — it skips host-GL/virgl setup a compile-only VM has no use for (it is *not* a workaround for any
boot failure; a plain coexist boot is fine, just heavier). Then SSH in and build:
```bash
target/debug/limina --firmware /opt/homebrew/share/krunkit/KRUN_EFI.silent.fd \
  --disk f44-edk2-build.raw --gpu-software-2d --display-capture /tmp/buildvm-cap.png \
  --net --cpus 8 --ram-mib 12288 &
ssh -p 2222 … claude@127.0.0.1 './guest-build-edk2.sh'   # -> ~/KRUN_EFI.gop.debug.caller.fd
scp -P 2222 … claude@127.0.0.1:KRUN_EFI.*.fd target/krun-efi/   # pull the blob back
```

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
| `Fedora-Workstation-43.raw` | The F43 **dev image** — provision freely. *Not* truly pristine: modified across many `selinux=0` custom-kernel sessions. EFI-prepared (permissive relabel + `console=tty0 console=ttyAMA0`) via `scripts/prepare-efi-image.sh`. |
| `Fedora-Workstation-43.test.raw` | **Frozen CoW snapshot** the test harness boots by default, so dev changes don't perturb tests. Refresh with `cp -c Fedora-Workstation-43.raw Fedora-Workstation-43.test.raw`. |
| `Fedora-Workstation-43.dev-enh.raw` | The **enhanced-tier, seated-venus** dev image — the validated "actually usable accelerated desktop." Direct-booted with the custom 16 KiB kernel (`target/test-guest/kernel/Image-16k`); has baked in: gdm autologin, zink→venus session env, `/opt/mesa-zink`, the #32 mutter stencil-clip fix, `limina-agent` + session clipboard bridge, x11-enabled venus ICD, gfxreconstruct, kmscube. Bake/seal via `scripts/run-enhanced.sh` / the seated-kk flow; reinstall individual components via the `scripts/install-*.sh` helpers. **Irreplaceable** until provisioning is automated — keep safe. |
| `Fedora-Workstation-43.vanilla.raw` | **Pristine** stock F43 Workstation aarch64 (mesa 25.2.4, mutter 49.1, 4 KiB kernel) — clone source only, no user. Boots to gnome-initial-setup. |
| `Fedora-Workstation-43.accessible.raw` | **Pristine-but-accessible base** (added 2026-06-25): a `…vanilla.raw` clone with gnome-initial-setup done (user `claude`), the host pubkey in `authorized_keys`, **autologin**, and **NOPASSWD sudo** (`/etc/sudoers.d/91-claude-nopasswd`), saved via a clean PSCI poweroff. The go-to start point for **enhanced-tier provisioning tests**: `cp -c` it, boot, scp the RPM payload, run `scripts/provision/install-enhanced.sh`. Stays stock until you opt in — no `/opt/mesa-zink` cruft (unlike the dev images), so the enhanced tier validates without the gdm crash-loop confound. **End-to-end enhanced install + 16k+venus desktop pixel-verified on a clone of this 2026-06-25.** |

Boot the enhanced tier: `scripts/run-enhanced.sh [--window | --capture <png>]` (clones internally).

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

With `--net`, gvproxy forwards `127.0.0.1:2222 → guest:22` (the well-known MAC gives the guest the
static `.2` lease — see `docs/research/07-networking.md`). Wait for the SSH banner before logging in:
```bash
ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1
```
