# Design — distribution, signing & updates  ·  IN PROGRESS

> **Status: IN PROGRESS (2026-09-01).** The self-contained app/DMG builder and the rolling,
> notarized development-release workflow exist. A stable release, clean-Mac acceptance run,
> Homebrew cask, and updater have not shipped.
> Fills the largest planned-nowhere area (GAPS §3.4, roadmap M11 "not yet scoped"): how a
> user who is not us gets `limina.app`, trusts it, keeps it updated, and gets the enhanced
> guest tools into their VMs. Decisions here are *directions*; each ships with its own
> validation (a notarized build that boots a VM on a clean Mac is the "done" test).

## 1. What exists today

- `scripts/build-app.sh` assembles a self-contained `Limina.app` plus `Limina.dmg`: supervisor +
  worker + the whole host venus/GL closure vendored into `Contents/Frameworks` at `@rpath`,
  bundle-relative KK ICD, firmware, and `gvproxy`. Local builds use Apple Development (or an
  explicit ad-hoc opt-in); published builds require Developer ID Application.
- The worker carries `com.apple.security.hypervisor`; tests codesign it per-build.
- Guest tools are an out-of-band tarball (`limina-guest-tools-<ver>.tar.zst`) built by
  `scripts/provision/f44/build-all.sh` and installed by `install-enhanced.sh` run manually
  in the guest ([[limina-enh-delivery]]).

Every gap below is between this and "a stranger downloads limina and it just works."

## 2. Channel decision: direct download (+ Homebrew cask), NOT the App Store

The Mac App Store is ruled out, not deferred:
- The App Store sandbox is incompatible with our process model (spawning the entitled
  worker, the future `SMAppService` root helper for USB/vmnet, CGEventTap pointer capture).
- MAS would grant `com.apple.vm.device-access` (USB without root) but forbids most of the
  rest — the M7 investigation already concluded root-helper is the Developer-ID path.

**Primary channel: a signed, notarized DMG from GitHub Releases; convenience channel: a
Homebrew cask** (`brew install --cask limina`) pointing at the same artifact. The DMG is
also what the in-app updater consumes.

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
- **Notarize** the outer distribution container (`notarytool submit --wait`), staple the DMG,
  and assess it with Gatekeeper. Nested code is signed and verified inside-out before submission.
- **CI:** a signing job on a self-hosted Apple-Silicon runner (the same one the L2 boot
  tests need — one machine, two reasons to exist). The certificate and a `notarytool`
  keychain profile live in that runner's login keychain; no signing material enters PR jobs.
  Explicit ad-hoc signing stays available as a no-secrets local fallback.

### Development channel

`.github/workflows/development-release.yml` is manually dispatched and replaces the rolling
`development` GitHub prerelease only after all of these succeed:

1. fork checkouts match `third_party/manifest.toml` and have no tracked source changes;
2. the KK/zink, epoxy, virglrenderer, and firmware outputs are refreshed;
3. the full HVF boot suite passes;
4. the release-profile app is signed with Developer ID, packaged, notarized, stapled, and
   accepted by Gatekeeper.

The release is rolling, but its DMG filename, plist metadata, checksum, commit link, and uploaded
`release-inputs.txt` identify the exact build. A development release is an optimized Cargo release
build, not a debug build.

The workflow only accepts dispatches from `main`. The `development-release` GitHub environment
should also restrict deployments to `main`, so a branch cannot use the persistent runner's signing
identity. The dedicated runner needs the custom `limina-release` label and the native build
environment from `docs/dev-onboarding.md`. Install the Developer ID Application certificate in the
runner's unlocked login keychain, then create the default notary profile once:

```sh
xcrun notarytool store-credentials limina-notary \
  --key /path/to/AuthKey_KEYID.p8 --key-id KEYID --issuer ISSUER_UUID
```

If the runner has multiple identities or uses another profile name, set the
`LIMINA_SIGN_IDENTITY` (certificate SHA-1) and `LIMINA_NOTARY_KEYCHAIN_PROFILE` variables in the
`development-release` GitHub environment.

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

1. **Validate the Developer-ID-signed/notarized development DMG on a clean second Mac with
   quarantine intact** — flushes out every hardened-runtime/dyld assumption before calling the
   channel stable.
2. Promote the proven pipeline to versioned stable releases; add the Homebrew cask.
3. Guest-tools manifest + `limina install-guest-tools` (closes two backlog items).
4. Sparkle appcast wiring.
5. Add scheduled cadence after the dedicated runner has proven reliable; manual dispatch avoids
   publishing stale host-native inputs during bring-up.

## 8. Open questions

- Whether the GL closure can shrink (the zink→LLVM host-GL stack dominates the 245 MB;
  llvmpipe-less builds or a pared LLVM are possible but low-priority).
- Payload hosting for guest-tools when releases become frequent (GitHub bandwidth is fine
  to start).
- A `limina import` (Parallels migration, `dogfooding-parallels-migration.md`) shipping in
  the same release as the first public DMG — the highest-leverage acquisition feature.
