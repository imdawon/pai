// Secure Enclave keys for pai's sealed secrets file (src/sealed.rs). Built into a static library by
// build.rs on macOS. Only ever called from `pai keystore-helper`, a short-lived child process, so a
// crash in CryptoKit takes down only that child.
import CryptoKit
import Foundation

@_cdecl("pai_se_available")
public func paiSeAvailable() -> Bool {
    SecureEnclave.isAvailable
}

// A new P-256 key-agreement key in the Secure Enclave, with no access-control flags, so using it
// needs no password, Touch ID or prompt. Writes its handle (dataRepresentation, usable only by this
// Mac's enclave and this user) and its x9.63 public key (65 bytes).
@_cdecl("pai_se_create")
public func paiSeCreate(_ out: UnsafeMutablePointer<UInt8>, _ cap: Int, _ outLen: UnsafeMutablePointer<Int>,
                        _ pub: UnsafeMutablePointer<UInt8>) -> Int32 {
    guard SecureEnclave.isAvailable else { return 1 }
    guard let key = try? SecureEnclave.P256.KeyAgreement.PrivateKey() else { return 2 }
    let handle = key.dataRepresentation
    guard handle.count <= cap else { return 3 }
    let publicKey = key.publicKey.x963Representation
    guard publicKey.count == 65 else { return 4 }
    handle.copyBytes(to: out, count: handle.count)
    outLen.pointee = handle.count
    publicKey.copyBytes(to: pub, count: 65)
    return 0
}

// The raw ECDH shared secret (32 bytes) between the enclave key and a software public key.
@_cdecl("pai_se_agree")
public func paiSeAgree(_ key: UnsafePointer<UInt8>, _ keyLen: Int, _ peer: UnsafePointer<UInt8>,
                       _ out: UnsafeMutablePointer<UInt8>) -> Int32 {
    let handle = Data(bytes: key, count: keyLen)
    let peerData = Data(bytes: peer, count: 65)
    guard let privateKey = try? SecureEnclave.P256.KeyAgreement.PrivateKey(dataRepresentation: handle) else { return 1 }
    guard let publicKey = try? P256.KeyAgreement.PublicKey(x963Representation: peerData) else { return 2 }
    guard let secret = try? privateKey.sharedSecretFromKeyAgreement(with: publicKey) else { return 3 }
    return secret.withUnsafeBytes { raw -> Int32 in
        guard raw.count == 32, let base = raw.bindMemory(to: UInt8.self).baseAddress else { return 4 }
        out.update(from: base, count: 32)
        return 0
    }
}
