//! pai's secrets file, sealed by the computer's own hardware so it opens only on that computer and
//! for that user, with no password and no prompt.
//!
//! - macOS: a P-256 key made inside the Secure Enclave. Each write makes a fresh software key pair,
//!   agrees a secret with the enclave key's public half and encrypts with AES-256-GCM; opening asks
//!   the enclave for the same agreement. The enclave key never leaves the chip; the file keeps only
//!   its handle (`dataRepresentation`), which works on no other Mac and for no other user.
//! - Windows: DPAPI for the current user (`CryptProtectData`, no UI).
//! - Elsewhere, or a Mac without a Secure Enclave: nothing to seal with, and pai keeps the 0600
//!   file it always has.
//!
//! The enclave is only ever called from a short-lived child process (`pai keystore-helper`) with a
//! time limit, and only after the handle's SHA-256 matches the one saved beside it: on macOS 27.2 a
//! damaged handle crashed the caller, and one crashed the per-user CryptoTokenKit daemon so every
//! enclave call on the account hung until the user logged out (tested 2026-11-08). privacy-ai
//! docs/THREAT-MODEL.md, "pai seals its secrets with the Secure Enclave or DPAPI".

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::{aead, agreement, hkdf, rand as ring_rand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// The sealed file's name, beside `secrets.json`.
pub const SEALED_FILE: &str = "secrets.sealed";
const INFO: &[u8] = b"pai-secrets-v1";
/// How long the child that calls the Secure Enclave may take. privacy-ai docs/SETTINGS.md.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const HELPER_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SecureEnclave,
    Dpapi,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::SecureEnclave => "sealed by this Mac's Secure Enclave",
            Kind::Dpapi => "sealed by Windows for this user (DPAPI)",
        }
    }
}

/// What this computer can seal with, for the secrets file at `secrets_json`. `PAI_KEYSTORE=file`
/// (read by the caller) turns sealing off. On a Mac, a sealed file already there answers without
/// asking the enclave; otherwise the child process asks, since even that call can hang when the
/// enclave's daemon is down.
pub fn available(secrets_json: &Path) -> Option<Kind> {
    #[cfg(target_os = "macos")]
    if sealed_path(secrets_json).exists() || helper("available", &[]).is_ok_and(|out| out == b"yes")
    {
        return Some(Kind::SecureEnclave);
    }
    let _ = secrets_json;
    #[cfg(windows)]
    return Some(Kind::Dpapi);
    #[allow(unreachable_code)]
    None
}

pub fn sealed_path(secrets_json: &Path) -> PathBuf {
    secrets_json.with_file_name(SEALED_FILE)
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    v: u32,
    kind: String,
    /// Secure Enclave: the key's handle (base64), its SHA-256 (hex) and its public key (x9.63).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_pub: Option<String>,
    /// Secure Enclave: this write's software public key and AES-GCM nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    data: String,
}

fn bad(path: &Path, what: &str) -> Error {
    Error::Keystore(format!("{} {what}", path.display()))
}

fn b64(path: &Path, v: &Option<String>, what: &str) -> Result<Vec<u8>> {
    v.as_deref()
        .and_then(|s| B64.decode(s).ok())
        .ok_or_else(|| bad(path, &format!("has no valid {what}")))
}

/// Seals `plain` for this computer. `previous` is the file being replaced: its enclave key is kept,
/// so the enclave isn't asked for a new key on every write.
pub fn seal(path: &Path, kind: Kind, plain: &[u8]) -> Result<Vec<u8>> {
    let sealed = match kind {
        Kind::SecureEnclave => {
            let previous = std::fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Sealed>(&b).ok());
            let (key, key_pub) = match previous
                .filter(|p| p.kind == "secure-enclave" && p.key.is_some() && p.key_pub.is_some())
            {
                Some(p) => (
                    b64(path, &p.key, "enclave key")?,
                    b64(path, &p.key_pub, "enclave public key")?,
                ),
                None => enclave_create()?,
            };
            let (peer, secret) = software_agree(&key_pub)?;
            let mut nonce = [0u8; 12];
            ring_rand::SecureRandom::fill(&ring_rand::SystemRandom::new(), &mut nonce)
                .map_err(|_| Error::Keystore("no randomness for the secrets file".into()))?;
            let data = aes(&secret, nonce, plain.to_vec(), true)?;
            Sealed {
                v: 1,
                kind: "secure-enclave".into(),
                key_sha256: Some(hex(&Sha256::digest(&key))),
                key: Some(B64.encode(&key)),
                key_pub: Some(B64.encode(&key_pub)),
                peer: Some(B64.encode(peer)),
                nonce: Some(B64.encode(nonce)),
                data: B64.encode(data),
            }
        }
        Kind::Dpapi => Sealed {
            v: 1,
            kind: "dpapi".into(),
            key: None,
            key_sha256: None,
            key_pub: None,
            peer: None,
            nonce: None,
            data: B64.encode(dpapi(plain, true)?),
        },
    };
    Ok(serde_json::to_vec_pretty(&sealed).expect("sealed serializes"))
}

