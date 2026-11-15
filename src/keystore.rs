use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zeroize::{Zeroize, Zeroizing};

use crate::error::{Error, Result, io_err};
use crate::sealed;

pub const SERVICE: &str = "dawonai";
pub const DEVICE_KEY: &str = "device-key";
pub const CREDENTIAL: &str = "day-credential";
/// Tomorrow's credential, collected ahead at a random time.
pub const NEXT_CREDENTIAL: &str = "next-day-credential";
/// The key coding tools send to the local proxy. It only works on this machine.
pub const PROXY_KEY: &str = "local-proxy-key";
/// The account's pass key (base64url of 32 bytes), from the web app's pairing command. Every device
/// that holds it derives the same day pass, so a plan's streams are counted per account.
pub const PASS_KEY: &str = "pass-key";

/// Holds the device key and the cached day credentials.
#[derive(Debug, Clone)]
pub enum Keystore {
    /// The OS store (the macOS login keychain, Windows Credential Manager or the Secret Service on
    /// Linux), where pai before 0.1.6 kept its secrets. Read only to move them into the file: the
    /// macOS keychain tied each item to the binary that saved it, and pai's ad-hoc signature changes
    /// with every update, so each update brought a permission prompt, or a failure over SSH.
    Os,
    /// `secrets.json` next to `config.json`, created with mode 0600 on Unix. On Windows it relies
    /// on the user profile's own permissions, like other tools' token files.
    File(PathBuf),
    /// `secrets.sealed` beside the given `secrets.json` path, sealed by the Secure Enclave (macOS)
    /// or DPAPI (Windows): it opens only on this computer, for this user, with no prompt
    /// (src/sealed.rs). A `secrets.json` left by an older pai is sealed the first time it's read,
    /// and removed only by `drop_plain_copy`, once no proxy started by that older pai can still be
    /// reading it.
    Sealed(PathBuf, crate::sealed::Kind),
}

impl Keystore {
    pub fn describe(&self) -> String {
        match self {
            Keystore::Os => "OS keychain".into(),
            Keystore::File(p) => format!("file {}", p.display()),
            Keystore::Sealed(p, kind) => format!("file {}, {}", sealed::sealed_path(p).display(), kind.describe()),
        }
    }

    /// Fails early with a readable message when the OS keychain can't be reached, so `pai pair`
    /// never registers a device whose key it then can't keep.
    pub fn check(&self) -> Result<()> {
        match self {
            Keystore::Os => {
                if let Err(e) = keyring::Entry::store_status() {
                    return Err(Error::KeychainUnavailable(e.to_string()));
                }
                macos_keychain_ready()
            }
            Keystore::File(_) | Keystore::Sealed(..) => Ok(()),
        }
    }

