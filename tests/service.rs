mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::{FakeRelay, TestEnv};
use pai::app;
use pai::error::Error;
use pai::service::{self, Manager};

fn uid() -> String {
    String::from_utf8(std::process::Command::new("id").arg("-u").output().unwrap().stdout).unwrap().trim().to_string()
}

#[test]
#[cfg(unix)]
fn launchd_install_is_idempotent_and_uninstalls() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::Launchd;
    t.env.pai_bin = PathBuf::from("/Apps & Tools/pai");
    t.env.vars.insert("PAI_CONFIG_DIR".into(), "/tmp/pai <cfg>".into());
    t.env.vars.insert("PAI_KEYSTORE".into(), "file".into());
    let port = t.env.vars["PAI_PROXY_PORT"].clone();

    let notes = service::install(&t.env).unwrap();
    let path = t.home().join("Library/LaunchAgents/ai.privacyai.pai-proxy.plist");
    assert_eq!(service::file(&t.env).unwrap(), path);
    assert!(notes[0].contains(&path.display().to_string()), "{notes:?}");
    let plist = fs::read_to_string(&path).unwrap();
    for want in [
        "<string>ai.privacyai.pai-proxy</string>",
        "<string>/Apps &amp; Tools/pai</string>\n        <string>proxy</string>",
        "<key>RunAtLoad</key>\n    <true/>",
        "<key>KeepAlive</key>",
        "<key>PAI_CONFIG_DIR</key>\n        <string>/tmp/pai &lt;cfg&gt;</string>",
        "<key>PAI_KEYSTORE</key>\n        <string>file</string>",
        &format!("<key>PAI_PROXY_PORT</key>\n        <string>{port}</string>"),
    ] {
        assert!(plist.contains(want), "missing {want:?} in\n{plist}");
    }
    assert!(!plist.contains("PAI_PREFETCH"), "only variables that are set are carried");

    let target = format!("gui/{}/ai.privacyai.pai-proxy", uid());
    let bootstrap = format!("launchctl bootstrap gui/{} {}", uid(), path.display());
    assert_eq!(t.env.commands.recorded(), [format!("launchctl bootout {target}"), bootstrap.clone()]);

    service::install(&t.env).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), plist);
    assert_eq!(t.env.commands.recorded().iter().filter(|c| **c == bootstrap).count(), 2);
    assert!(service::describe(&t.env).starts_with("installed"), "{}", service::describe(&t.env));
    assert!(service::status(&t.env).contains(&format!("not running at http://127.0.0.1:{port}")));

    let notes = service::uninstall(&t.env).unwrap();
    assert!(notes[0].contains("Removed the login service"), "{notes:?}");
    assert!(!path.exists());
    assert_eq!(t.env.commands.recorded().last().unwrap(), &format!("launchctl bootout {target}"));
    assert!(service::uninstall(&t.env).unwrap()[0].contains("isn't installed"));
    assert_eq!(service::describe(&t.env), "not installed");
}

#[test]
fn systemd_unit_goes_in_the_user_config_dir() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::Systemd;
    t.env.pai_bin = PathBuf::from("/home/me/my bin/100%/pai");
    let xdg = t.dir.path().join("xdg");
    t.env.vars.insert("XDG_CONFIG_HOME".into(), xdg.to_string_lossy().into_owned());
    let port = t.env.vars["PAI_PROXY_PORT"].clone();

    service::install(&t.env).unwrap();
    let path = xdg.join("systemd/user/pai-proxy.service");
    let unit = fs::read_to_string(&path).unwrap();
    assert!(unit.contains("ExecStart=\"/home/me/my bin/100%%/pai\" proxy\n"), "{unit}");
    assert!(unit.contains("Restart=on-failure\n"), "{unit}");
    assert!(unit.contains(&format!("Environment=\"PAI_PROXY_PORT={port}\"\n")), "{unit}");
    assert!(unit.contains("[Install]\nWantedBy=default.target\n"), "{unit}");
    assert_eq!(
        t.env.commands.recorded(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable pai-proxy.service",
            "systemctl --user restart pai-proxy.service"
        ]
    );

    t.env.vars.remove("XDG_CONFIG_HOME");
    assert_eq!(service::file(&t.env).unwrap(), t.home().join(".config/systemd/user/pai-proxy.service"));
}

#[test]
fn without_systemd_install_says_how_to_turn_it_on_under_wsl() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::Systemd;
    t.env.vars.insert("XDG_CONFIG_HOME".into(), t.dir.path().join("xdg").to_string_lossy().into_owned());
    t.env.vars.insert("PAI_TEST_NO_SYSTEMD".into(), "1".into());

    let e = service::install(&t.env).unwrap_err().to_string();
    assert!(e.contains("doesn't run systemd") && e.contains("pai token"), "{e}");
    t.env.vars.insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
    let e = service::install(&t.env).unwrap_err().to_string();
    assert!(e.contains("systemd=true") && e.contains("/etc/wsl.conf") && e.contains("wsl --shutdown"), "{e}");
    assert!(!service::installed(&t.env), "nothing is written");
    assert!(t.env.commands.recorded().is_empty());

    // `pai pair --setup` under WSL says the same, without failing.
    let notes = service::set_up_under_wsl(&t.env);
    assert!(notes.len() == 1 && notes[0].contains("systemd=true"), "{notes:?}");
}

