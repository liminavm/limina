// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Spike A: prove Touch ID + Secure Enclave signing works from a limina-shaped
// CLI process (terminal-launched, codesigned, no app bundle, no provisioning
// profile). This is the host half of the virtual-FIDO-authenticator design:
// if [3] passes, we have the ES256 signing primitive WebAuthn needs, gated on
// the physical sensor.
//
// Tests:
//  [1] biometry availability (LAContext.canEvaluatePolicy)          — no touch
//  [2] bare biometric prompt (evaluatePolicy)                       — touch #1
//  [3] ephemeral SEP P-256 key: create, ES256-sign, verify          — touch #2
//  [4] INFO: permanent SEP key in the data-protection keychain      — no touch
//      (entitlement probe: expected to fail without keychain-access-groups;
//       tells us what credential persistence will need)
//
// Exit code = number of FAILed tests ([4] is informational, never counted).

import Foundation
import LocalAuthentication
import Security

var failures = 0

func report(_ n: Int, _ name: String, _ ok: Bool, _ detail: String) {
    print("[\(n)] \(ok ? "PASS" : "FAIL") \(name) — \(detail)")
    if !ok { failures += 1 }
}

func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

func cfDesc(_ e: Unmanaged<CFError>?) -> String {
    guard let e = e?.takeRetainedValue() else { return "no error" }
    return String(describing: e)
}

// [1] biometry availability -------------------------------------------------
let probeCtx = LAContext()
var availErr: NSError?
let canBio = probeCtx.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics,
                                        error: &availErr)
let bioType: String
switch probeCtx.biometryType {
case .touchID: bioType = "touchID"
case .faceID: bioType = "faceID"
default: bioType = "none/other(\(probeCtx.biometryType.rawValue))"
}
report(1, "biometry availability", canBio,
       "biometryType=\(bioType)"
       + (availErr.map { " err=\($0.localizedDescription)" } ?? ""))

// [2] bare biometric prompt -------------------------------------------------
// PROBE_SKIP_TOUCH=1 skips the interactive tests [2]/[3] so entitlement-only
// reruns (test [4]) don't need a human at the sensor.
let skipTouch = ProcessInfo.processInfo.environment["PROBE_SKIP_TOUCH"] == "1"
if skipTouch {
    print("[2] SKIP evaluatePolicy biometric prompt — PROBE_SKIP_TOUCH=1")
    print("[3] SKIP SEP ephemeral create+sign+verify — PROBE_SKIP_TOUCH=1")
} else if canBio {
    let sem = DispatchSemaphore(value: 0)
    var ok = false
    var detail = ""
    let ctx = LAContext()
    print("    ... Touch ID prompt 1/2 should be on screen NOW (bare evaluatePolicy)")
    ctx.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics,
                       localizedReason: "limina spike A — prompt 1 of 2 (bare biometric check)") { success, error in
        ok = success
        detail = success ? "user authenticated"
                         : "error=\(error?.localizedDescription ?? "?")"
        sem.signal()
    }
    sem.wait()
    report(2, "evaluatePolicy biometric prompt", ok, detail)
} else {
    report(2, "evaluatePolicy biometric prompt", false,
           "skipped: biometry unavailable (lid closed? no Touch ID?)")
}

