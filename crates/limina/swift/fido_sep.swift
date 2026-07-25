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

/// Can this host perform a biometric verification right now? True only when a Touch
/// ID sensor is present AND has an enrolled finger (`canEvaluatePolicy` fails on a
/// SEP-but-no-Touch-ID desktop, or with no enrolled finger). The Rust side gates the
/// `fingerprint` capability on this — NOT on `limina_sep_available()` (SEP presence),
/// so a Mac that can never satisfy a biometric prompt never advertises the reader.
@_cdecl("limina_sep_can_verify")
public func limina_sep_can_verify() -> Int32 {
    LAContext().canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: nil) ? 1 : 0
}

/// Prompt a biometrics-only Touch ID sheet (`reason` is the sheet text) and report
/// whether a trusted finger matched: 1 = matched, 0 = declined / failed / no sensor.
/// This is the fingerprint reader's "match" primitive — it produces no crypto (the
/// guest checks none), just the boolean "an authorized finger was presented."
/// `.deviceOwnerAuthenticationWithBiometrics` has NO passcode fallback, the honest
/// mapping of a fingerprint sensor. `evaluatePolicy` is asynchronous (completion
/// handler), unlike the synchronous CryptoKit `signature(for:)`, so we block on a
/// semaphore until the sheet resolves.
@_cdecl("limina_sep_verify")
public func limina_sep_verify(_ reason: UnsafePointer<CChar>?) -> Int32 {
    let ctx = LAContext()
    let reasonText = reason.map { String(cString: $0) } ?? "Verify your fingerprint"
    let sem = DispatchSemaphore(value: 0)
    var matched = false
    ctx.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: reasonText) {
        success, _ in
        matched = success
        sem.signal()
    }
    sem.wait()
    return matched ? 1 : 0
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