#[test]
fn pair_setup_installs_the_service_under_wsl_only() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::Systemd;
    t.env.vars.insert("XDG_CONFIG_HOME".into(), t.dir.path().join("xdg").to_string_lossy().into_owned());
    assert!(service::set_up_under_wsl(&t.env).is_empty(), "plain Linux keeps `pai token` starting the proxy");
    assert!(!service::installed(&t.env));

    t.env.vars.insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
    let notes = service::set_up_under_wsl(&t.env);
    assert!(notes[0].starts_with("Wrote "), "{notes:?}");
    assert!(service::installed(&t.env));
    assert!(t.env.commands.recorded().contains(&"systemctl --user enable pai-proxy.service".to_string()));
    assert!(service::set_up_under_wsl(&t.env).is_empty(), "once installed, it's left alone");
}

#[test]
fn unsupported_os_says_what_to_do() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::Unsupported;
    let e = service::install(&t.env).unwrap_err().to_string();
    assert!(e.contains("pai token"), "{e}");
    assert!(service::uninstall(&t.env).is_err());
    assert_eq!(service::describe(&t.env), "not available on this OS");
    assert!(t.env.commands.recorded().is_empty());
}

#[test]
fn install_needs_a_pairing() {
    let t = TestEnv::new();
    assert!(matches!(service::install(&t.env), Err(Error::NotPaired) | Err(Error::Setup(_))));
    assert!(!service::installed(&t.env));
    assert!(t.env.commands.recorded().is_empty());
}

#[test]
fn install_stops_a_proxy_started_by_pai_token() {
    let t = TestEnv::with_config();
    let key = app::local_key(&t.env).unwrap();
    let port = &t.env.vars["PAI_PROXY_PORT"];
    let env = t.env.clone();
    let proxy = std::thread::spawn(move || pai::proxy::run(env));
    let http = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build().unwrap();
    let up = || http.get(format!("http://127.0.0.1:{port}/pai/health")).bearer_auth(&key).send().is_ok_and(|r| r.status() == 200);
    for _ in 0..100 {
        if up() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(up(), "the proxy started");
    assert!(service::status(&t.env).contains("Proxy:      running"));

    if service::file(&t.env).is_some() {
        service::install(&t.env).unwrap();
        assert!(!up(), "install stopped the old proxy so the service can take the port");
    } else {
        http.post(format!("http://127.0.0.1:{port}/pai/stop")).bearer_auth(&key).send().unwrap();
    }
    proxy.join().unwrap().unwrap();
}

#[test]
fn unpair_removes_the_service() {
    let relay = FakeRelay::start("QRST6789");
    let t = TestEnv::new();
    app::pair(&t.env, "QRST6789", Some(&relay.url), Some("http://api.test"), None, None, None).unwrap();
    if service::file(&t.env).is_none() {
        return;
    }
    service::install(&t.env).unwrap();
    assert!(service::installed(&t.env));
    assert!(app::status(&t.env).unwrap().to_string().contains("Service:    installed"));

    let notes = app::unpair(&t.env).unwrap();
    assert!(notes.iter().any(|n| n.contains("Removed the login service")), "{notes:?}");
    assert!(!service::installed(&t.env));
}

#[test]
fn windows_install_adds_a_run_key_entry_and_removes_it() {
    let mut t = TestEnv::with_config();
    t.env.service_manager = Manager::WindowsRun;
    t.env.pai_bin = PathBuf::from(r"C:\Users\Me Too\AppData\Local\pai\pai.exe");
    t.env.vars.insert("SystemRoot".into(), r"C:\WINDOWS".into());
    let port = t.env.vars.remove("PAI_PROXY_PORT").unwrap();

    assert!(service::supported(&t.env));
    assert!(service::file(&t.env).is_none(), "the registry holds it, not a file");
    assert_eq!(service::describe(&t.env), "not installed");
    let command = service::run_command(&t.env);
    assert_eq!(command, r#""C:\WINDOWS\System32\conhost.exe" --headless "C:\Users\Me Too\AppData\Local\pai\pai.exe" proxy"#);

    let notes = service::install(&t.env).unwrap();
    assert!(notes[0].contains("DawonAI pai proxy"), "{notes:?}");
    let add = format!(r"reg add HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v DawonAI pai proxy /t REG_SZ /d {command} /f");
    assert_eq!(t.env.commands.recorded(), std::slice::from_ref(&add));
    assert!(service::installed(&t.env));
    assert!(service::describe(&t.env).starts_with("installed, starts at login"), "{}", service::describe(&t.env));
    assert!(!service::restart(&t.env), "`pai token` starts the proxy itself on Windows");

    // Again is fine: /f replaces the value.
    service::install(&t.env).unwrap();
    assert_eq!(t.env.commands.recorded().iter().filter(|c| **c == add).count(), 2);

    let notes = service::uninstall(&t.env).unwrap();
    assert!(notes[0].contains("Removed the login service"), "{notes:?}");
    assert!(!service::installed(&t.env));
    assert_eq!(service::describe(&t.env), "not installed");
    assert!(service::uninstall(&t.env).unwrap()[0].contains("isn't installed"));

    // The Run key can't carry PAI_PROXY_PORT and the like, so pai refuses rather than start a
    // proxy with other settings.
    t.env.vars.insert("PAI_PROXY_PORT".into(), port);
    let e = service::install(&t.env).unwrap_err().to_string();
    assert!(e.contains("PAI_PROXY_PORT") && e.contains("pai token"), "{e}");
    assert!(!service::installed(&t.env));
}