    pub fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>> {
        match self {
            Keystore::Os => {
                let entry = os_entry(name)?;
                match entry.get_password() {
                    Ok(v) => Ok(Some(Zeroizing::new(v))),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(e) => Err(os_error(e)),
                }
            }
            Keystore::File(path) => Ok(read_file(path)?.remove(name).map(Zeroizing::new)),
            Keystore::Sealed(path, kind) => Ok(read_sealed(path, *kind)?.remove(name).map(Zeroizing::new)),
        }
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        match self {
            Keystore::Os => {
                macos_keychain_ready()?;
                os_entry(name)?.set_password(value).map_err(os_error)
            }
            Keystore::File(path) => {
                let mut map = read_file(path)?;
                map.insert(name.to_string(), value.to_string());
                write_file(path, &map)
            }
            Keystore::Sealed(path, kind) => {
                let mut map = read_sealed(path, *kind)?;
                map.insert(name.to_string(), value.to_string());
                write_sealed(path, *kind, &map)
            }
        }
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        match self {
            Keystore::Os => match os_entry(name)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(os_error(e)),
            },
            Keystore::File(path) => {
                let mut map = read_file(path)?;
                if map.remove(name).is_some() {
                    if map.is_empty() {
                        fs::remove_file(path).map_err(|e| io_err("remove", path, e))?;
                    } else {
                        write_file(path, &map)?;
                    }
                }
                Ok(())
            }
            Keystore::Sealed(path, kind) => {
                // The plain copy an older pai left goes too, or the next read would seal it again.
                if path.exists() {
                    Keystore::File(path.clone()).delete(name)?;
                }
                let mut map = read_sealed(path, *kind)?;
                if map.remove(name).is_some() {
                    if map.is_empty() {
                        let sealed = sealed::sealed_path(path);
                        fs::remove_file(&sealed).map_err(|e| io_err("remove", &sealed, e))?;
                    } else {
                        write_sealed(path, *kind, &map)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Seals what a plain `secrets.json` beside the sealed file still holds, then removes it. A proxy
    /// started by pai before 0.1.22 reads only that plain file, and keeps running after an update
    /// until it's replaced, so the file stays until then: removing it at once made that proxy answer
    /// "can't find its device key" (privacy-ai docs/INCIDENTS.md, 2026-11-10). Values in the sealed
    /// file win; the plain file adds only what the sealed one lacks. Returns whether a file was
    /// removed.
    pub fn drop_plain_copy(&self) -> Result<bool> {
        let Keystore::Sealed(json, kind) = self else { return Ok(false) };
        if !json.exists() {
            return Ok(false);
        }
        let plain = read_file(json)?;
        let path = sealed::sealed_path(json);
        let mut map = if path.exists() { read_sealed_only(&path)? } else { SecretMap::default() };
        let before = map.len();
        for (name, value) in plain.iter() {
            map.entry(name.clone()).or_insert_with(|| value.clone());
        }
        if map.len() != before {
            write_sealed(json, *kind, &map)?;
        }
        // Removed only once the sealed copy is known to open and to hold everything.
        if !map.is_empty() && read_sealed_only(&path)?.0 != map.0 {
            return Err(Error::Keystore(format!("{} didn't keep what pai sealed", path.display())));
        }
        fs::remove_file(json).map_err(|e| io_err("remove", json, e))?;
        Ok(true)
    }
}

/// The sealed file's secrets. Without one, the plain `secrets.json` an older pai left (or none),
/// sealed here and left in place for that pai's proxy (`Keystore::drop_plain_copy`).
fn read_sealed(json: &Path, kind: sealed::Kind) -> Result<SecretMap> {
    let path = sealed::sealed_path(json);
    match fs::read(&path) {
        Ok(bytes) => {
            let plain = sealed::open(&path, &bytes)?;
            serde_json::from_slice(&plain)
                .map(SecretMap)
                .map_err(|_| Error::Keystore(format!("{} holds no valid secrets", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let map = read_file(json)?;
            if !map.is_empty() {
                write_sealed(json, kind, &map)?;
            }
            Ok(map)
        }
        Err(e) => Err(io_err("read", &path, e)),
    }
}

fn read_sealed_only(path: &Path) -> Result<SecretMap> {
    let bytes = fs::read(path).map_err(|e| io_err("read", path, e))?;
    let plain = sealed::open(path, &bytes)?;
    serde_json::from_slice(&plain)
        .map(SecretMap)
        .map_err(|_| Error::Keystore(format!("{} holds no valid secrets", path.display())))
}

fn write_sealed(json: &Path, kind: sealed::Kind, map: &SecretMap) -> Result<()> {
    let path = sealed::sealed_path(json);
    let plain = Zeroizing::new(serde_json::to_vec(&map.0).expect("secrets serialize"));
    let bytes = sealed::seal(&path, kind, &plain)?;
    write_bytes(&path, &bytes)
}

/// Asks the default keychain for an item that doesn't exist, with keychain dialogs turned off.
/// When macOS can't reach the keychain (pai run inside a sandbox, or over SSH), saving to it would
/// show "Keychain Not Found" with a Reset To Defaults button, which replaces the user's login
/// keychain. With dialogs off, macOS returns an error here instead, and pai stops before saving.
/// A locked keychain is refused too: searching one works, so the probe alone passes, but writing
/// to it asks for its password, which can't happen over SSH, and `pai pair` would find that out
/// only after spending the pairing code.
#[cfg(target_os = "macos")]
fn macos_keychain_ready() -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    const ITEM_NOT_FOUND: i32 = -25300;
    let unreachable = |detail: String| Error::KeychainUnavailable(detail.trim_end_matches('.').to_string());
    let _no_dialogs = SecKeychain::disable_user_interaction().map_err(|e| unreachable(e.to_string()))?;
    if !mac::default_keychain_unlocked()? {
        return Err(Error::KeychainLocked);
    }
    let keychain = SecKeychain::default().map_err(|e| unreachable(e.to_string()))?;
    match keychain.find_generic_password("dawonai-keychain-check", "check") {
        Ok(_) => Ok(()),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
        Err(e) => Err(unreachable(e.to_string())),
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_keychain_ready() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::ptr;

    use crate::error::{Error, Result};

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainCopyDefault(keychain: *mut *mut c_void) -> i32;
        fn SecKeychainGetStatus(keychain: *mut c_void, status: *mut u32) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// kSecUnlockStateStatus
    const UNLOCKED: u32 = 1;

    pub fn default_keychain_unlocked() -> Result<bool> {
        let mut keychain: *mut c_void = ptr::null_mut();
        let status = unsafe { SecKeychainCopyDefault(&mut keychain) };
        if status != 0 || keychain.is_null() {
            return Err(Error::KeychainUnavailable(describe(status)));
        }
        let unlocked = unlocked(keychain);
        unsafe { CFRelease(keychain) };
        unlocked
    }

    pub fn unlocked(keychain: *mut c_void) -> Result<bool> {
        let mut state = 0u32;
        let status = unsafe { SecKeychainGetStatus(keychain, &mut state) };
        if status != 0 {
            return Err(Error::KeychainUnavailable(describe(status)));
        }
        Ok(state & UNLOCKED != 0)
    }

    fn describe(code: i32) -> String {
        security_framework::base::Error::from_code(code)
            .to_string()
            .trim_end_matches('.')
            .to_string()
    }
}

fn os_entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, name).map_err(|e| match e {
        keyring::Error::NoDefaultStore => match keyring::Entry::store_status() {
            Err(inner) => Error::KeychainUnavailable(inner.to_string()),
            Ok(()) => Error::KeychainUnavailable(e.to_string()),
        },
        other => os_error(other),
    })
}

fn os_error(e: keyring::Error) -> Error {
    #[cfg(target_os = "macos")]
    if matches!(e, keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_))
        && matches!(mac::default_keychain_unlocked(), Ok(false))
    {
        return Error::KeychainLocked;
    }
    match e {
        keyring::Error::NoStorageAccess(_)
        | keyring::Error::PlatformFailure(_)
        | keyring::Error::NoDefaultStore => Error::KeychainUnavailable(e.to_string()),
        other => Error::Keystore(other.to_string()),
    }
}

#[derive(Default)]
struct SecretMap(BTreeMap<String, String>);

impl Drop for SecretMap {
    fn drop(&mut self) {
        self.0.values_mut().for_each(Zeroize::zeroize);
    }
}

impl std::ops::Deref for SecretMap {
    type Target = BTreeMap<String, String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SecretMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn read_file(path: &Path) -> Result<SecretMap> {
    let text = match fs::read_to_string(path) {
        Ok(t) => Zeroizing::new(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SecretMap::default()),
        Err(e) => return Err(io_err("read", path, e)),
    };
    serde_json::from_str(&text)
        .map(SecretMap)
        .map_err(|_| Error::Keystore(format!("{} is not valid JSON", path.display())))
}

fn write_file(path: &Path, map: &SecretMap) -> Result<()> {
    let text = Zeroizing::new(serde_json::to_string_pretty(&map.0).expect("secrets serialize"));
    write_bytes(path, text.as_bytes())
}

/// Writes through a 0600 temporary file and a rename, so a crash never leaves half a file.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Io(format!("bad secrets path {}", path.display())))?;
    fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
    // A name of its own for each write: with one shared name, two writers at once (the new pai
    // sealing an older pai's file while its proxy reads) removed each other's file.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Created 0600 rather than chmod-ed afterwards, so the secrets are never readable by others.
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| io_err("create", &tmp, e))?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|_| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err("write", path, e));
    }
    Ok(())
}

/// The sealed file beside a plain one an older pai left. `Kind::Dpapi` stands in for both kinds:
/// off Windows, tests seal with a stand-in that keeps the bytes (src/sealed.rs).
#[cfg(test)]
mod plain_copy_tests {
    use super::*;

