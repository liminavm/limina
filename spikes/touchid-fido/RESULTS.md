# Spike A: Touch ID + Secure Enclave signing from a limina-shaped process

**Date:** 2026-07-24 · **Host:** macOS 26.5, M1 Max (Touch ID in keyboard) ·
**Signing:** Apple Development identity (team WDNHP64H9G), plain codesign, no
provisioning profile — i.e. exactly how `scripts/build-app.sh` signs the app.

## Question

Can the host half of a virtual FIDO2 authenticator — Touch-ID-gated ES256
signatures from Secure Enclave keys — run in our process shape (terminal-
launched CLI, dev-cert codesign, no app bundle, no profile)? And what does
credential *persistence* require, entitlement-wise?

## Method

`probe.swift`, built+signed by `build.sh` (identity discovery mirrors
`build-app.sh`, ad-hoc fallback is loud). Run seated; 2 Touch ID touches.
`PROBE_SKIP_TOUCH=1` skips the interactive tests for entitlement-only reruns.

## Results (all observed, one run each)

| # | Test | Result |
|---|------|--------|
| 1 | `LAContext.canEvaluatePolicy(biometrics)` | **PASS** — `biometryType=touchID` |
| 2 | Bare `evaluatePolicy` biometric prompt from the CLI | **PASS** — system sheet appeared with our reason string; user authenticated |
| 3 | Ephemeral SEP P-256 key (`.userPresence` AC): create → Touch ID prompt → `SecKeyCreateSignature` ES256 → verify | **PASS** — 72 B DER sig verifies against the 65 B X9.63 pubkey. This is the WebAuthn ES256 primitive, gated on the physical sensor. |
| 4 | Permanent SEP key in the data-protection keychain, plain signature | **FAIL (expected)** — `-34018 errSecMissingEntitlement` |
| 4b | Same, signed with `com.apple.application-identifier` + `keychain-access-groups` but **no provisioning profile** | **KILLED at spawn** (SIGKILL/exit 137) — AMFI rejects profile-backed entitlements without a profile |
| 5 | CryptoKit `SecureEnclave.P256.Signing.PrivateKey` → `dataRepresentation` blob → re-init → sign → verify | **PASS** — 284 B opaque enclave-encrypted blob round-trips to the same key |

## Conclusions

1. **The host authenticator design is viable as-is.** Prompt + SEP ES256
   signing work from a terminal-launched, dev-cert-signed CLI with **zero
   entitlements**. No app-bundle, TCC, or profile prerequisite for the core
   flow. (Where it should *live* is still the supervisor/app process — it owns
   UI — but nothing forces that.)