/// Opens a sealed file's bytes, read from `path`.
pub fn open(path: &Path, bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let sealed: Sealed =
        serde_json::from_slice(bytes).map_err(|_| bad(path, "is not a sealed secrets file"))?;
    if sealed.v != 1 {
        return Err(bad(path, "was written by a newer pai. Run `pai update`"));
    }
    let data = B64
        .decode(&sealed.data)
        .map_err(|_| bad(path, "has no valid data"))?;
    match sealed.kind.as_str() {
        "secure-enclave" => {
            let key = b64(path, &sealed.key, "enclave key")?;
            // A damaged handle can crash the enclave's daemon for the whole account, so it is never
            // handed over unless it is byte for byte the one pai saved.
            if sealed.key_sha256.as_deref() != Some(hex(&Sha256::digest(&key)).as_str()) {
                return Err(bad(
                    path,
                    "is damaged (its Secure Enclave key doesn't match its checksum). Run `pai unpair`, then pair again",
                ));
            }
            let peer = b64(path, &sealed.peer, "public key")?;
            let nonce: [u8; 12] = b64(path, &sealed.nonce, "nonce")?
                .try_into()
                .map_err(|_| bad(path, "has no valid nonce"))?;
            let secret = enclave_agree(&key, &peer)?;
            aes(&secret, nonce, data, false)
                .map(Zeroizing::new)
                .map_err(|_| bad(path, "can't be opened by this Mac's Secure Enclave"))
        }
        "dpapi" => dpapi(&data, false).map(Zeroizing::new),
        other => Err(bad(
            path,
            &format!("uses `{other}`, which this pai can't open. Run `pai update`"),
        )),
    }
}

/// The software half: a fresh P-256 key pair, agreed with the enclave key's public half.
fn software_agree(key_pub: &[u8]) -> Result<([u8; 65], Zeroizing<[u8; 32]>)> {
    let rng = ring_rand::SystemRandom::new();
    let fail = |_| Error::Keystore("couldn't make a key for the secrets file".into());
    let mine =
        agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).map_err(fail)?;
    let public = mine.compute_public_key().map_err(fail)?;
    let peer: [u8; 65] = public
        .as_ref()
        .try_into()
        .map_err(|_| Error::Keystore("bad P-256 public key".into()))?;
    let theirs = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, key_pub);
    let secret = agreement::agree_ephemeral(mine, &theirs, |s| {
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(s);
        out
    })
    .map_err(|_| Error::Keystore("the Secure Enclave's public key is not a P-256 key".into()))?;
    Ok((peer, secret))
}

struct Len32;
impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn aes(secret: &[u8; 32], nonce: [u8; 12], mut data: Vec<u8>, seal: bool) -> Result<Vec<u8>> {
    let fail = || Error::Keystore("couldn't seal or open the secrets file".into());
    let mut key = Zeroizing::new([0u8; 32]);
    hkdf::Salt::new(hkdf::HKDF_SHA256, &[])
        .extract(secret)
        .expand(&[INFO], Len32)
        .and_then(|okm| okm.fill(&mut key[..]))
        .map_err(|_| fail())?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &key[..]).map_err(|_| fail())?,
    );
    let nonce = aead::Nonce::assume_unique_for_key(nonce);
    if seal {
        key.seal_in_place_append_tag(nonce, aead::Aad::from(INFO), &mut data)
            .map_err(|_| fail())?;
        Ok(data)
    } else {
        let len = key
            .open_in_place(nonce, aead::Aad::from(INFO), &mut data)
            .map_err(|_| fail())?
            .len();
        data.truncate(len);
        Ok(data)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- the Secure Enclave, through a child process ----

/// Makes a new enclave key: its handle and its public key.
fn enclave_create() -> Result<(Vec<u8>, Vec<u8>)> {
    let out = helper("create", &[])?;
    let v: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|_| Error::Keystore("bad answer from the Secure Enclave helper".into()))?;
    let get = |k: &str| v[k].as_str().and_then(|s| B64.decode(s).ok());
    match (get("key"), get("pub")) {
        (Some(k), Some(p)) if p.len() == 65 => Ok((k, p)),
        _ => Err(Error::Keystore(
            "bad answer from the Secure Enclave helper".into(),
        )),
    }
}