    fn stores() -> (tempfile::TempDir, PathBuf, Keystore, Keystore) {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("secrets.json");
        let old = Keystore::File(json.clone());
        old.set(DEVICE_KEY, "device").unwrap();
        old.set(CREDENTIAL, "today").unwrap();
        let new = Keystore::Sealed(json.clone(), sealed::Kind::Dpapi);
        (dir, json, old, new)
    }

    #[test]
    fn a_proxy_from_an_older_pai_still_finds_its_secrets_after_the_new_pai_seals_them() {
        let (_dir, json, old, new) = stores();
        assert_eq!(new.get(DEVICE_KEY).unwrap().as_deref().map(|s| s.as_str()), Some("device"));
        assert!(sealed::sealed_path(&json).exists());
        // The old proxy reads only the plain file, as pai before 0.1.22 does.
        assert_eq!(old.get(DEVICE_KEY).unwrap().as_deref().map(|s| s.as_str()), Some("device"));
        assert_eq!(new.get(DEVICE_KEY).unwrap().as_deref().map(|s| s.as_str()), Some("device"));
    }

    #[test]
    fn dropping_the_plain_copy_keeps_what_the_older_proxy_added() {
        let (_dir, json, old, new) = stores();
        new.get(DEVICE_KEY).unwrap();
        new.set(CREDENTIAL, "sealed today").unwrap();
        old.set(CREDENTIAL, "plain today").unwrap();
        old.set(NEXT_CREDENTIAL, "tomorrow").unwrap();
        assert!(new.drop_plain_copy().unwrap());
        assert!(!json.exists());
        assert_eq!(new.get(CREDENTIAL).unwrap().as_deref().map(|s| s.as_str()), Some("sealed today"));
        assert_eq!(new.get(NEXT_CREDENTIAL).unwrap().as_deref().map(|s| s.as_str()), Some("tomorrow"));
        assert_eq!(new.get(DEVICE_KEY).unwrap().as_deref().map(|s| s.as_str()), Some("device"));
        assert!(!new.drop_plain_copy().unwrap());
    }

