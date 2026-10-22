mod common;

use std::process::{Command, Output};

use common::FakeRelay;

struct Cli {
    dir: tempfile::TempDir,
    port: u16,
}

impl Cli {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pai"))
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path().join("home"))
            .env("PAI_CONFIG_DIR", self.dir.path().join("config"))
            .env("PAI_KEYSTORE", "file")
            .env("PAI_PREFETCH", "0")
            .env("PAI_PROXY_PORT", self.port.to_string())
            .output()
            .unwrap()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn binary_end_to_end() {
    let relay = FakeRelay::start("WXYZ6789");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cli = Cli {
        dir: tempfile::tempdir().unwrap(),
        port,
    };
    let health = |key: &str| {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/pai/health"))
            .bearer_auth(key)
            .send()
            .is_ok_and(|r| r.status() == 200)
    };

    let help = text(&cli.run(&["pair", "--help"]).stdout);
    assert!(help.contains("--relay") && !help.contains("test"), "test mode stays out of --help: {help}");
    let version = text(&cli.run(&["--version"]).stdout);
    #[cfg(not(feature = "test-mode"))]
    {
        assert!(!version.contains("staging"), "{version}");
        let out = cli.run(&["pair", "wxyz-6789", "--test-ca", "ca.pem", "--test-pin", "AAAA"]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out.stderr).contains("unexpected argument '--test-ca'"), "{}", text(&out.stderr));
    }
    #[cfg(feature = "test-mode")]
    assert!(version.contains("staging build"), "{version}");

    let out = cli.run(&["token"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        text(&out.stderr).contains("not paired"),
        "{}",
        text(&out.stderr)
    );

    let out = cli.run(&[
        "pair",
        "wxyz-6789",
        "--relay",
        &relay.url,
        "--api",
        "http://api.test",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("warning: PAI_KEYSTORE=file"));

    // `pai token` starts the local proxy and prints the key for it, never a credential.
    let out = cli.run(&["token"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("pai-local-") && stdout.ends_with('\n') && stdout.lines().count() == 1);
    let credential = stdout.trim().to_string();
    assert!(health(&credential), "the proxy is running");
    assert_eq!(text(&cli.run(&["token"]).stdout).trim(), credential);

    let out = cli.run(&["status"]);
    let status = text(&out.stdout);
    assert!(
        status.contains("Paired:     yes") && status.contains(&format!("running at http://127.0.0.1:{port}")),
        "{status}"
    );
    let secrets = std::fs::read_to_string(cli.dir.path().join("config/secrets.json")).unwrap();
    let device_key: serde_json::Value = serde_json::from_str(&secrets).unwrap();
    assert!(
        !status.contains(&credential)
            && !status.contains(device_key["device-key"].as_str().unwrap())
    );

    let out = cli.run(&["setup", "claude-code", "--apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let settings =
        std::fs::read_to_string(cli.dir.path().join("home/.claude/settings.json")).unwrap();
    assert!(
        settings.contains("\"apiKeyHelper\"") && settings.contains(&format!("http://127.0.0.1:{port}")),
        "{settings}"
    );

    let out = cli.run(&["unpair"]);
    assert!(out.status.success());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!health(&credential), "unpairing stops the proxy");
    assert!(relay.state.lock().unwrap().devices.is_empty());
    assert_eq!(cli.run(&["token"]).status.code(), Some(2));
}
