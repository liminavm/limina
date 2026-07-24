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

- Per-VM store path wired to the VM state dir (currently `LIMINA_FIDO_STORE`).
- Deliver the FIDO agent + SEP-signed supervisor via the enhanced payload /
  app-bundle (build-app.sh must copy `liblimina_sep.dylib` into Frameworks and
  fix its rpath — dev/test bakes the rpath to OUT_DIR).
- ✅ Browser oracle: **webauthn.io in guest Firefox** registered + logged in a
  passkey with a host Touch ID prompt (2026-07-24, user-verified). Firefox uses
  its own CTAP stack (authenticator-rs), not libfido2 — so this independently
  confirms the CTAP2/canonical-CBOR implementation across two clients.
- PAM `pam_u2f` recipe, then the stock-tier xHCI + impersonated-MOC-fingerprint
  wave (roadmap M14).