fn enclave_agree(key: &[u8], peer: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let input = serde_json::json!({ "key": B64.encode(key), "peer": B64.encode(peer) }).to_string();
    let out = Zeroizing::new(helper("agree", input.as_bytes())?);
    let text = std::str::from_utf8(&out)
        .map_err(|_| Error::Keystore("bad answer from the Secure Enclave helper".into()))?;
    let bytes = Zeroizing::new(
        B64.decode(text.trim())
            .map_err(|_| Error::Keystore("bad answer from the Secure Enclave helper".into()))?,
    );
    let mut secret = Zeroizing::new([0u8; 32]);
    if bytes.len() != 32 {
        return Err(Error::Keystore(
            "bad answer from the Secure Enclave helper".into(),
        ));
    }
    secret.copy_from_slice(&bytes);
    Ok(secret)
}

#[cfg(target_os = "macos")]
fn helper(op: &str, input: &[u8]) -> Result<Vec<u8>> {
    use std::time::Duration;

    let exe = std::env::current_exe()
        .map_err(|e| Error::Keystore(format!("can't find pai itself: {e}")))?;
    run_child(&exe, op, input, Duration::from_secs(HELPER_TIMEOUT_SECS))
}

/// Runs `<exe> keystore-helper <op>` and kills it if it hasn't finished within `limit`.
#[cfg(target_os = "macos")]
fn run_child(exe: &Path, op: &str, input: &[u8], limit: std::time::Duration) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(exe)
        .args(["keystore-helper", op])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Keystore(format!("can't start the Secure Enclave helper: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Keystore(format!(
                    "the Secure Enclave didn't answer within {} s. If this keeps happening, log out of macOS and back in",
                    limit.as_secs()
                )));
            }
        }
    };
    let mut out = Vec::new();
    let mut err = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_end(&mut out);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut err);
    }
    if !status.success() {
        return Err(Error::Keystore(format!(
            "the Secure Enclave refused: {}",
            err.trim()
        )));
    }
    Ok(out)
}

#[cfg(not(target_os = "macos"))]
fn helper(_op: &str, _input: &[u8]) -> Result<Vec<u8>> {
    Err(Error::Keystore(
        "this computer has no Secure Enclave".into(),
    ))
}

