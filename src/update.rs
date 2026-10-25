//! Updates `pai` in place from `<api>/pai/latest/`, over the same pinned TLS as model requests,
//! so an update check reaches only the box, through the relay, like any request.
//!
//! A release is trusted only if `SHA256SUMS` carries a valid signature from the offline release
//! key built into `pai` (release/SIGNING.md), and each file matches its hash there. The server is
//! never trusted for anything else, and a release older than this one is refused.
//!
//! `pai` isn't always running, so when to check lives on disk (`update.json`): any `pai token`
//! that finds a check due starts `pai update --auto` in the background and returns at once.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::{self, Env};
use crate::error::{Error, Result, io_err};

const RELEASE_KEY_PEM: &str = include_str!("../release/signing-key.pub");
const CHECK_EVERY: i64 = 24 * 3600;
/// Spread out so every installed `pai` doesn't check at the same moment.
const JITTER: i64 = 6 * 3600;
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const MAX_TEXT: u64 = 64 * 1024;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The release file for the platform this `pai` was built for.
pub fn asset() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "pai-aarch64-apple-darwin",
        ("macos", "x86_64") => "pai-x86_64-apple-darwin",
        ("linux", "aarch64") => "pai-aarch64-unknown-linux-musl",
        ("linux", "x86_64") => "pai-x86_64-unknown-linux-musl",
        ("windows", "aarch64") => "pai-aarch64-pc-windows-msvc.exe",
        ("windows", "x86_64") => "pai-x86_64-pc-windows-msvc.exe",
        _ => return None,
    })
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Unix time of the next background check.
    #[serde(default)]
    next_check: i64,
}

fn state_file(env: &Env) -> PathBuf {
    env.paths.config_dir.join("update.json")
}

fn load_state(env: &Env) -> State {
    fs::read(state_file(env)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_state(env: &Env, s: &State) -> Result<()> {
    let path = state_file(env);
    fs::create_dir_all(&env.paths.config_dir).map_err(|e| io_err("create", &env.paths.config_dir, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(s).expect("state serializes")).map_err(|e| io_err("write", &tmp, e))?;
    fs::rename(&tmp, &path).map_err(|e| io_err("write", &path, e))
}

/// Background updates are on in release builds unless `PAI_AUTO_UPDATE=0`. A build from source
/// has no release domain compiled in, so it never replaces itself unasked.
pub fn auto_enabled(env: &Env) -> bool {
    option_env!("PAI_RELEASE_API_URL").is_some() && env.var("PAI_AUTO_UPDATE") != Some("0")
}

/// Starts `pai update --auto` in the background when a check is due. Never fails the caller.
pub fn start_if_due(env: &Env) {
    if !auto_enabled(env) || asset().is_none() {
        return;
    }
    let now = env.clock.now();
    if load_state(env).next_check > now {
        return;
    }
    // Written before starting, so tools that run `pai token` together start one check between them,
    // and a failing check waits a day like any other.
    let next = now + CHECK_EVERY + rand::random::<u32>() as i64 % JITTER;
    if save_state(env, &State { next_check: next }).is_err() {
        return;
    }
    let _ = app::spawn_detached(env, &["update", "--auto"]);
}

pub enum Outcome {
    UpToDate,
    Updated { from: String, to: String },
}

/// Checks for a newer release and installs it over this binary.
pub fn run(env: &Env) -> Result<Outcome> {
    let asset = asset().ok_or_else(|| Error::Update("there's no release of pai for this platform".into()))?;
    let base = format!("{}/pai/latest", app::api_url(env));
    let http = client(env)?;
    let key = release_key(env)?;

    // Unchecked, but a server that lies here can only make pai skip an update, which it could do by
    // not answering at all.
    let announced = get(&http, &format!("{base}/VERSION"), MAX_TEXT)?;
    let announced_version = parse_version(&announced)?;
    if !newer(&announced_version, VERSION) {
        return Ok(Outcome::UpToDate);
    }

    let sums = get(&http, &format!("{base}/SHA256SUMS"), MAX_TEXT)?;
    let sig = get(&http, &format!("{base}/SHA256SUMS.sig"), MAX_TEXT)?;
    verify_signature(&key, &sums, &sig)?;
    let hashes = parse_sums(&sums)?;
    check_hash(&hashes, "VERSION", &announced)?;

    let bin = get(&http, &format!("{base}/{asset}"), MAX_BINARY)?;
    check_hash(&hashes, asset, &bin)?;
    install(&env.pai_bin, &bin)?;
    Ok(Outcome::Updated { from: VERSION.into(), to: announced_version })
}

fn client(env: &Env) -> Result<reqwest::blocking::Client> {
    let config = env.config()?;
    let trust = app::trust(env, config.as_ref());
    let builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("pai/", env!("CARGO_PKG_VERSION")));
    let builder = if app::api_url(env).starts_with("https://") {
        builder.use_preconfigured_tls(crate::pin::client_config(&trust.api_pins, trust.test_ca.as_deref())?)
    } else {
        builder.no_proxy()
    };
    builder.build().map_err(|e| Error::Update(format!("couldn't set up the connection: {e}")))
}

fn get(http: &reqwest::blocking::Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let resp = http.get(url).send().map_err(|e| Error::Update(format!("couldn't reach {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Update(format!("{url} answered {}", resp.status())));
    }
    let mut body = Vec::new();
    resp.take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|e| Error::Update(format!("couldn't download {url}: {e}")))?;
    if body.len() as u64 > limit {
        return Err(Error::Update(format!("{url} is larger than any pai release file")));
    }
    Ok(body)
}

/// The release key's raw 32 bytes, from the PEM built into `pai`. Staging builds can swap it for a
/// test key with `PAI_TEST_RELEASE_KEY` (the same PEM).
fn release_key(env: &Env) -> Result<[u8; 32]> {
    #[cfg(feature = "test-mode")]
    if let Some(path) = env.var("PAI_TEST_RELEASE_KEY") {
        let pem = fs::read_to_string(path).map_err(|e| io_err("read", Path::new(path), e))?;
        return parse_key(&pem);
    }
    let _ = env;
    parse_key(RELEASE_KEY_PEM)
}

fn parse_key(pem: &str) -> Result<[u8; 32]> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = B64.decode(b64.trim()).map_err(|_| Error::Update("the release key isn't valid base64".into()))?;
    // An Ed25519 SubjectPublicKeyInfo is a fixed 12-byte header and the 32-byte key.
    const PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    if der.len() != 44 || der[..12] != PREFIX {
        return Err(Error::Update("the release key isn't an Ed25519 public key".into()));
    }
    Ok(der[12..].try_into().expect("32 bytes"))
}

fn verify_signature(key: &[u8; 32], sums: &[u8], sig: &[u8]) -> Result<()> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(sums, sig)
        .map_err(|_| Error::Update("the release isn't signed with pai's release key, so pai didn't install it".into()))
}

/// `shasum -a 256` lines: hex hash, two spaces (or space and `*`), file name.
fn parse_sums(sums: &[u8]) -> Result<Vec<(String, String)>> {
    let text = std::str::from_utf8(sums).map_err(|_| Error::Update("SHA256SUMS isn't text".into()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (hash, name) = l.split_once(' ').ok_or_else(|| Error::Update("SHA256SUMS has a malformed line".into()))?;
            let name = name.trim_start_matches([' ', '*']);
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Update("SHA256SUMS has a malformed line".into()));
            }
            Ok((hash.to_ascii_lowercase(), name.to_string()))
        })
        .collect()
}

