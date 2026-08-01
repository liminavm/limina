// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Secure-Enclave signing shim for the virtual FIDO authenticator (M14).
//
// The host CTAP2 core (Rust, `crates/limina/src/fido.rs`) needs three things the
// Secure Enclave provides and that Spike A proved work with zero entitlements:
// create a userPresence-gated P-256 key, read its public key, and produce an
// ES256 signature behind a Touch ID prompt. Persistence is the CryptoKit key
// *blob* (`dataRepresentation`) — the only no-keychain, no-provisioning-profile
// path (Spike A test 4 = AMFI kill with keychain entitlements; test 5 = blob
// round-trips). CryptoKit has no C/ObjC surface, so this bridge is Swift.
//
// C ABI: every call writes into a caller-provided buffer and returns the byte
// count, or a negative error code — no cross-language allocation to free.
//   -1 access-control creation failed   -2 output buffer too small
//   -3 CryptoKit/Enclave error (key create/load/sign)
//
// The signed message is `authenticatorData || clientDataHash`; CryptoKit's
// P256.Signing hashes it with SHA-256 internally, i.e. ECDSA-SHA256 = WebAuthn
// ES256, and `derRepresentation` is exactly the ASN.1 signature WebAuthn wants.

import CryptoKit
import Foundation
import LocalAuthentication
import Security

/// Is a Secure Enclave present (Apple silicon / T2)? The Rust side gates on this
/// and refuses to advertise the `fido` capability when it returns 0.
@_cdecl("limina_sep_available")
public func limina_sep_available() -> Int32 {
    SecureEnclave.isAvailable ? 1 : 0
}

/// userPresence access control: every *signature* requires Touch ID; key creation
/// and public-key reads do not.
private func makeAccessControl() -> SecAccessControl? {
    SecAccessControlCreateWithFlags(
        nil,
        kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        [.privateKeyUsage, .userPresence],
        nil)
}

/// Create a fresh SEP key; write its opaque blob to `out`. Returns the blob length.
@_cdecl("limina_sep_create")
public func limina_sep_create(_ out: UnsafeMutablePointer<UInt8>, _ cap: Int) -> Int {
    guard let ac = makeAccessControl() else { return -1 }
    do {
        let key = try SecureEnclave.P256.Signing.PrivateKey(accessControl: ac)
        let blob = key.dataRepresentation
        if blob.count > cap { return -2 }
        blob.copyBytes(to: out, count: blob.count)
        return blob.count
    } catch {
        return -3
    }
}

/// Read the X9.63 uncompressed public key (65 bytes: 0x04 || X || Y) for a blob.
@_cdecl("limina_sep_pubkey")
public func limina_sep_pubkey(
    _ blob: UnsafePointer<UInt8>, _ blobLen: Int,
    _ out: UnsafeMutablePointer<UInt8>, _ cap: Int
) -> Int {
    let data = Data(bytes: blob, count: blobLen)
    do {
        let key = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: data)
        let x963 = key.publicKey.x963Representation
        if x963.count > cap { return -2 }
        x963.copyBytes(to: out, count: x963.count)
        return x963.count
    } catch {
        return -3
    }
}

/// Does this host have a Touch ID sensor? The Rust side gates the `fingerprint`
/// capability on sensor PRESENCE — deliberately NOT on `canEvaluatePolicy`. The latter
/// answers "can a biometric prompt succeed *right now*", which reports transient
/// UNavailability (a closed lid / clamshell, or a Mac that needs a fresh password
/// unlock to re-arm Touch ID) as `false` with `systemCancel`, even though the actual
/// `evaluatePolicy` prompt would work — wrongly hiding the reader. `biometryType`
/// (populated after a priming `canEvaluatePolicy` call — an Apple API quirk) reflects
/// hardware presence regardless of the current availability, mirroring how the FIDO
/// authenticator gates on `SecureEnclave.isAvailable`. Momentary unavailability is then
/// the verify prompt's problem: a failed `evaluatePolicy` just degrades to a clean
/// no-match, exactly like a real reader whose finger didn't match.
@_cdecl("limina_sep_has_touchid")
public func limina_sep_has_touchid() -> Int32 {
    let ctx = LAContext()
    // biometryType is only populated after a canEvaluatePolicy call; ignore the bool
    // (it tracks momentary availability) and read the sensor type it leaves behind.
    _ = ctx.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: nil)
    return ctx.biometryType == .touchID ? 1 : 0
}

/// The in-flight verify's context and the caller's token for it, so `limina_sep_cancel` can
/// dismiss exactly the prompt it meant to. `cancelledToken` remembers a cancel that arrived
/// before its sheet existed; because tokens are never reused, a cancel that loses the race
/// against its own prompt finishing simply matches nothing later.
private let verifyLock = NSLock()
private var verifyContext: LAContext?
private var verifyToken: UInt64 = 0
private var cancelledToken: UInt64 = 0