/// `pai keystore-helper <op>`: the child side. Reads its input from stdin and writes its answer to
/// stdout, so a crash or hang in the enclave takes down only this process.
pub fn run_helper(op: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::io::{Read, Write};
        let mut input = Zeroizing::new(Vec::new());
        std::io::stdin()
            .read_to_end(&mut input)
            .map_err(|e| Error::Keystore(e.to_string()))?;
        let answer = match op {
            "available" => if mac::available() { "yes" } else { "no" }.to_string(),
            "create" => {
                let (key, public) = mac::create()?;
                serde_json::json!({ "key": B64.encode(key), "pub": B64.encode(public) }).to_string()
            }
            "agree" => {
                let v: serde_json::Value = serde_json::from_slice(&input)
                    .map_err(|_| Error::Keystore("bad input".into()))?;
                let get = |k: &str| v[k].as_str().and_then(|s| B64.decode(s).ok());
                let (Some(key), Some(peer)) = (get("key"), get("peer")) else {
                    return Err(Error::Keystore("bad input".into()));
                };
                B64.encode(*mac::agree(&key, &peer)?)
            }
            _ => {
                return Err(Error::Keystore(format!(
                    "unknown keystore-helper command {op}"
                )));
            }
        };
        std::io::stdout()
            .write_all(answer.as_bytes())
            .map_err(|e| Error::Keystore(e.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = op;
        Err(Error::Keystore(
            "this computer has no Secure Enclave".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
mod mac {
    //! CryptoKit's Secure Enclave keys, from `src/secure_enclave.swift` (built by build.rs).
    use zeroize::Zeroizing;

    use crate::error::{Error, Result};

    unsafe extern "C" {
        fn pai_se_available() -> bool;
        fn pai_se_create(out: *mut u8, cap: usize, out_len: *mut usize, public: *mut u8) -> i32;
        fn pai_se_agree(key: *const u8, key_len: usize, peer: *const u8, out: *mut u8) -> i32;
    }

    pub fn available() -> bool {
        unsafe { pai_se_available() }
    }

    pub fn create() -> Result<(Vec<u8>, Vec<u8>)> {
        let mut key = vec![0u8; 4096];
        let mut len = 0usize;
        let mut public = vec![0u8; 65];
        let rc =
            unsafe { pai_se_create(key.as_mut_ptr(), key.len(), &mut len, public.as_mut_ptr()) };
        if rc != 0 {
            return Err(Error::Keystore(format!(
                "the Secure Enclave couldn't make a key (code {rc})"
            )));
        }
        key.truncate(len);
        Ok((key, public))
    }

    pub fn agree(key: &[u8], peer: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        if peer.len() != 65 {
            return Err(Error::Keystore("bad public key".into()));
        }
        let mut out = Zeroizing::new([0u8; 32]);
        let rc = unsafe { pai_se_agree(key.as_ptr(), key.len(), peer.as_ptr(), out.as_mut_ptr()) };
        if rc != 0 {
            return Err(Error::Keystore(format!(
                "the Secure Enclave couldn't use its key (code {rc})"
            )));
        }
        Ok(out)
    }
}

// ---- DPAPI ----

#[cfg(windows)]
fn dpapi(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    let blob_in = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: INFO.len() as u32,
        pbData: INFO.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &blob_in,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &blob_in,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 || out.pbData.is_null() {
        return Err(Error::Keystore(format!(
            "Windows couldn't {} the secrets file for this user",
            if protect { "seal" } else { "open" }
        )));
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe {
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData as _);
    }
    Ok(bytes)
}

#[cfg(all(not(windows), not(any(test, feature = "test-mode"))))]
fn dpapi(_input: &[u8], _protect: bool) -> Result<Vec<u8>> {
    Err(Error::Keystore("DPAPI is Windows only".into()))
}

/// Tests on other systems: a stand-in that keeps the bytes as they are, so the sealed file's
/// handling can be tested there. Release builds leave out `test-mode`.
#[cfg(all(not(windows), any(test, feature = "test-mode")))]
fn dpapi(input: &[u8], _protect: bool) -> Result<Vec<u8>> {
    Ok(input.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The software side of the enclave scheme, with a software key standing in for the enclave:
    /// what one side seals with the enclave key's public half, the other opens with the agreement.
    #[test]
    fn a_secret_agreed_with_the_public_half_opens_what_was_sealed() {
        let rng = ring_rand::SystemRandom::new();
        let enclave =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let enclave_pub = enclave.compute_public_key().unwrap().as_ref().to_vec();
        let (peer, secret) = software_agree(&enclave_pub).unwrap();
        let sealed = aes(&secret, [7; 12], b"{\"device-key\":\"x\"}".to_vec(), true).unwrap();
        let theirs = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, peer);
        let opened_secret = agreement::agree_ephemeral(enclave, &theirs, |s| {
            let mut o = [0u8; 32];
            o.copy_from_slice(s);
            o
        })
        .unwrap();
        assert_eq!(&opened_secret, &*secret);
        assert_eq!(
            aes(&opened_secret, [7; 12], sealed.clone(), false).unwrap(),
            b"{\"device-key\":\"x\"}"
        );
        let mut tampered = sealed;
        tampered[0] ^= 1;
        assert!(aes(&opened_secret, [7; 12], tampered, false).is_err());
    }

    #[test]
    fn a_damaged_enclave_key_is_never_handed_to_the_enclave() {
        let file = Sealed {
            v: 1,
            kind: "secure-enclave".into(),
            key: Some(B64.encode([1u8; 32])),
            key_sha256: Some(hex(&Sha256::digest([2u8; 32]))),
            key_pub: Some(B64.encode([4u8; 65])),
            peer: Some(B64.encode([4u8; 65])),
            nonce: Some(B64.encode([0u8; 12])),
            data: B64.encode([0u8; 32]),
        };
        let bytes = serde_json::to_vec(&file).unwrap();
        let err = open(Path::new("secrets.sealed"), &bytes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("damaged"), "{err}");
    }

    #[test]
    fn a_newer_format_asks_for_an_update() {
        let err = open(
            Path::new("secrets.sealed"),
            br#"{"v":2,"kind":"x","data":""}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pai update"), "{err}");
    }

    /// A helper that never answers is killed at the time limit, and its process is gone. A stub
    /// stands in for `pai keystore-helper`, so the enclave is never asked.
    #[cfg(target_os = "macos")]
    fn a_hung_helper_is_killed(limit: std::time::Duration) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pai-hung-helper-{}-{}", std::process::id(), limit.as_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let stub = dir.join("stub");
        std::fs::write(&stub, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n", pid_file.display())).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = std::time::Instant::now();
        let err = run_child(&stub, "agree", b"{}", limit).unwrap_err().to_string();
        let took = started.elapsed();
        assert!(err.contains(&format!("didn't answer within {} s", limit.as_secs())), "{err}");
        assert!(took >= limit && took < limit + std::time::Duration::from_secs(5), "{took:?}");
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = std::process::Command::new("/bin/kill").args(["-0", pid.trim()]).status().unwrap().success();
        assert!(!alive, "the helper {pid} is still running");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_helper_that_doesnt_answer_is_killed() {
        a_hung_helper_is_killed(std::time::Duration::from_secs(1));
    }

    /// The same at the real limit: `cargo test -- --ignored` (takes 15 s).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn a_helper_that_doesnt_answer_is_killed_after_15_s() {
        a_hung_helper_is_killed(std::time::Duration::from_secs(HELPER_TIMEOUT_SECS));
    }
}