    #[test]
    fn dropping_the_plain_copy_seals_it_when_nothing_was_sealed_yet() {
        let (_dir, json, _old, new) = stores();
        assert!(new.drop_plain_copy().unwrap());
        assert!(!json.exists());
        assert_eq!(new.get(DEVICE_KEY).unwrap().as_deref().map(|s| s.as_str()), Some("device"));
    }

    #[test]
    fn a_deleted_secret_doesnt_come_back_from_the_plain_copy() {
        let (_dir, json, old, new) = stores();
        new.get(DEVICE_KEY).unwrap();
        for name in [DEVICE_KEY, CREDENTIAL] {
            new.delete(name).unwrap();
        }
        assert!(!sealed::sealed_path(&json).exists());
        assert_eq!(old.get(DEVICE_KEY).unwrap(), None);
        assert_eq!(new.get(DEVICE_KEY).unwrap(), None);
    }

    /// The new pai and readers that run at the same time never see the device key missing.
    #[test]
    fn readers_never_see_the_device_key_missing_while_it_is_sealed() {
        for _ in 0..50 {
            let (_dir, _json, old, new) = stores();
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let store = if i % 2 == 0 { new.clone() } else { old.clone() };
                    std::thread::spawn(move || store.get(DEVICE_KEY).unwrap().is_some())
                })
                .collect();
            assert!(handles.into_iter().all(|h| h.join().unwrap()));
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use std::ffi::{CString, c_char, c_void};
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::process::Command;
    use std::ptr;

    use super::mac;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainOpen(path: *const c_char, keychain: *mut *mut c_void) -> i32;
    }

    fn security(args: &[&str]) {
        let status = Command::new("/usr/bin/security").args(args).status().unwrap();
        assert!(status.success(), "security {args:?} failed");
    }

    /// A keychain of its own, so the user's login keychain is never locked by a test.
    #[test]
    fn a_locked_keychain_is_detected() {
        let path = std::env::temp_dir().join(format!("pai-test-{}.keychain-db", std::process::id()));
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);
        security(&["create-keychain", "-p", "pw", path_str]);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let c_path = CString::new(path_str).unwrap();
            let mut keychain: *mut c_void = ptr::null_mut();
            assert_eq!(unsafe { SecKeychainOpen(c_path.as_ptr(), &mut keychain) }, 0);
            assert!(mac::unlocked(keychain).unwrap(), "a new keychain starts unlocked");
            security(&["lock-keychain", path_str]);
            assert!(!mac::unlocked(keychain).unwrap());
            security(&["unlock-keychain", "-p", "pw", path_str]);
            assert!(mac::unlocked(keychain).unwrap());
        }));
        security(&["delete-keychain", path_str]);
        if let Err(e) = outcome {
            resume_unwind(e);
        }
    }
}