2. **Persistence: use CryptoKit key blobs, not the keychain.** The
   data-protection keychain needs profile-backed entitlements (test 4/4b), but
   test 5 shows we don't need it: store each credential's `dataRepresentation`
   (encrypted to this Mac's enclave, useless off-machine) alongside its
   metadata (RP ID, user handle, credential ID) in limina's per-VM state.
   Product keys add `.userPresence`/biometry access control (CryptoKit's
   `init(accessControl:authenticationContext:)`) so every signature prompts —
   test 3 already proved the AC-gated prompt path.
3. **Trap confirmed for later:** if we ever do want keychain storage (e.g.
   iCloud-sync-adjacent features), the app needs an embedded provisioning
   profile — plain `codesign --entitlements` bricks the binary (exit 137).
   Same failure class as the TCC ad-hoc CDHash trap: sign-shape decides
   runtime behavior.
4. Reason strings render in the system sheet (`LAContext.localizedReason` /
   `evaluatePolicy` reason), so per-RP prompts ("'dogfood-guest' wants to sign in
   to github.com") will work.

## Spike B — GREEN (2026-07-24, same day)

The uhid transport shipped as real code, not spike scratch: `limina-proto`
`CHANNEL_FIDO`/`FIDO_REPORT`, agent `guest/limina-agent/src/fido.rs` (uhid FIDO
device, created on host WELCOME `fido` cap, connection-scoped so a dead bridge
never leaves a zombie device), host `crates/limina/src/fido.rs` (complete
CTAPHID framing + minimal CTAP2 getInfo; 6 unit tests).

Verified on a live F44 enhanced-image boot (EFI+venus, `cargo xtask run`),
new agent delivered over SSH:

- agent log: `virtual FIDO device up`
- guest `/dev/hidraw0`: `HID_NAME=limina Touch ID FIDO`, uaccess ACL applied —
  **systemd's fido-id detected the device by usage page** (vendor-neutral
  VID/PID 0x1d6b:0x0f1d, exactly the stock-zero-config premise)
- `fido2-token -L` lists the device; `fido2-token -I` completes CTAPHID INIT
  (proto 0x02, caps `cbor,nomsg`) **and a CTAP2 getInfo round-trip**
  (`FIDO_2_0`, aaguid `6c…2121` = "limina-touchid!!") through the full chain:
  hidraw → uhid → agent → vsock → host state machine → back.

## CTAP2 core — GREEN end-to-end (2026-07-24, same day)

The virtual authenticator is now a real WebAuthn platform authenticator. Host
CTAP2 was **hand-rolled** (ES256-only, consistent with our hand-rolled CTAPHID;
no `passkey-rs`) over a **Swift CryptoKit SEP shim** (`swift/fido_sep.swift` +
`build.rs` + `src/sep.rs`) — the only no-entitlement SEP persistence (blob, not
keychain). Commands: `fido/ctap2.rs` (makeCredential = packed self-attestation,
getAssertion = SEP-signed, getInfo), `fido/store.rs` (per-VM passkey store).

**Verified on a live F44 enhanced guest** with the real `fido2-cred` / `fido2-
assert` tools driving `/dev/hidraw0`:
- `fido2-cred -M … | fido2-cred -V` → registration **attestation verified**;
  credential id + EC public key written.
- `fido2-assert -G … | fido2-assert -V` → **`ASSERTION-VERIFIED-OK`**: libfido2
  cryptographically verified the ES256 signature against the enclave public key.
- Both steps prompted host Touch ID (register, then sign-in). Counter increments.

### Two bugs, both root-caused by instrumenting the wire (not guessing)

1. **Missing CTAPHID keepalive.** makeCredential/getAssertion block for seconds
   on the Touch ID prompt; CTAPHID requires `KEEPALIVE(processing)` meanwhile or
   the client times out (`FIDO_ERR_RX`). Fixed: `on_report` returns an `Outcome`;
   the CBOR command runs on a worker thread while `control.rs` pumps keepalive
   every 100 ms. (Real browsers need this too.)
2. **Non-canonical CBOR.** getInfo options were `rk,up,plat,uv`; CTAP2 canonical
   order is shorter-keys-first then bytewise = `rk,up,uv,plat`. libfido2's
   `FIDO_DEBUG` trace showed `ctap_check_cbor: invalid cbor … iterator < 0 on
   i=2`, then a U2F fallback → RX. A raw CTAPHID probe bypassing libfido2 had
   already proven our multi-packet + keepalive + SEP path correct (92-byte
   request, 32 keepalives during the touch, valid packed attestation), which is
   what pinned the fault to libfido2's stricter parse. Regression test added.

## Next (productization, not core)

- ✅ Per-VM store path wired, and **persistence is now the default everywhere** (2026-08-08).
  Managed VMs keep passkeys in the bundle beside `state.toml`; a flat `--disk` run keeps them in
  a sidecar beside its disk, `<disk>.limina-fido.json` — the same shape (and the same reasoning)
  as `<disk>.limina-suspend.bin`: the credentials belong to the guest that registered them and
  travel with it.
  A store is also a **precondition**, not a convenience: a run that can keep none — read-only
  disk, or no disk at all (L1 initrd, ISO) — gets **no authenticator**. It used to fall back to an
  in-memory store, so such a run would register a passkey a real site then kept forever while our
  half died with the process: an account entry nothing can satisfy, and a lockout if it was the
  only one. No authenticator is a state every browser handles; an amnesiac one is not.
  `LIMINA_FIDO_STORE` overrides outright (how the L1 tests opt in).
- Deliver the FIDO agent + SEP-signed supervisor via the enhanced payload /
  app-bundle (build-app.sh must copy `liblimina_sep.dylib` into Frameworks and
  fix its rpath — dev/test bakes the rpath to OUT_DIR).
- ✅ Browser oracle: **webauthn.io in guest Firefox** registered + logged in a
  passkey with a host Touch ID prompt (2026-07-24, user-verified). Firefox uses
  its own CTAP stack (authenticator-rs), not libfido2 — so this independently
  confirms the CTAP2/canonical-CBOR implementation across two clients.
- PAM `pam_u2f` recipe, then the stock-tier xHCI + impersonated-MOC-fingerprint
  wave (roadmap M14).

## The GitHub passkey report — closed, no bug (2026-08-08)

Reported: GitHub wouldn't take our authenticator as a passkey, and its dialog said *"this browser
reports partial passkey support"*. Two separate things, neither ours:

**The registration failure was user error** — the flow asks to verify with an *existing* passkey
before adding a new one, and that prompt was read as the new-passkey prompt. Registration on a
second guest then succeeded. No GitHub bug to fix.

**The "partial passkey support" banner is Firefox-on-Linux, device-independent.** GitHub feature-
detects `PublicKeyCredential.isConditionalMediationAvailable()` (passkey autofill / conditional
UI). Firefox has the API from 122, but conditional UI only works where the OS provides a
credential manager — Windows 11, macOS, Android, iOS 16+ — and **not on Linux**. The same banner
is reported by Firefox and Firefox-derived users with no limina device anywhere near them. Nothing
we can emit from CTAP changes it.

### Is `plat: true` on a USB-attached authenticator a contradiction? — Measured: inert

Our `getInfo` advertises `plat` (see `fido/ctap2.rs`), which looked like something a browser could
reasonably object to. Read in Firefox's actual CTAP stack (`authenticator` 0.5.0, the crate Gecko
vendors) rather than guessed at:

- **`plat` is purely reportorial.** It appears in exactly one decision, in both
  `make_credentials.rs:337` and `get_assertion.rs:369`:
  `Some(info) if info.options.platform_device => AuthenticatorAttachment::Platform`. That string is
  handed to the RP as `authenticatorAttachment`. **Nothing filters or rejects a device on it.**
- **Transport is a separate axis we don't control.** `authrs_bridge/src/lib.rs::get_transports`:
  *"In production, we only support the 'usb' transport"* — it returns `usb` unconditionally. So an
  RP sees `attachment: "platform"` with `transports: ["usb"]`. Odd-looking, but Firefox itself
  produces that pairing; it isn't something the RP can hold against us specifically.
- **A `platform` request never reaches any device.** `authrs_bridge` rejects a request carrying
  `authenticatorAttachment: "platform"` with `NS_ERROR_FAILURE` before enumerating. So `plat: true`
  cannot gain us anything on Linux Firefox, and its absence cannot lose us anything either.

**Verdict: leave it true, and this is why.** CTAP defines `plat` as *"the device is attached to the
client and therefore can't be removed and used on another client"* (quoted verbatim in
`get_info.rs:69`). That is exactly true of ours: the keys are non-exportable SEP blobs bound to
this Mac, and the store is per-VM. `true` is the spec-accurate statement. The change would be
cosmetic, and the one thing it could buy — consistency with the `usb` transport string — is a
string Firefox hardcodes anyway.

### The probe run, and an A/B that retired a documented claim (2026-08-08)

`spikes/fido-passkey/verify-credential.sh` in a live F44 guest (USB gadget, no guest components),
`fido2-cred -M -r -v` then `-V`:

| Supervisor signature | Device attaches | getInfo | Touch ID sheet | `fido2-cred -V` |
|---|---|---|---|---|
| Apple Development (`WDNHP64H9G`) | yes | `FIDO_2_0` | appeared, user authenticated | **PASS** |
| **ad-hoc, linker-signed** (plain `cargo build`) | yes | `FIDO_2_0` | appeared, user authenticated | **PASS** |

Both wrote a 284-byte enclave blob to the store. So the credential we mint is well-formed by an
independent verifier's standard, and **`docs/fido-authenticator.md`'s "the supervisor must be
Apple-Development-signed — ad-hoc signing can't use the enclave" was wrong** and has been
corrected. That claim was an over-generalization of test 4/4b above: it is the *data-protection
keychain* that needs profile-backed entitlements. The CryptoKit `dataRepresentation` path we
actually use needs neither entitlement nor identity.

Why it was worth running the dev-signed leg first even though both pass: `sep::available()` is
`SecureEnclave.isAvailable`, i.e. **hardware presence only** — it never asks whether *this
process* may use the enclave. So an ad-hoc build advertises the authenticator regardless, and a
makeCredential failure would have been ambiguous between "our CTAP is wrong" and "this binary
can't reach the enclave". The baseline made the second leg attributable to one variable. The
happy outcome also dissolves the follow-up it suggested (tightening the capability gate to prove
the enclave is usable): no configuration was found that advertises an authenticator unable to
mint.

Worth knowing if this comes back: the reasoning above says nothing about clients we haven't read
(Chrome, libfido2 policy layers). The cheap empirical check is **webauthn.io in the guest, which
prints `authenticatorAttachment` for a registration** — that measures what an RP actually receives,
which `fido2-cred -V` cannot show.
