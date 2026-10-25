// `pai update` against a plain-HTTP fake of `<api>/pai/latest/`, with a test release key, which
// only a staging build accepts: run with `cargo test --features test-mode`.
#![cfg(all(feature = "test-mode", unix))]

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use base64::Engine;
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::{Digest, Sha256};

const NEW_BINARY: &[u8] = b"#!/bin/sh\nexit 0\n";

struct Release {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    port: u16,
}

impl Release {
    /// Serves each file under /pai/latest/ and counts nothing; GETs for anything else get a 404.
    fn serve() -> Release {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let shared = files.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split(' ').nth(1).unwrap_or("").to_string();
                let body = path.strip_prefix("/pai/latest/").and_then(|n| shared.lock().unwrap().get(n).cloned());
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            }
        });
        Release { files, port }
    }

    /// Publishes a release the way release/build.sh does: SHA256SUMS over every file, signed.
    fn publish(&self, key: &Ed25519KeyPair, version: &str, binary: &[u8]) {
        let asset = pai::update::asset().unwrap();
        let mut files = HashMap::from([("VERSION".to_string(), format!("{version}\n").into_bytes()), (asset.to_string(), binary.to_vec())]);
        let mut sums = String::new();
        let mut names: Vec<_> = files.keys().cloned().collect();
        names.sort();
        for n in names {
            let h: String = Sha256::digest(&files[&n]).iter().map(|b| format!("{b:02x}")).collect();
            sums.push_str(&format!("{h}  {n}\n"));
        }
        files.insert("SHA256SUMS.sig".into(), key.sign(sums.as_bytes()).as_ref().to_vec());
        files.insert("SHA256SUMS".into(), sums.into_bytes());
        *self.files.lock().unwrap() = files;
    }

    fn set(&self, name: &str, body: &[u8]) {
        self.files.lock().unwrap().insert(name.into(), body.to_vec());
    }
}

fn keypair() -> Ed25519KeyPair {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
}

fn pem(key: &Ed25519KeyPair) -> String {
    let mut der = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    der.extend_from_slice(key.public_key().as_ref());
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
}

struct Install {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

/// A copy of pai in its own folder, since an update replaces the binary that runs it.
fn install(trusted: &Ed25519KeyPair) -> Install {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin/pai");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_pai"), &bin).unwrap();
    fs::write(dir.path().join("release-key.pem"), pem(trusted)).unwrap();
    Install { dir, bin }
}

fn update(i: &Install, release: &Release) -> Output {
    Command::new(&i.bin)
        .arg("update")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", i.dir.path().join("home"))
        .env("PAI_CONFIG_DIR", i.dir.path().join("config"))
        .env("PAI_KEYSTORE", "file")
        .env("PAI_API_URL", format!("http://127.0.0.1:{}", release.port))
        .env("PAI_TEST_RELEASE_KEY", i.dir.path().join("release-key.pem"))
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn unchanged(bin: &Path) -> bool {
    fs::read(bin).unwrap() == fs::read(env!("CARGO_BIN_EXE_pai")).unwrap()
}

#[test]
fn installs_a_newer_signed_release() {
    let key = keypair();
    let release = Release::serve();
    let i = install(&key);
    release.publish(&key, "99.0.0", NEW_BINARY);
    let out = update(&i, &release);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains(&format!("Updated pai from {} to 99.0.0", env!("CARGO_PKG_VERSION"))), "{}", text(&out.stdout));
    assert_eq!(fs::read(&i.bin).unwrap(), NEW_BINARY);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&i.bin).unwrap().permissions().mode() & 0o777, 0o755);
}

#[test]
fn keeps_the_same_or_older_release() {
    let key = keypair();
    let release = Release::serve();
    let i = install(&key);
    for v in [env!("CARGO_PKG_VERSION"), "0.0.1"] {
        release.publish(&key, v, NEW_BINARY);
        let out = update(&i, &release);
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(text(&out.stdout).contains("is the latest release"), "{}", text(&out.stdout));
        assert!(unchanged(&i.bin));
    }
}

#[test]
fn refuses_a_release_signed_by_another_key() {
    let release = Release::serve();
    let i = install(&keypair());
    release.publish(&keypair(), "99.0.0", NEW_BINARY);
    let out = update(&i, &release);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("isn't signed with pai's release key"), "{}", text(&out.stderr));
    assert!(unchanged(&i.bin));
}

#[test]
fn refuses_a_binary_that_isnt_the_signed_one() {
    let key = keypair();
    let release = Release::serve();
    let i = install(&key);
    release.publish(&key, "99.0.0", NEW_BINARY);
    release.set(pai::update::asset().unwrap(), b"#!/bin/sh\necho swapped\n");
    let out = update(&i, &release);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("doesn't match the signed release"), "{}", text(&out.stderr));
    assert!(unchanged(&i.bin));
    let left: Vec<_> = fs::read_dir(i.bin.parent().unwrap()).unwrap().collect();
    assert_eq!(left.len(), 1, "no temporary file left behind");
}

#[test]
fn refuses_a_version_file_that_isnt_the_signed_one() {
    let key = keypair();
    let release = Release::serve();
    let i = install(&key);
    release.publish(&key, "99.0.0", NEW_BINARY);
    release.set("VERSION", b"100.0.0\n");
    let out = update(&i, &release);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("VERSION doesn't match"), "{}", text(&out.stderr));
    assert!(unchanged(&i.bin));
}

#[test]
fn a_running_proxy_moves_to_the_new_binary_in_the_same_process() {
    let i = install(&keypair());
    let config_dir = i.dir.path().join("config");
    pai::config::Config {
        relay_url: "http://127.0.0.1:9".into(),
        api_url: "http://127.0.0.1:9".into(),
        app_url: None,
        device_id: "dev_1".into(),
        account_id: "acct_1".into(),
        test: None,
    }
    .save(&config_dir.join("config.json"))
    .unwrap();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut proxy = Command::new(&i.bin)
        .arg("proxy")
        .env_clear()
        .env("HOME", i.dir.path().join("home"))
        .env("PAI_CONFIG_DIR", &config_dir)
        .env("PAI_KEYSTORE", "file")
        .env("PAI_PREFETCH", "0")
        .env("PAI_PROXY_PORT", port.to_string())
        .env("PAI_TEST_UPDATE_POLL", "1")
        .spawn()
        .unwrap();
    let keystore = pai::keystore::Keystore::File(config_dir.join("secrets.json"));
    let health = || -> Option<String> {
        let key = keystore.get(pai::keystore::PROXY_KEY).ok()??;
        let r = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/pai/health"))
            .bearer_auth(key.to_string())
            .send()
            .ok()?;
        r.json::<serde_json::Value>().ok()?["pai"].as_str().map(str::to_string)
    };
    let wait = |f: &dyn Fn(Option<String>) -> bool| {
        for _ in 0..100 {
            if f(health()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    };
    assert!(wait(&|h| h.is_some()), "the proxy started");
    let before = health().unwrap();

    // What an update does: a new file renamed over the old one.
    let tmp = i.bin.with_file_name(".pai.new");
    fs::write(&tmp, fs::read(&i.bin).unwrap()).unwrap();
    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    fs::rename(&tmp, &i.bin).unwrap();

    let moved = wait(&|h| h.is_some_and(|h| h != before));
    let still_running = proxy.try_wait().unwrap().is_none();
    let _ = proxy.kill();
    let _ = proxy.wait();
    assert!(moved, "the proxy restarted on the new binary");
    assert!(still_running, "and kept its process, so a login service never sees it exit");
}
