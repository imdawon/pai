use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zeroize::{Zeroize, Zeroizing};

use crate::error::{Error, Result, io_err};

pub const SERVICE: &str = "dawonai";
pub const DEVICE_KEY: &str = "device-key";
pub const CREDENTIAL: &str = "day-credential";
/// Tomorrow's credential, collected ahead at a random time.
pub const NEXT_CREDENTIAL: &str = "next-day-credential";
/// The key coding tools send to the local proxy. It only works on this machine.
pub const PROXY_KEY: &str = "local-proxy-key";

/// Holds the device key and the cached day credential.
#[derive(Debug, Clone)]
pub enum Keystore {
    /// macOS Keychain, Windows Credential Manager, or the Secret Service on Linux.
    Os,
    /// `PAI_KEYSTORE=file`: a 0600 JSON file, for headless machines without a Secret Service.
    File(PathBuf),
}

impl Keystore {
    pub fn describe(&self) -> String {
        match self {
            Keystore::Os => "OS keychain".into(),
            Keystore::File(p) => format!("file {} (PAI_KEYSTORE=file)", p.display()),
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
                macos_keychain_reachable()
            }
            Keystore::File(_) => Ok(()),
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
        }
    }

    pub fn set(&self, name: &str, value: &str) -> Result<()> {
        match self {
            Keystore::Os => {
                macos_keychain_reachable()?;
                os_entry(name)?.set_password(value).map_err(os_error)
            }
            Keystore::File(path) => {
                let mut map = read_file(path)?;
                map.insert(name.to_string(), value.to_string());
                write_file(path, &map)
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
        }
    }
}

/// Asks the default keychain for an item that doesn't exist, with keychain dialogs turned off.
/// When macOS can't reach the keychain (pai run inside a sandbox, or over SSH), saving to it would
/// show "Keychain Not Found" with a Reset To Defaults button, which replaces the user's login
/// keychain. With dialogs off, macOS returns an error here instead, and pai stops before saving.
#[cfg(target_os = "macos")]
fn macos_keychain_reachable() -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    const ITEM_NOT_FOUND: i32 = -25300;
    let unreachable = |detail: String| Error::KeychainUnavailable(detail.trim_end_matches('.').to_string());
    let _no_dialogs = SecKeychain::disable_user_interaction().map_err(|e| unreachable(e.to_string()))?;
    let keychain = SecKeychain::default().map_err(|e| unreachable(e.to_string()))?;
    match keychain.find_generic_password("dawonai-keychain-check", "check") {
        Ok(_) => Ok(()),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
        Err(e) => Err(unreachable(e.to_string())),
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_keychain_reachable() -> Result<()> {
    Ok(())
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
    let dir = path
        .parent()
        .ok_or_else(|| Error::Io(format!("bad secrets path {}", path.display())))?;
    fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
    if !path.exists() {
        eprintln!(
            "warning: PAI_KEYSTORE=file: the device key is stored unencrypted in {} (mode 0600)",
            path.display()
        );
    }
    let text = Zeroizing::new(serde_json::to_string_pretty(&map.0).expect("secrets serialize"));
    let tmp = path.with_extension("json.tmp");
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Created 0600 rather than chmod-ed afterwards, so the secrets are never readable by others.
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| io_err("create", &tmp, e))?;
    f.write_all(text.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| io_err("write", &tmp, e))?;
    drop(f);
    fs::rename(&tmp, path).map_err(|e| io_err("write", path, e))
}
