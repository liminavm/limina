# Design — distribution, signing & updates  ·  PROPOSED

> **Status: PROPOSED. Signing and packaging are built; notarization, updates and
> guest-tools delivery are not.**
> Fills the largest planned-nowhere area (GAPS §3.4, roadmap M11 "not yet scoped"): how a
> user who is not us gets `limina.app`, trusts it, keeps it updated, and gets the enhanced
> guest tools into their VMs. Decisions here are *directions*; each ships with its own
> validation (a notarized build that boots a VM on a clean Mac is the "done" test).

## 1. What exists today

- `scripts/build-app.sh` assembles a self-contained `limina.app` (~245 MB): supervisor +
  worker + the whole host venus/GL closure vendored into `Contents/Frameworks` at `@rpath`,
  bundle-relative KK ICD. It signs **inside-out with hardened runtime and a secure
  timestamp** under an **Apple Development** identity from the keychain, pins the designated
  requirement to identifier+team, and **refuses to build ad-hoc** unless explicitly opted in
  (`LIMINA_ALLOW_ADHOC=1`) — ad-hoc TCC grants pin to the CDHash, so they break on every
  redeploy. It also always emits `target/Limina.dmg`.
- What is missing to make that shippable: a **Developer ID Application** identity in place of
  Apple Development, and notarization + stapling. Quarantine is still stripped by hand on the
  dogfood Mac.
- The worker carries `com.apple.security.hypervisor` (plus two hardened-runtime hatches, see
  §2.1(a)); tests codesign it per-build.
- Guest tools are an out-of-band tarball (`limina-guest-tools-<ver>.tar.zst`) built by
  `scripts/provision/f44/build-all.sh` and installed by `install-enhanced.sh` run manually
  in the guest ([[limina-enh-delivery]]).

Every gap below is between this and "a stranger downloads limina and it just works."

## 2. Channel decision: direct download (+ Homebrew cask) first

**Primary channel: a signed, notarized DMG from GitHub Releases; convenience channel: a
Homebrew cask** (`brew install --cask limina`) pointing at the same artifact. The DMG is
also what the in-app updater consumes. This is the channel v1 ships on, because it needs
nothing from Apple beyond a Developer ID certificate.

### 2.1 The Mac App Store is reachable, and worth keeping reachable

MAS is a **second** channel we have not built, not a foreclosed one. Two things are worth
stating plainly because both are easy to get wrong:

- **We have not "disabled" any sandbox.** App Sandbox is opt-in
  (`com.apple.security.app-sandbox`); limina has simply never adopted it.
  `crates/limina-vmm/hvf-entitlements.plist` carries `com.apple.security.hypervisor` plus two
  hardened-runtime escape hatches, and nothing sandbox-related. Adopting the sandbox is a
  redesign, not a switch — but it is a bounded one.
- **Our process model is not the blocker.** A sandboxed parent spawning a worker signed
  `app-sandbox` + `com.apple.security.inherit` that then calls `hv_vm_create` is a supported
  shape since macOS 11.3 (Apple fixed the `HV_DENIED` / sandbox-profile crash there;
  developer.apple.com/forums/thread/668496). Both `hypervisor` and `app-sandbox` are
  free-to-claim. UTM ships exactly this on the App Store.

What a MAS build would actually require, sorted by who has to say yes:

**(a) Plumbing — ours to do, no permission needed.**
- Sign supervisor `app-sandbox` + `hypervisor`; worker and `gvproxy` `app-sandbox` + `inherit`.
- Drop `com.apple.security.cs.disable-library-validation` and
  `com.apple.security.cs.allow-dyld-environment-variables`. MAS re-signs the whole bundle under
  one Team ID and `build-app.sh` already vendors the venus/GL closure at `@rpath` with a
  bundle-relative ICD, so neither *should* be needed — but this is the same unverified
  assumption §3 flags, and it has never been tested on a non-ad-hoc build.
- **Security-scoped bookmarks** for every disk image, `--cdrom` and virtiofs `--share` path,
  persisted in the VM definition; `.liminavm` bundles default into the app container.
- `com.apple.security.network.client` + `.server` (gvproxy user-mode NAT survives),
  `com.apple.security.device.audio-input` for the mic.
- **Kill the global `IOSurfaceLookup` fallback.** `limina-surfaceport` already hands scanouts
  over by Mach port, which is the sandbox-clean form; `present.rs` and `guestwindow.rs` still
  fall back to a global lookup for the venus zero-copy path, and that fallback has to go.
- Delete Sparkle and the `/usr/local/bin/limina` symlink — MAS does its own updates and forbids
  writing outside the container.

**(b) Design work — pointer and key capture without Accessibility.** The session-level
`CGEventTap` needs the Accessibility grant, which a sandboxed app cannot hold. This is the one
substantial piece of engineering, and it is worth doing **independently of the App Store** —
see `input-and-windows.md` §5.

**(c) Apple's approval — the real gate, and the reason to think about this early.**
- **USB passthrough** needs `com.apple.vm.device-access`: Apple-managed, no self-service even
  with a paid account, requested through an Apple rep with no SLA (`m7-usb-passthrough.md`).
  On MAS it replaces the root helper entirely; off MAS the helper is the only path.
