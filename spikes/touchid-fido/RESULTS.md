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

## Next (Spike B)

Guest side: `limina-agent` creates a `/dev/uhid` FIDO HID device (usage page
0xF1D0), bridges CTAP frames over a new vsock control-plane channel. Oracle
ladder: `fido2-token -L` → `fido2-cred`/`fido2-assert` → webauthn.io in
Firefox/Chromium on the enhanced image. Then join the halves (evaluate
`passkey-rs` for the CTAP2 state machine).