fn check_hash(hashes: &[(String, String)], name: &str, data: &[u8]) -> Result<()> {
    let want = hashes
        .iter()
        .find(|(_, n)| n == name)
        .map(|(h, _)| h)
        .ok_or_else(|| Error::Update(format!("the signed release doesn't list {name}")))?;
    let have: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
    if &have != want {
        return Err(Error::Update(format!("{name} doesn't match the signed release, so pai didn't install it")));
    }
    Ok(())
}

fn parse_version(raw: &[u8]) -> Result<String> {
    let v = std::str::from_utf8(raw).unwrap_or("").trim();
    if version_parts(v).is_none() {
        return Err(Error::Update("the release's VERSION isn't a version number".into()));
    }
    Ok(v.to_string())
}

fn version_parts(v: &str) -> Option<[u64; 3]> {
    let mut it = v.split('.').map(|p| p.parse::<u64>().ok());
    let parts = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(parts)
}

fn newer(candidate: &str, current: &str) -> bool {
    match (version_parts(candidate), version_parts(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Writes the new binary next to the old one and renames it into place, so the path never holds
/// a half-written file. A running `pai` keeps the old file open and is unaffected. Windows can't
/// replace a running program but can rename it, so the old one is moved aside first.
fn install(path: &Path, bin: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| Error::Update(format!("can't tell which folder {} is in", path.display())))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "pai".into());
    let tmp = dir.join(format!(".{name}.update-{}", std::process::id()));
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bin)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Update(format!(
            "couldn't write to {} ({e}). Reinstall pai somewhere you can write to, or run `pai update` with permission to change it",
            dir.display()
        )));
    }
    #[cfg(windows)]
    {
        let old = dir.join(format!("{name}.old"));
        let _ = fs::remove_file(&old);
        if let Err(e) = fs::rename(path, &old) {
            let _ = fs::remove_file(&tmp);
            return Err(io_err("move aside", path, e));
        }
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err("replace", path, e)
    })
}

/// Windows leaves the previous binary as `pai.exe.old` until the next run can delete it.
pub fn clean_up(env: &Env) {
    #[cfg(windows)]
    if let (Some(dir), Some(name)) = (env.pai_bin.parent(), env.pai_bin.file_name()) {
        let _ = fs::remove_file(dir.join(format!("{}.old", name.to_string_lossy())));
    }
    let _ = env;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(!newer("0.1.3", "0.1.3"));
        assert!(!newer("0.1.2", "0.1.3"));
        assert!(!newer("0.1", "0.0.1"));
        assert!(!newer("0.1.4-rc1", "0.1.3"));
    }

    #[test]
    fn reads_the_built_in_release_key() {
        parse_key(RELEASE_KEY_PEM).unwrap();
    }

    #[test]
    fn reads_shasum_output() {
        let h = "a".repeat(64);
        let sums = format!("{h}  VERSION\n{h} *pai-x86_64-apple-darwin\n");
        let parsed = parse_sums(sums.as_bytes()).unwrap();
        assert_eq!(parsed[1].1, "pai-x86_64-apple-darwin");
        assert!(parse_sums(b"zz  VERSION\n").is_err());
    }

    #[test]
    fn installs_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pai");
        fs::write(&path, b"old").unwrap();
        install(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file left behind");
    }
}
