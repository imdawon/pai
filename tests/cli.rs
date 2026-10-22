mod common;

use std::process::{Command, Output};

use common::FakeRelay;

struct Cli {
    dir: tempfile::TempDir,
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
    let cli = Cli {
        dir: tempfile::tempdir().unwrap(),
    };

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

    let out = cli.run(&["token"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("pai1.") && stdout.ends_with('\n') && stdout.lines().count() == 1);
    let credential = stdout.trim().to_string();
    assert_eq!(text(&cli.run(&["token"]).stdout).trim(), credential);

    let out = cli.run(&["status"]);
    let status = text(&out.stdout);
    assert!(
        status.contains("Paired:     yes") && status.contains("yes, for today"),
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
        settings.contains("\"apiKeyHelper\"") && settings.contains("http://api.test"),
        "{settings}"
    );

    let out = cli.run(&["unpair"]);
    assert!(out.status.success());
    assert!(relay.state.lock().unwrap().devices.is_empty());
    assert_eq!(cli.run(&["token"]).status.code(), Some(2));
}