// [3] ephemeral SEP key: create + sign (touch) + verify ---------------------
if !skipTouch {
var acErr: Unmanaged<CFError>?
let ac = SecAccessControlCreateWithFlags(nil,
    kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
    [.privateKeyUsage, .userPresence], &acErr)
if let ac = ac {
    let signCtx = LAContext()
    signCtx.localizedReason = "limina spike A — prompt 2 of 2 (Secure Enclave ES256 signature)"
    let attrs: [String: Any] = [
        kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
        kSecAttrKeySizeInBits as String: 256,
        kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
        kSecUseAuthenticationContext as String: signCtx,
        kSecPrivateKeyAttrs as String: [
            kSecAttrIsPermanent as String: false,
            kSecAttrAccessControl as String: ac,
        ] as [String: Any],
    ]
    var keyErr: Unmanaged<CFError>?
    if let priv = SecKeyCreateRandomKey(attrs as CFDictionary, &keyErr) {
        var challenge = Data(count: 32)
        _ = challenge.withUnsafeMutableBytes {
            SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!)
        }
        print("    ... Touch ID prompt 2/2 should be on screen NOW (SEP signature)")
        var sigErr: Unmanaged<CFError>?
        if let sig = SecKeyCreateSignature(priv, .ecdsaSignatureMessageX962SHA256,
                                           challenge as CFData, &sigErr) as Data? {
            let pub = SecKeyCopyPublicKey(priv)!
            var verErr: Unmanaged<CFError>?
            let verified = SecKeyVerifySignature(pub, .ecdsaSignatureMessageX962SHA256,
                                                 challenge as CFData, sig as CFData,
                                                 &verErr)
            var expErr: Unmanaged<CFError>?
            let pubData = SecKeyCopyExternalRepresentation(pub, &expErr) as Data?
            report(3, "SEP ephemeral create+sign+verify", verified,
                   verified
                   ? "ES256 sig verifies; DER sig (\(sig.count)B) = \(hex(sig.prefix(12)))…; "
                     + "pub X9.63 (\(pubData?.count ?? 0)B) = \(hex((pubData ?? Data()).prefix(12)))…"
                   : "verify failed: \(cfDesc(verErr))")
        } else {
            report(3, "SEP ephemeral create+sign+verify", false,
                   "sign: \(cfDesc(sigErr))")
        }
    } else {
        report(3, "SEP ephemeral create+sign+verify", false,
               "create: \(cfDesc(keyErr))")
    }
} else {
    report(3, "SEP ephemeral create+sign+verify", false,
           "SecAccessControlCreateWithFlags: \(cfDesc(acErr))")
}
}

// [4] INFO: permanent SEP key in the data-protection keychain ---------------
let tag = "eti.noronha.limina.spike-a".data(using: .utf8)!
let permAttrs: [String: Any] = [
    kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
    kSecAttrKeySizeInBits as String: 256,
    kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
    kSecAttrLabel as String: "limina-spike-a-permanent",
    kSecUseDataProtectionKeychain as String: true,
    kSecPrivateKeyAttrs as String: [
        kSecAttrIsPermanent as String: true,
        kSecAttrApplicationTag as String: tag,
    ] as [String: Any],
]
var permErr: Unmanaged<CFError>?
if SecKeyCreateRandomKey(permAttrs as CFDictionary, &permErr) != nil {
    print("[4] INFO permanent-key probe — CREATED (entitlements sufficient); deleting")
    let del: [String: Any] = [
        kSecClass as String: kSecClassKey,
        kSecAttrApplicationTag as String: tag,
        kSecUseDataProtectionKeychain as String: true,
    ]
    let st = SecItemDelete(del as CFDictionary)
    if st != errSecSuccess { print("    (cleanup SecItemDelete status=\(st))") }
} else {
    print("[4] INFO permanent-key probe — NOT created: \(cfDesc(permErr))")
    print("    (expected without keychain-access-groups; persistence plan in RESULTS.md)")
}

// [5] CryptoKit SEP key blob persistence (no keychain, no entitlements) -----
// SecureEnclave.P256 keys export an opaque dataRepresentation encrypted to
// this Mac's enclave — storable in a plain file (limina's per-VM state dir)
// and useless off-machine. If the round-trip works, credential persistence
// needs NO keychain and NO provisioning profile. Created without access
// control here so the round-trip signs promptless; product keys add
// .userPresence and an LAContext.
import CryptoKit
do {
    let k1 = try SecureEnclave.P256.Signing.PrivateKey()
    let blob = k1.dataRepresentation
    let k2 = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: blob)
    let msg = Data("limina spike A persistence".utf8)
    let sig = try k2.signature(for: msg)
    let ok = k1.publicKey.isValidSignature(sig, for: msg)
        && k1.publicKey.rawRepresentation == k2.publicKey.rawRepresentation
    report(5, "SEP blob persistence (CryptoKit dataRepresentation)", ok,
           ok ? "blob (\(blob.count)B) restores to the same key; sig verifies"
              : "restored key mismatch or bad signature")
} catch {
    report(5, "SEP blob persistence (CryptoKit dataRepresentation)", false,
           "\(error)")
}

print(failures == 0 ? "ALL PASS" : "\(failures) FAILURE(S)")
exit(Int32(failures))