- **Bridged networking** needs `com.apple.vm.networking`, restricted the same way.
- The `SMAppService` root helper (`privileged-helper.md`) is not permitted on MAS at all. So
  USB and bridged networking are *either* MAS-with-approval *or* Developer-ID-with-a-root-helper,
  and the two designs do not share an implementation. **Decide the channel before building
  either**, or build the mechanism behind an interface that can take both brokers.

The licensing side is already settled: the GPL-2.0-only "limina exception" carries an App Store
distribution grant (shipped 2026-09-02, `LICENSE` / `CONTRIBUTING.md`).

## 3. Signing & notarization

- **Developer ID Application** certificate; sign **inside-out** (never `--deep`):
  1. every dylib in `Contents/Frameworks` (the venus/GL closure),
  2. `limina-vmm` with **hardened runtime + `com.apple.security.hypervisor`**
     (unrestricted entitlement — any Developer ID may claim it; it must be in the
     *worker's* signature, the supervisor needs none),
  3. the future `limina-privhelperd` (its own identifier; `SMAppService` requires the
     daemon plist name ↔ signing identifier match, see `privileged-helper.md`),
  4. the app bundle.
- **Hardened runtime everywhere.** Expected friction to verify on a real signed build:
  - the worker loads our bundled dylibs only from `@rpath` inside the bundle — fine under
    hardened runtime (same Team ID); no `com.apple.security.cs.disable-library-validation`
    unless something loads a foreign plugin (nothing should);
  - `DYLD_*`/`VK_ICD_FILENAMES` env the supervisor sets for the worker: dyld *strips*
    DYLD_ variables for hardened binaries — the bundle already avoids needing them
    (rpath + bundle-relative ICD, `venus_env.rs`), but this is the #1 thing to re-verify
    signed, since the dev bundle was only ever ad-hoc.
- **Notarize** (`notarytool submit --wait`) and **staple** both the app and the DMG.
- **CI:** a signing job on a self-hosted Apple-Silicon runner (the same one the L2 boot
  tests need — one machine, two reasons to exist). Secrets: Developer ID cert + notary
  API key. Ad-hoc signing stays the no-secrets dev/PR default.

## 4. Updates

**Sparkle 2** (the de-facto standard for Developer-ID macOS apps): appcast on GitHub
Releases, EdDSA-signed archives, staged rollouts if ever needed. Alternatives considered:
- roll-our-own check+download — reinvents delta updates, signature checks, skip-versions;
- Homebrew-only — fine for CLI users, wrong for the app audience.

Wrinkles to design around when it's built:
- limina runs *long-lived VMs*; the updater must never relaunch under a running guest —
  "install on quit" is the only acceptable mode, and the M9 suspend work eventually turns
  "quit for update" into suspend/resume.
- The CLI (`/usr/local/bin/limina` symlink into the bundle) versions with the app.

## 5. Guest-tools delivery from the app

The enhanced payload (kernel + mesa + mutter RPMs + agents + installer) is **not bundled**
in the app (it is Fedora-release-specific and ~app-sized itself). Design:

- Each release publishes `limina-guest-tools-<ver>-<distro>.tar.zst` next to the DMG, with
  a **manifest** (payload version, target `/etc/os-release` IDs, component versions,
  sha256 per file) — the manifest is the missing version-check the backlog tracks.
- **`limina install-guest-tools [<vm>]`**: downloads (or takes `--payload PATH`), verifies
  checksum + manifest↔guest `/etc/os-release` match, stages it into an ephemeral read-only
  `--share`, and runs the installer in the guest — via the agent when present, else by
  printing the exact one-line `sudo` invocation for the user (the bootstrap floor: the
  stock tier must be able to receive tools with nothing pre-installed, per the two-tier
  tenet).
- The installed `tools_version` is recorded in the VM definition
  (`vm-definitions.md` §3 `[guest]`) and re-checked on connect; mismatch = a prompt to
  update tools, never a refusal to boot.

## 6. Crash reporting & telemetry

Opt-in only, and not v1: macOS `.ips` crash logs from the worker are the actionable
artifact; a "reveal crash logs" menu item costs nothing and respects the posture. No
network telemetry.

## 7. Build order (each step independently shippable)

1. **Developer-ID sign + notarize the existing bundle** and boot a VM on a clean second
   Mac with quarantine intact — flushes out every hardened-runtime/dyld assumption while
   the surface is smallest. (Do this before more Frameworks content accretes.)
2. DMG packaging + stapling; publish on a GitHub Release; Homebrew cask.
3. Guest-tools manifest + `limina install-guest-tools` (closes two backlog items).
4. Sparkle appcast wiring.
5. CI signing job on the self-hosted runner (with the L2 lane).

## 8. Open questions

- Whether the GL closure can shrink (the zink→LLVM host-GL stack dominates the 245 MB;
  llvmpipe-less builds or a pared LLVM are possible but low-priority).
- Payload hosting for guest-tools when releases become frequent (GitHub bandwidth is fine
  to start).
- A `limina import` (Parallels migration, `dogfooding-parallels-migration.md`) shipping in
  the same release as the first public DMG — the highest-leverage acquisition feature.
