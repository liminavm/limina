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
- **Enhanced** — our custom 16 KiB kernel + venus + guest components (`limina-agent`,
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
| **F43 enhanced** (`enhanced`, `enhanced.test`) | `limina-kernel-16k-6.12.0` *(co-installed beside stock `6.17.1`)* | 16 KiB | `26.2.0-3.limina.fc43` *(respun 2026-07-21: -3 adds mesa 0016 ring-loss DEVICE_LOST + 0017 venus submit free-list fix [via the 0016-pre upstream free-list-scan backport — the 26.2.0 base predates it]; -2 [2026-07-19] added mesa 0014)* | `49.6-1.limina.fc43` | `49.1` *(stock, unbumped)* |
| **F44 stock** (`*.raw`, `*.boot.raw`) | `6.19.10-300.fc44` | 4 KiB | `26.0.3-4.fc44` | `50.0-1.fc44` | `50.0` |
| **F44 enhanced** (`enhanced`, `enhanced.test`) | `limina-kernel-16k-7.1.6-2` *(respun 2026-08-04: the blob-scanout fence DROPPED — 86% of frames under async scanout, see below; respun 2026-08-03: **first fork-model kernel** — built from `liminavm/linux` branch `limina` at the rev pinned in `third_party/manifest.toml`, no patch series; co-installed beside `7.1.4`/`7.1.2-limina16k` [fallbacks] + stock `6.19.10-300`)* | 16 KiB | `26.1.5-6.limina.fc44` *(respun 2026-08-04: base caught up to stock 26.1.5; **mesa 0010 DELETED** — the host advertises `VK_EXT_image_drm_format_modifier` for real now [KK LINEAR-only + vkr verbatim passthrough], guest rides upstream venus's own passthrough with truthful host pitches; **0015 slimmed** — its `vn_wsi_create_image` DRM_MOD→OPTIMAL rewrite SIGSEGVed WSI once 0010's fabricated query stopped masking it [firefox, `spikes/modifier-necessity/RESULTS.md`]; prior: -3 2026-07-21 added 0017)* | `50.1-1.limina.fc44` | `50.0` *(stock)* |
| **F44 dogfood deployment** *(the user's Dev VM + upgraded dev clones — deployed via guest-tools; r3 install pass 2026-07-23, agent refresh 2026-08-01)* | `limina-kernel-16k-7.1.5` *(7.0.13/7.1.2/7.1.4 kept as fallbacks; NO stock `kernel-core` installed)* | 16 KiB | `26.1.4-3.limina.fc44` *(current: -2 adds 0016 ring-loss hardening, -3 adds 0017 submit-freelist fix; versionlock re-armed; running session picks the new mesa up at next login)* | **stock** `50.3-3.fc44` *(since 2026-07-11: distro update displaced the patched build; clipboard rides the clipboard@limina extension at `/usr/share/gnome-shell/extensions` — bridge backend confirmed up 2026-08-01)* | `50.3` *(stock)* |

Notes: enhanced **mesa + kernel** are pinned to *our* version and `dnf versionlock`ed; enhanced
**mutter** is rebuilt from the target distro's mutter SRPM carrying our patches over the stock GNOME
Shell of that release (same `libmutter-NN` ABI) — F43 `49.6` over shell `49.1`, F44 `50.1` over shell
`50.0`. **F44 enhanced is VALIDATED working (2026-06-29):** the old GNOME 49→50 mutter/cogl "scanout
regression" / `kk_encoder.c:299` block did NOT reproduce on the clean stack — F44 16k+venus+mutter-50.1
boots the seated desktop and runs WebGL at ~60fps on venus→KK→Metal (see `limina-enh-delivery` memory).
The enhanced 16 KiB kernel ships as RPM `limina-kernel-16k` (BLS entry beside stock).
**Current shippable tarball (2026-08-04, re-tarred same day for the GL flip):**
`target/guest-tools-7.1.6-mesa2615r6/limina-guest-tools-f44.tar.zst` (kernel 7.1.6-2 +
mesa 26.1.5-6.limina; both F44 enhanced images took its `install-enhanced.sh` pass).
**GL default flipped 2026-08-04 (drop-guest-zink):** the installer's
`/etc/environment.d/90-limina-zink.conf` (name kept) now selects
`GALLIUM_DRIVER=virgl` + `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` — the session's GL rides
**vrend** (EGLImage-backed IOSurface scanout, zero-copy since virgl `d042ed65`); **venus
stays as the Vulkan side** (`VK_DRIVER_FILES` → virtio ICD). zink-as-guest-GL is no longer
a supported configuration. Both F44 enhanced images re-ran the installer pass and were
smoke-tested (session-on-vrend + vkcube-on-venus + firefox, human-verified; stock tier
re-smoked the same day).
NOTE the host prerequisite: the 0010-less guest needs a host with KK ≥ `b778250986b`
(modifier ext + graceful query) and virglrenderer ≥ `0cc513fd` (verbatim passthrough) —
the revs pinned in `third_party/manifest.toml`. **Update the host app before (or with) the
guest mesa**: on an older host the guest sees no modifier ext, and without 0010's fabricated
table the WSI takes the prime-blit fallback — NOT validated, and plausibly the very
zero-copy breakage 0010(b) existed to avoid. Deploy host-first.

**Mutter left the delivery (2026-07-11).** The guest support package no longer ships a patched
mutter: the GNOME clipboard tier is now the `clipboard@limina` gnome-shell extension
(`guest/gnome-shell-extension/`; agent tier order ext-data-control → extension bridge →
RemoteDesktop). Trigger: a stock F44 update replaced dogfood-guest's `50.1-1.limina` with stock
`50.3-2.fc44` (rpm release `.limina` loses to any stock bump; mutter was deliberately unlocked),
demoting clipboard to the screen-share-indicator tier — and the same event validated that stock
mutter needs none of our rendering patches (0001/0002 retired; root causes were host-side/our own
build, see `patches/mutter/README.md`). Images that still contain `50.1-1.limina` mutter are fine:
it gets displaced by the guest's next distro update, and the tier ladder absorbs either state.
From the next payload/image refresh, guest mutter is plain stock. **Refresh done
2026-07-11:** the shippable tarball is now `target/guest-tools-7.1.2-ext/limina-guest-tools-f44.tar.zst`
(mesa-only repo, clipboard@limina extension, 3-tier limina-agent-session, no mutter), and BOTH
`enhanced.raw` and `enhanced.test.raw` took an `install-enhanced.sh` pass from it (kernel/mesa
idempotent no-ops; extension + new helper + mesa-only repo landed; 7.1.2 trial boot re-promoted;
clean poweroff). The images still carry the old patched mutter, so their helper sits on the
ext-data-control tier until a distro update displaces it — then the extension tier takes over.
GOTCHA found while validating: the accessible-derived images have gsettings
`org.gnome.shell disable-user-extensions=true` (origin unknown, not our provisioning), which blocks
ALL user extensions — on such guests the helper stamps its one-time enable, parks ~20 s, then rides
the RemoteDesktop tier. Consider `gsettings set org.gnome.shell disable-user-extensions false` in
`make-accessible.sh` at the next image respin.

**F44 kernel 7.1.6 respin — the first FORK-MODEL kernel (2026-08-03)** — `limina-kernel-16k-7.1.4-1`
→ **`7.1.6-1.fc44`** (KREL `7.1.6-limina16k`). Source is no longer "a stable tag + `patches/linux/`":
it is **`liminavm/linux`** branch **`limina`** (a fork of `gregkh/linux` — the stable mirror, since
`torvalds/linux` has no point-release tags), base `v7.1.6`, at the rev pinned in
`third_party/manifest.toml`. `build-kernel-rpm.sh` fetches that exact rev and has **no patch stage
at all**. Kernel delta (4 commits): the `mm/page_reporting` freezable-workqueue backport (upstream
`0b45f69` — verified NOT yet in v7.1.6, so still carried, replacing our old driver-side 0005); the
blob-scanout `RESOURCE_FLUSH` fence, **rewritten** against the `vgplane_st` prepare_fb refactor
(the old patch had silently stopped applying at the 7.1.x bump — this is the first shipped kernel
that actually has it); widened primary-plane formats (ARGB/XBGR/ABGR, 0002+0006 folded); the
LINEAR modifier advertisement. The 16 KiB host-visible alignment patch **left the kernel** (no-op
at 16k pages) and now lives in `guest/virtio-gpu-dkms/` for the stock-4k tier only. Payload =
`target/guest-tools-7.1.6/limina-guest-tools-f44.tar.zst` (mesa/agent/extension unchanged from
`mesa2614r3`, hardlinked; kernel-source reference regenerated around the fork pin). Both
`enhanced.raw` and `enhanced.test.raw` took an `install-enhanced.sh` pass — one-shot trial boot,
16384-byte pages, auto-promoted to default, 7.1.4/7.1.2 kept as fallbacks. **Runtime-verified on
the new kernel** (the check that would have caught the silent skip): the primary plane now
advertises `XRGB8888/ARGB8888/XBGR8888/ABGR8888` and carries an `IN_FORMATS` blob with
`DRM_FORMAT_MOD_LINEAR` — neither exists without our commits. Pre-respin snapshots kept as
`*.pre716.bak`. NOT on dogfood-guest (user's call).

**F44 kernel 7.1.6-**2** — the blob-scanout fence DROPPED (2026-08-04)** — `limina-kernel-16k-7.1.6-1`
→ **`7.1.6-2.fc44`**, KREL unchanged (`7.1.6-limina16k`), fork branch now **3 commits**
(rev `74ae69ad`). The fence the `-1` respin above shipped for the first time turned out to cost
**86% of frames** under async scanout — it *blocks* in `commit_tail` on a fence our host does not
signal until the CA latch. Three-arm isolation (arm C = the same tree minus that one commit,
back at 1.2%): `docs/perf/gsrs-local-rig.md`, verdict in `docs/upstreaming/ledger/linux.md`. The
two format commits are unchanged from `-1` and stay runtime-verified by the check above. Payload =
`target/guest-tools-7.1.6-nofence/limina-guest-tools` (kernel RPM swapped, everything else
hardlinked). `enhanced.raw` took an `install-enhanced.sh` pass (venus enumerates —
`Virtio-GPU Venus (Apple M1 Max)`, trial boot auto-promoted, clean poweroff);
`enhanced.test.raw` recloned from it. NOT on dogfood-guest.

**KNOWN DRIFT (2026-08-04): the shipped images are one commit-set AHEAD of the branch pin.** The
two DRM format/modifier commits were **dropped from `liminavm/linux` `limina`** the same day (the
branch is now a single commit, rev `07db31df`; tag `limina/2026-08-04-modifiers` recovers them —
rationale in `docs/upstreaming/ledger/linux.md`). The installed `limina-kernel-16k-7.1.6-2` still
*contains* them, and no respin was done: nothing is broken, the images keep working exactly as
measured, but **the next kernel build from the pin will not have them**. Two consequences to carry
into that respin:

- Bump `RELEASE` to **3** — `7.1.6-2` is taken, and same-NEVRA-different-content is the trap below.
- **A guest running our Vulkan compositor needs the one-line `MOD_INVALID` → LINEAR fallback first**
  (`spikes/modifier-necessity/niri-mod-invalid-linear-fallback.patch`), or it will fail to render
  every frame on the new kernel. Stock mutter guests are unaffected — they never name a scanout
  modifier.

> **TRAP found here — a Release bump does NOT let two builds of the same KREL coexist.** RPM
> `Release` is what makes rpm/dnf *see* a content change at the same version, so the bump is
> necessary; but `limina-kernel-16k` is `installonlypkg(kernel)`, so dnf tries to install `-2`
> **beside** `-1` — and both own `/lib/modules/7.1.6-limina16k/…`, giving hundreds of lines of
> `conflicts with file from package`. The old package must come **off** first, and it is the
> running kernel: boot the previous fallback (`grubby --set-default /boot/vmlinuz-7.1.4-limina16k`,
> reboot), `rpm -e limina-kernel-16k-7.1.6-1.fc44`, *then* run the installer. Bump `LOCALVERSION`
> instead only for throwaway probe kernels (e.g. `-limina16knf`), never for a shipped one — KREL is
> the guest-visible identity.

**`VN_PERF=no_fence_feedback` RETIRED from the guest env — ALL FOUR enhanced images (2026-07-25)**
— the flag was a MoltenVK-era workaround for the 16 KiB `hv_vm_map` blob-coherency bug
(`spikes/venus-render-server` Finding 6); that root cause was fixed 2026-07-03 (libkrun 0043 +
virglrenderer 0023 + `patches/linux/0004`) and the flag simply outlived it. Keeping it forced every
fence check onto a synchronous driver↔renderer round trip: `gnome-shell-rs` measured **25–30% of
wall clock** on submits carrying real work (eight blur submits, 13.6–15.3 ms with feedback vs
17.7–22.1 ms without), while a batched one-submit control was identical either way — so it is the
wait, not the work. Source-side removal = `1542acc`/`937eb7c` in `install-enhanced.sh`.

**Delivered to all four enhanced images** (`Fedora-Workstation-4{3,4}.enhanced{,.test}.raw`): each
`enhanced.raw` was booted in place, took the two current installer config deltas (rewrite
`/etc/environment.d/90-limina-zink.conf` without `VN_PERF`; remove the retired
`90-limina-pointer.gschema.override` + recompile schemas), rebooted, and was verified seated on the
16k kernel with `libvulkan_virtio` mapped into `gnome-shell` and the new env in its `/proc/PID/environ`
— then a clean poweroff; each `.test.raw` was recloned (`cp -c`) from it. Backups kept as
`*.enhanced.raw.pre-vnperf.bak`. No RPM work: kernel/mesa NEVRAs were already current
(F44 `7.1.4-limina16k` + mesa `26.1.4-3`, F43 `6.12.0-limina16k+` + mesa `26.2.0-3`), so this was a
config-delta pass rather than a full installer run. Tarball respun as
**`target/guest-tools-7.1.4-vnperf/limina-guest-tools-f44.tar.zst`** — same RPMs as `mesa2614r3`,
with the current `install-enhanced.sh` swapped in (the packaged one was three installer changes
behind: the VN_PERF removal, the retired flat-pointer override, and the stale-overlay mixed-mesa
guard). Hardlinks between `repo/` and the top-level RPMs restored before packing, so it stays the
same size as the payload it derives from. **NOT yet on dogfood-guest** — the deployed guest still
carries the flag.

*Settles half of the mesa-bump question:* the F43 image's **mesa `26.2.0-3.limina` still ships
`no_fence_feedback`** (`strings /usr/lib64/libvulkan_virtio.so | grep '^no_'`, in-guest), so
upstream's `4c1938c8adb` "venus: deprecate fence feedback" (2026-07-12) is not in 26.2.0. The
"read the commit before bumping" trigger therefore moves from *past 26.1.x* to **past 26.2.x**;
what the commit removed — the flag or the mechanism — is still unestablished.

**F43 guest-mesa 26.2.0-3 ring-loss + submit free-list respin (2026-07-21)** — F43 guest mesa
`26.2.0-2.limina` → **`26.2.0-3.limina.fc43`**, catching the F43 family up to the F44 26.1.4-3
patch level: **mesa 0016** (venus ring loss → `VK_ERROR_DEVICE_LOST` instead of `abort()`) +
**mesa 0017** (venus ring-submit free-list capacity fix — the quadratic CPU creep in
long-running venus apps). The F43 base (26.2.0-devel `3515c52`) predates upstream's
free-list-scan commit `2cf1f6cb508` that 0016 anchors on and 0017 fixes, so the build now
applies it first as **`patches/mesa/0016-pre-venus-ring-get-submit-freelist-scan-backport.diff`**
(verbatim upstream; also fixes the unbound free-submits malloc leak on this base). All three
wired **fail-loud** into `scripts/build-mesa-rpm.sh` (container build, `LIMINA_REL=3`); venus
verified in the RPM (`rpm -qlp` → `libvulkan_virtio.so` + `virtio_icd.aarch64.json`, 0016
marker strings present). Delivery: the six installed subpackages scp'd + dnf-upgraded in-guest
(versionlock delete-by-exact-name → upgrade → re-lock at `-3`), installed
`libvulkan_virtio.so` sha256-matched the RPM payload, reboot came back seated on
`6.12.0-limina16k+` with venus enumerating (`Virtio-GPU Venus`), clean poweroff;
`enhanced.test.raw` recloned.

**Dogfood guest-tools refresh — agent binaries only (2026-08-01, user-requested)** — dogfood-guest's
guest components were audited against HEAD and only the two binaries were stale (installed
2026-07-23): `limina-agent` `0.2.0` → **`0.3.0`** (FIDO, below) and `limina-agent-session` (a
`limina-proto` rebuild — no behaviour change). Everything else was already current and byte-identical
to the tree: kernel `7.1.5-limina16k` (running), mesa `26.1.4-3.limina`, the `clipboard@limina`
extension, both systemd units, and `/etc/environment.d/90-limina-zink.conf` (no `VN_PERF`); the
retired pointer gschema override was already gone. So this was a **per-file install**, not a payload
respin — the 2026-07-11 precedent, and the honest one when no RPM changed. Old binaries kept beside
the new ones as `*.bak-20260723` for rollback. Verified after restart: agent logs
`limina-agent 0.3.0` + `virtual FIDO device up` (so the deployed host bundle already advertises the
`fido` cap), helper logs `extension-bridge backend up` + `connected to host`, and both `/proc/PID/exe`
hashes match the shipped build. A **second pass the same day** shipped the `--version`/`--help`
handling both binaries had been missing (the probe trap above), verified on the guest before
installing: the musl unit-test binaries run there green (3+3) and the real binaries answer
`--version` and exit 2 on an unknown argument. **Not restarted:** the `gsrs` and gdm-greeter session helpers still
run the old binary until their next login — functionally identical, so restarting another user's
session was not worth the disruption. The host-side per-peer clipboard serial fix (`583030a`) is
**not** deployed here; the user deploys the app bundle separately.

**limina-agent 0.3.0 — Touch ID FIDO authenticator (2026-07-24)** — the agent advertises the
**`fido`** cap and, when the host has a Secure Enclave, creates a `/dev/uhid` FIDO2 HID device
and bridges CTAP over the vsock control plane (M14; the guest gets WebAuthn passkeys + Touch-ID
`pam_u2f` login, backed by enclave-bound keys). Feature doc + recipes: `docs/fido-authenticator.md`.
**Both F44 enhanced images carry 0.3.0 (refreshed 2026-07-24):** `enhanced.raw` took an
agent-only pass (booted in place, 0.3.0 binary installed to `/usr/local/bin/limina-agent` +
`restorecon` + `virtual FIDO device up` verified + clean poweroff); `enhanced.test.raw` recloned
from it (`cp -c`). A fresh boot of either now presents the FIDO device. The **app-bundle** side
ships too (`liblimina_sep.dylib` in Frameworks, per-VM store). Kernel/mesa/extension unchanged, so
no full payload respin was needed; the guest-tools tarball still rebuilds 0.3.0 from source via
`build-all.sh` when next repackaged.

**USB + fingerprint are default-on since `f9646d0` (2026-08-02):** every boot presents an emulated
xHCI controller with the elanmoc fingerprint gadget attached (no `--usb` flag, no guest components
needed) — a guest `lsusb` shows the reader on any image. User doc: `docs/fingerprint-reader.md`.

**limina-agent 0.2.0 — guest-clock TimeSync (2026-07-20, same-day follow-up #2)** — the agent
now advertises the **`timesync`** cap and steps the guest `CLOCK_REALTIME` to the host's
wallclock when it drifts ≥1 s (TimeSync over the control plane; supervisor sends on agent
connect, on detected host sleep, and every `LIMINA_TIMESYNC_SECS` [60] as insurance) — cures
the host-sleep clock lag (the dogfood-guest 6 h drift), the post-restore gap, and the CNTVCT
2119 wrap (backward steps allowed). Ships with **libkrun 0088** (PL031 RTC reads anchored to
the host wallclock instead of a sleep-frozen Instant). Payload =
`target/guest-tools-7.1.4-agent02/limina-guest-tools-f44.tar.zst` (host-side reassembly of the
mesa2614r2 payload: agent binary swap + manifest note; kernel/mesa/extension unchanged). Both
`enhanced.raw` (installer pass) and `enhanced.test.raw` (reclone) carry it. L1 gate =
`l1_agent_steps_a_skewed_guest_clock` (init skews the clock −7200 s via `limina.skew_clock`;
the agent steps it back within ~2 s of boot). NOT yet on dogfood-guest.

**Guest-mesa 26.1.4-2 ring-loss hardening (2026-07-20, same-day follow-up)** — F44 guest mesa
`26.1.4-1.limina` → **`26.1.4-2.limina.fc44`**, adding
**`patches/mesa/0016-venus-ring-loss-device-lost-not-abort.diff`**: a dead venus ring (host-side
`VK_RING_STATUS_FATAL_BIT_MESA`, e.g. a snapshot-restore replay gap) now surfaces as
`VK_ERROR_DEVICE_LOST` from the calling entrypoints instead of `abort()`ing the whole client
process — the guest-side companion to host virglrenderer 0040 (the vkmark-on-resume SIGABRT,
`spikes/m9-vkmark-resume-crash/RESULTS.md`; watchdog/renderer-hang aborts deliberately unchanged).
Built with the same F44 SRPM recipe in the `f44-kbuild` build guest (`LIMINA_REL=2`; all patches
`%prep`-clean; venus ICD + the 0016 marker strings verified in the RPM). Payload =
`target/guest-tools-7.1.4-mesa2614r2/limina-guest-tools-f44.tar.zst` (kernel/agent/extension/
installer unchanged). Both `enhanced.raw` (installer pass: mesa upgraded + re-versionlocked,
16k trial re-promoted, venus live in the seated session, clean poweroff) and `enhanced.test.raw`
(recloned) carry it. Graceful-death validated: with a pre-0040 host virgl, the L2 vkpipeline
client exits with `PIPE FAIL` instead of a SIGABRT coredump. NOT yet on dogfood-guest.

**Guest-mesa 26.1.4 base catch-up + clipboard RD opt-in (2026-07-20)** — F44 guest mesa respun
`26.1.3-4.limina` → **`26.1.4-1.limina.fc44`**, rebasing the base onto the CURRENT F44 stock SRPM
(`26.1.4-1.fc44`, `dnf download --source` — no koji pin needed anymore). The 26.1.4 stable branch
carries an upstream equivalent of venus patch 0009's `vn_wsi_clone_present_info` rectangle
deep-copy, so 0009 is superseded there by **`patches/mesa/0015-venus-wsi-present-fix-post-rect-clone.diff`**
(0009 minus those three hunks; see `patches/mesa/README.md` §"0009 vs 0015" — 0009 stays for
26.2.0/F43, never apply both). Patch set: 0001 + 0015 + 0010–0014, all `%prep`-clean; venus
verified in the RPM (`rpm -qlp … libvulkan_virtio.so` + `virtio_icd.aarch64.json` — the build
script's own venus WARN false-negatived AGAIN, trust only `rpm -qlp`). The same payload updates
**limina-agent-session**: the RemoteDesktop clipboard fallback is now **opt-in**
(`LIMINA_CLIPBOARD_RD=1`, default off — it lights GNOME's screen-share indicator; the
clipboard@limina extension bridge is the stock-GNOME tier). The `-1` payload tarball was
superseded the same day by the `-2` ring-loss-hardening respin above (which is the one kept on
disk). `build-mesa-rpm.sh` gained `PREP_ONLY=1` (rpmbuild -bp fast patch-apply iteration).

**F44 kernel 7.1.4 respin (2026-07-20)** — `limina-kernel-16k-7.1.2-1` → **`7.1.4-1.fc44`** (latest
stable), carrying **`patches/linux/0005`** (virtio_balloon: stop page-reporting across suspend — the
s2idle UAF the host masks with libkrun 0059; the guest-side prerequisite for re-enabling
`--balloon-free-page-reporting` across suspend/resume). Built in the F44 build guest
(`f44-kbuild.raw`, stable.git `v7.1.4` + the validated 7.0.13 Fedora config base);
`build-kernel-rpm.sh` now applies 0005 **fail-loud** (not upstream — a silent skip would ship the
UAF back). 0001 skipped as already upstream in 7.1.4; 0002–0004 applied. *(**Correction,
2026-08-03:** "0001 skipped as already upstream" was WRONG — it stopped applying because of an
upstream refactor, and its delta was never upstream, so every kernel from 7.1.2 through 7.1.5
shipped blob scanout unfenced. Fixed by the rewrite in the 7.1.6 respin above.)* Payload =
`target/guest-tools-7.1.4/limina-guest-tools-f44.tar.zst` (mesa/agent/extension unchanged from
2026-07-19; kernel-source reference regenerated). `enhanced.raw` took the installer pass (kernel
co-installed beside 7.1.2, one-shot trial boot → 16384-byte pages, auto-promoted to default, venus
enumerates in the seated session, clean poweroff); `enhanced.test.raw` recloned from it. The F43
family keeps its 6.12.0 kernel (F43 is mesa-current only). NOT yet on dogfood-guest — the tarball is
ready when the user wants to deploy.

**Guest-mesa 0014 refresh (2026-07-19)** — both image families' guest mesa respun to pick up
`patches/mesa/0014-zink-fix-unflushed-batch-wait-lost-wakeup.diff` (the zink multi-context
unflushed-batch-wait lost-wakeup deadlock that wedged `venus_replay` for ~100 min on 2026-07-12;
see the `limina-zink-lost-wakeup` memory + `spikes/venus-replay-zink-hang-2026-07-12/`):

- **F44**: mesa `26.1.3-3.limina` → **`26.1.3-4.limina.fc44`**, built with the f44 SRPM recipe in a
  `fedora:44` Apple container from the **koji `mesa-26.1.3-1.fc44` SRPM** (new `MESA_SRPM_URL` pin in
  `build-mesa-rpm.sh`): F44's repos have moved to mesa **26.1.4**, where venus patch `0009` no longer
  applies (3/5 hunks fail in `vn_wsi.c`) — rebase 0009/0010 before any 26.1.4-based build. Patch set:
  0001 + 0009–0014. Tarball = `target/guest-tools-7.1.2-mesa4/limina-guest-tools-f44.tar.zst`
  (kernel/agent/extension unchanged); `enhanced.raw` took a full `install-enhanced.sh` pass
  (kernel idempotent, mesa upgraded + re-versionlocked, 7.1.2 trial re-promoted, venus enumerates,
  clean poweroff), `enhanced.test.raw` recloned from it.
- **F43**: mesa `26.2.0-1.limina` → **`26.2.0-2.limina.fc43`** (container build
  `scripts/build-mesa-rpm.sh`, which now applies 0014 fail-loud + takes `LIMINA_REL`); the six
  installed subpackages were dnf-upgraded in-guest (versionlock delete-by-exact-name → upgrade →
  re-lock at `-2`), clean poweroff, `enhanced.test.raw` recloned.
- ~~NOT deployed: `patches/linux/0005`~~ *(deployed 2026-07-20 via the 7.1.4 kernel respin, see
  below)*; the host zink-on-KK build (`/Volumes/mesa-cs`) still lacks 0014 (apply at the next host
  mesa refresh).

**Enhanced images RESPUN to dogfood parity (2026-07-04)** — `enhanced.raw` + `enhanced.test.raw` were
brought from kernel `6.19.10-limina16k` + mesa `26.1.3-1` up to **kernel `7.1.2-limina16k` + mesa
`26.1.3-3`** (mutter already `50.1-1`). Method: reassembled a `-3` guest-tools payload from the
prebuilt RPMs (mesa `-2`→`-3` swap + a freshly `createrepo_c`'d `repo/` with the full subpackage set —
the `-2` tarball predated the local-repo feature), saved as
`target/guest-tools-7.1.2-mesa3/limina-guest-tools-f44.tar.zst`; then per image: `cp -c` clone →
boot → prune `/boot` (remove the old `6.19.10-limina16k`; stock `6.19.10-300` kept as fallback) →
`install-enhanced.sh` → validate (mesa `-3`, kernel `7.1.2` initramfs-verified, venus enumerates,
on-demand `mesa-libgbm-devel` resolves) → clean poweroff → swap over the golden (originals kept as
`*.enhanced.raw.pre-mesa3.bak` / `*.enhanced.test.raw.pre-mesa3.bak`). Not EFI-boot-tested in-image
(the deploy runs on the injected 6.12 kernel; `7.1.2` is the identical dogfood-proven RPM and
`install-enhanced` verified its initramfs mounts root).

**Latest F44 enhanced guest-tools DELIVERY (2026-06-30)** — was newer than the then-baked images
(now folded into the 2026-07-04 respin above): kernel
`limina-kernel-16k-7.1.2` (built from kernel.org `stable.git@v7.1.2` + a Fedora 7.0.x config + `olddefconfig`;
the 16k kernel is distro-independent so "latest stable" doesn't depend on Fedora packaging it), mesa
`26.1.3-2.limina` (Release bumped from `-1` so dnf upgrades cleanly; carries venus WSI patch `0011` = drop the
16-bit-unorm wayland swapchain format so a "first non-sRGB" wgpu client lands on a usable surface), mutter
`50.1-1.limina`. VALIDATED booting on a fresh F44 clone (16k pages, live venus, seated GNOME); shipped to
dogfood-guest as `~/limina-guest-tools-7.1.2-f44.tar.zst` (built by `scripts/provision/f44/` → `package-payload.sh`,
installed by `scripts/provision/install-enhanced.sh`). See the `limina-enh-delivery` + `limina-devmac-kernel-build`
memories.

**Repack 2026-07-01 (`target/guest-tools-7.1.2-clipboard/limina-guest-tools-f44.tar.zst`)** — same RPMs, plus
`limina-agent-session` (static musl) + its systemd **user** unit and the updated `install-enhanced.sh` that
installs/enables it `--global`: the clipboard bridge is session state, so the system-level `limina-agent` never
advertised the `clipboard` cap and host↔guest copy/paste was silently inert on the dogfooding VM. VALIDATED
end-to-end 2026-07-01 on a fresh `enhanced.test` clone: installer clean (kernel 7.1.2 co-install + mesa `-2`
upgrade + helper enabled), trial-boot to 7.1.2, helper auto-starts at login on the ext-data-control backend,
and a two-way pbcopy/wl-paste + wl-copy/pbpaste round trip passed.

**Payloads now carry the FULL mesa/mutter subpackage set via a local dnf repo (2026-07-03)** — the
delivered payloads above shipped only the *runtime* mesa subpackages, and on an enhanced guest the
mesa versionlock excludes stock, so Fedora's exact-NEVRA `-devel` subpackages could never resolve
(`dnf install mesa-libgbm-devel` → "filtered out by exclude filtering", hit on dogfood-guest
2026-07-03). Fix in the pipeline: `build-all.sh` stages **every** subpackage the builds produce
(don't hand-prune!), `package-payload.sh` builds `payload/repo/` (createrepo_c, devel/tests in,
debuginfo out), and `install-enhanced.sh` (step 3b) installs it as
`/usr/share/limina-guest-tools/repo` + `/etc/yum.repos.d/limina-guest-tools.repo` — so
`dnf install mesa-libgbm-devel` resolves against our NEVRA on demand. Upgrades carry any
installed `-devel/-tests` forward in the same transaction (else `--allowerasing` would erase
them). Takes effect from the NEXT payload build; existing guests can install matching devel RPMs
by file path (dogfood-guest has the `-3` set at `~/mesa-26.1.3-3.limina/`).

## Images

### Fedora 44 — mirrored image set (in progress, started 2026-06-29)

Mirrors the F43 five-role layout (see the release selector in Conventions); select with
`LIMINA_FEDORA_REL=44`. Built natively in-guest from Fedora's own F44 SRPMs + a minimal limina
delta (`scripts/provision/f44/`, `scripts/provision/make-accessible.sh`).

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