/// Prompt a biometrics-only Touch ID sheet (`reason` is the sheet text) and report the
/// outcome: 1 = a trusted finger matched, 0 = no match / unusable sensor, -2 = the prompt was
/// *cancelled* (the user dismissed the sheet, `limina_sep_cancel` invalidated it, the system
/// pulled it away, or biometry is locked out / unavailable).
///
/// The cancel/no-match split is load-bearing for the fingerprint reader: a no-match is the
/// "wrong finger, try again" a caller SHOULD retry, while a cancel means the human chose not
/// to authenticate this way — retrying just re-summons a sheet they dismissed. The caller maps
/// the two onto different guest-visible outcomes (`crates/limina/src/moc/mod.rs`).
///
/// `.deviceOwnerAuthenticationWithBiometrics` has NO passcode fallback, the honest mapping of a
/// fingerprint sensor. `evaluatePolicy` is asynchronous (completion handler), unlike the
/// synchronous CryptoKit `signature(for:)`, so we block on a semaphore until the sheet resolves.
@_cdecl("limina_sep_verify")
public func limina_sep_verify(_ token: UInt64, _ reason: UnsafePointer<CChar>?) -> Int32 {
    let ctx = LAContext()
    verifyLock.lock()
    if cancelledToken == token {
        // Cancelled before the sheet existed — honour it rather than opening one nobody wants.
        cancelledToken = 0
        verifyLock.unlock()
        return LIMINA_SEP_CANCELLED
    }
    verifyContext = ctx
    verifyToken = token
    verifyLock.unlock()

    defer {
        verifyLock.lock()
        verifyContext = nil
        verifyToken = 0
        verifyLock.unlock()
    }

    let reasonText = reason.map { String(cString: $0) } ?? "Verify your fingerprint"
    let sem = DispatchSemaphore(value: 0)
    var matched = false
    var failure: Error?
    ctx.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: reasonText) {
        success, error in
        matched = success
        failure = error
        sem.signal()
    }
    // Fail safe: if the completion never fires (should be impossible), treat it as a decline after
    // a generous timeout rather than wedging the serve thread — and thus the reader — forever.
    if sem.wait(timeout: .now() + 120) == .timedOut {
        return 0
    }
    if matched {
        return 1
    }
    return isCancellation(failure) ? LIMINA_SEP_CANCELLED : 0
}

/// The `-2` the Rust side reads as "cancelled" (see `crates/limina/src/sep.rs`).
private let LIMINA_SEP_CANCELLED: Int32 = -2

/// Does this `evaluatePolicy` failure mean "don't ask again", as opposed to "that finger
/// didn't match"? Cancels (by the user, by us via `invalidate()`, or by the system) and the
/// states where biometry simply cannot succeed — locked out, not available, not enrolled —
/// are all pointless to retry; `authenticationFailed` is the genuine no-match. Matched
/// symbolically against `LAError.Code`, never by raw code number.
private func isCancellation(_ error: Error?) -> Bool {
    guard let code = (error as? LAError)?.code else { return false }
    switch code {
    case .userCancel, .appCancel, .systemCancel, .userFallback,
        .biometryLockout, .biometryNotAvailable, .biometryNotEnrolled:
        return true
    default:
        return false
    }
}

/// Dismiss the in-flight Touch ID sheet, if any: `invalidate()` cancels a pending
/// `evaluatePolicy` (its completion fires with `LAError.appCancel`). Called when the *guest*
/// abandons the request that opened the sheet — a fingerprint verify whose USB transfer the
/// guest cancelled — so the Mac stops asking for something nobody is waiting for.
@_cdecl("limina_sep_cancel")
public func limina_sep_cancel(_ token: UInt64) {
    verifyLock.lock()
    if verifyContext != nil, verifyToken == token {
        verifyContext?.invalidate()
    } else {
        // Its sheet is not up (yet): leave the cancel for the prompt with that token to find.
        cancelledToken = token
    }
    verifyLock.unlock()
}

/// Sign `msg` with the blob's key behind a Touch ID prompt (`reason` is the sheet
/// text). Writes the ASN.1 DER ECDSA signature to `out`. Returns its length; a
/// user cancel / no-match surfaces as -3.
@_cdecl("limina_sep_sign")
public func limina_sep_sign(
    _ blob: UnsafePointer<UInt8>, _ blobLen: Int,
    _ msg: UnsafePointer<UInt8>, _ msgLen: Int,
    _ reason: UnsafePointer<CChar>?,
    _ out: UnsafeMutablePointer<UInt8>, _ cap: Int
) -> Int {
    let data = Data(bytes: blob, count: blobLen)
    let message = Data(bytes: msg, count: msgLen)
    let ctx = LAContext()
    if let reason = reason {
        ctx.localizedReason = String(cString: reason)
    }
    do {
        let key = try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: data, authenticationContext: ctx)
        let sig = try key.signature(for: message)
        let der = sig.derRepresentation
        if der.count > cap { return -2 }
        der.copyBytes(to: out, count: der.count)
        return der.count
    } catch {
        return -3
    }
}
