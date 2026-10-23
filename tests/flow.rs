mod common;

use common::{ACCOUNT_ID, FakeRelay, TestEnv, issuer_public_key, today};
use pai::app::{self, Clock};
use pai::error::Error;
use pai::keystore::{CREDENTIAL, DEVICE_KEY};
use pai_proto::{Credential, Plan, now_unix};

const CODE: &str = "ABCD2345";

fn paired(relay: &FakeRelay) -> TestEnv {
    let t = TestEnv::new();
    app::pair(
        &t.env,
        "abcd-2345",
        Some(&relay.url),
        Some("http://api.test/"),
        None,
        Some("test box"),
        None,
    )
    .unwrap();
    t
}

#[test]
fn pair_token_cached_token_unpair() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);

    let cfg = t.env.config().unwrap().unwrap();
    assert_eq!(cfg.account_id, ACCOUNT_ID);
    assert_eq!(cfg.device_id, "dev_1");
    assert_eq!(cfg.relay_url, relay.url);
    assert_eq!(cfg.api_url, "http://api.test");
    assert!(t.env.keystore.get(DEVICE_KEY).unwrap().is_some());
    let config_text = std::fs::read_to_string(t.env.paths.config_file()).unwrap();
    assert!(!config_text.contains(&*t.env.keystore.get(DEVICE_KEY).unwrap().unwrap()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&t.env.paths.secrets_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let first = app::credential(&t.env, false).unwrap();
    let cred = Credential::decode(&first).unwrap();
    assert_eq!(cred.day, today());
    assert_eq!(cred.plan, Plan::Chat);
    cred.verify(issuer_public_key(Plan::Chat, today())).unwrap();
    assert_eq!(relay.issued(), 1);

    let requests_before = relay.state.lock().unwrap().requests.len();
    let second = app::credential(&t.env, false).unwrap();
    assert_eq!(first, second);
    assert_eq!(relay.issued(), 1);
    assert_eq!(
        relay.state.lock().unwrap().requests.len(),
        requests_before,
        "a cached token makes no requests"
    );

    let status = app::status(&t.env).unwrap();
    assert!(status.credential_today);
    let shown = status.to_string();
    assert!(
        shown.contains("Paired:     yes")
            && shown.contains("Chat, monthly (active)")
            && shown.contains("dev_1"),
        "{shown}"
    );
    assert!(!shown.contains(&first));

    let notes = app::unpair(&t.env).unwrap();
    assert!(notes[0].contains("Removed this device"), "{notes:?}");
    assert!(relay.state.lock().unwrap().devices.is_empty());
    assert!(t.env.config().unwrap().is_none());
    assert!(t.env.keystore.get(DEVICE_KEY).unwrap().is_none());
    assert!(t.env.keystore.get(CREDENTIAL).unwrap().is_none());
    assert!(!t.env.paths.secrets_file.exists());

    let err = app::credential(&t.env, false).unwrap_err();
    assert!(matches!(err, Error::NotPaired));
    assert!(err.to_string().starts_with("not paired"));
}

#[test]
fn yesterdays_credential_is_replaced() {
    let relay = FakeRelay::start(CODE);
    let mut t = paired(&relay);

    t.env.clock = Clock::Fixed(now_unix() - 86_400);
    let old = app::credential(&t.env, false).unwrap();
    assert_eq!(Credential::decode(&old).unwrap().day, today() - 1);

    t.env.clock = Clock::System;
    let new = app::credential(&t.env, false).unwrap();
    assert_ne!(old, new);
    let cred = Credential::decode(&new).unwrap();
    assert_eq!(cred.day, today());
    cred.verify(issuer_public_key(Plan::Chat, today())).unwrap();
    assert_eq!(relay.issued(), 2);

    assert_eq!(app::credential(&t.env, false).unwrap(), new);
    app::credential(&t.env, true).unwrap();
    assert_eq!(relay.issued(), 3, "--refresh always fetches");
}

#[test]
fn issuance_limit_is_explained() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    for _ in 0..3 {
        app::credential(&t.env, true).unwrap();
    }
    let err = app::credential(&t.env, true).unwrap_err();
    assert!(
        matches!(
            err,
            Error::IssuanceLimit {
                retry_after: Some(7380)
            }
        ),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .contains("already collected 3 credentials today")
            && err.to_string().contains("2 h 3 min")
    );
}

#[test]
fn replayed_write_is_retried() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    relay.state.lock().unwrap().replay_next_write = true;
    app::credential(&t.env, false).unwrap();
    let requests = relay.state.lock().unwrap().requests.clone();
    assert_eq!(
        requests
            .iter()
            .filter(|r| *r == "POST /v1/credentials")
            .count(),
        2,
        "{requests:?}"
    );
    assert_eq!(relay.issued(), 1);
}

#[test]
fn parallel_calls_fetch_once() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    let tokens: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|_| s.spawn(|| app::credential(&t.env, false).unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(tokens.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(relay.issued(), 1);
}

#[test]
fn unpaid_account() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    relay.set_unpaid();
    let err = app::credential(&t.env, false).unwrap_err();
    assert!(matches!(err, Error::NotPaid { .. }), "{err:?}");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(
        msg.starts_with("plan not paid") && msg.contains("lapsed"),
        "{msg}"
    );
    assert!(
        app::status(&t.env)
            .unwrap()
            .to_string()
            .contains("not paid")
            || app::status(&t.env).unwrap().plan.contains("lapsed")
    );
}

/// What the opener was asked to open: the web app's root, by the OS's opener.
fn opened(t: &TestEnv) -> Vec<String> {
    let opener = if cfg!(target_os = "macos") {
        "open "
    } else if cfg!(windows) {
        "cmd /c start  "
    } else {
        "xdg-open "
    };
    let recorded = t.env.commands.recorded();
    assert!(recorded.iter().all(|c| c.starts_with(opener)), "{recorded:?}");
    recorded
}

#[test]
fn removed_device() {
    let relay = FakeRelay::start(CODE);
    let mut t = paired(&relay);
    relay.remove_all_devices();
    let err = app::token(&t.env).unwrap_err();
    assert!(matches!(err, Error::DeviceRemoved), "{err:?}");
    assert_eq!(err.exit_code(), 4);
    let msg = err.to_string();
    assert!(msg.starts_with("device removed, pair again"));
    assert!(msg.contains("unused for 30 days") && msg.contains("Account > Set up a coding tool"), "{msg}");

    // The web app opens once, and the day is noted so it doesn't open again today.
    assert_eq!(opened(&t).len(), 1);
    assert!(opened(&t)[0].ends_with(" https://app.privacyai.example/#account"), "{:?}", opened(&t));
    let marker = std::fs::read_to_string(t.env.paths.web_app_opened_file()).unwrap();
    assert_eq!(marker, today().to_string());
    app::credential(&t.env, false).unwrap_err();
    let status = app::status(&t.env).unwrap();
    assert!(status.plan.contains("device removed") && status.device_removed);
    let shown = status.to_string();
    assert!(shown.contains("device removed, pair again") && shown.contains("DawonAI web app: https://app.privacyai.example/"), "{shown}");
    assert_eq!(opened(&t).len(), 1);

    // And again the next day.
    t.env.clock = Clock::Fixed(now_unix() + 86_400);
    app::credential(&t.env, false).unwrap_err();
    assert_eq!(opened(&t).len(), 2);

    let notes = app::unpair(&t.env).unwrap();
    assert!(notes[0].contains("already removed"), "{notes:?}");
    assert!(t.env.config().unwrap().is_none());
    assert!(!t.env.paths.web_app_opened_file().exists());
}

#[test]
fn removed_device_seen_by_the_proxy_opens_the_web_app() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    relay.remove_all_devices();
    let key = app::local_key(&t.env).unwrap();
    let env = t.env.clone();
    std::thread::spawn(move || pai::proxy::run(env));
    let base = pai::proxy::url(&t.env);
    let http = reqwest::blocking::Client::builder().no_proxy().build().unwrap();
    let up = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        http.get(format!("{base}/pai/health")).bearer_auth(&key).send().is_ok_and(|r| r.status() == 200)
    });
    assert!(up, "the proxy started");

    for _ in 0..2 {
        let r = http.post(format!("{base}/v1/messages")).header("x-api-key", &key).body("{}").send().unwrap();
        assert_eq!(r.status(), 401);
        assert!(r.text().unwrap().contains("device removed, pair again"));
    }
    let _ = http.post(format!("{base}/pai/stop")).bearer_auth(&key).send();
    assert_eq!(opened(&t).len(), 1);
}

#[test]
fn the_web_app_stays_closed_with_pai_open_browser_0() {
    let relay = FakeRelay::start(CODE);
    let mut t = paired(&relay);
    t.env.vars.insert("PAI_OPEN_BROWSER".into(), "0".into());
    relay.remove_all_devices();
    app::token(&t.env).unwrap_err();
    assert!(opened(&t).is_empty());
    assert!(!t.env.paths.web_app_opened_file().exists());
}

#[test]
fn web_app_address() {
    use pai::config::{app_url_for_relay, openable_url};
    assert_eq!(app_url_for_relay("https://relay.privacyai.example"), "https://app.privacyai.example");
    assert_eq!(app_url_for_relay("https://relay.staging.example:8443/prefix"), "https://app.staging.example:8443");
    assert_eq!(app_url_for_relay("http://127.0.0.1:8787"), "https://app.privacyai.example");
    assert_eq!(app_url_for_relay("https://relays.example"), "https://app.privacyai.example");
    assert!(openable_url("https://app.privacyai.example/"));
    for bad in ["file:///etc/passwd", "https://a.example/&calc", "https://a.example/\"x", "javascript:alert(1)", "https://a .example"] {
        assert!(!openable_url(bad), "{bad}");
    }

    let relay = FakeRelay::start(CODE);
    let t = TestEnv::new();
    let err = app::pair(&t.env, CODE, Some(&relay.url), None, Some("https://x.example/&calc"), None, None).unwrap_err();
    assert!(err.to_string().contains("isn't one pai can open"), "{err}");
    let mut t = TestEnv::new();
    t.env.vars.insert("PAI_APP_URL".into(), "https://app.staging.example/".into());
    let cfg = app::pair(&t.env, CODE, Some(&relay.url), None, None, None, None).unwrap();
    assert_eq!(cfg.app_url(), "https://app.staging.example");
    assert_eq!(t.env.config().unwrap().unwrap().app_url.as_deref(), Some("https://app.staging.example"));
}

#[test]
fn unreachable_relay_still_unpairs_locally() {
    let relay = FakeRelay::start(CODE);
    let t = paired(&relay);
    let mut cfg = t.env.config().unwrap().unwrap();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.relay_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    cfg.save(&t.env.paths.config_file()).unwrap();

    let err = app::credential(&t.env, false).unwrap_err();
    assert!(matches!(err, Error::RelayUnreachable(_)), "{err:?}");
    assert!(err.to_string().starts_with("relay unreachable"));

    let notes = app::unpair(&t.env).unwrap();
    assert!(
        notes[0].contains("Couldn't remove the device on the relay"),
        "{notes:?}"
    );
    assert!(t.env.config().unwrap().is_none());
    assert!(t.env.keystore.get(DEVICE_KEY).unwrap().is_none());
}

#[test]
fn pairing_errors() {
    let relay = FakeRelay::start(CODE);
    let t = TestEnv::new();
    assert!(matches!(
        app::pair(&t.env, "short", Some(&relay.url), None, None, None, None),
        Err(Error::BadPairingCode)
    ));
    let err = app::pair(&t.env, "ZZZZ-ZZZZ", Some(&relay.url), None, None, None, None).unwrap_err();
    assert!(matches!(err, Error::PairingRejected(_)), "{err:?}");
    assert!(t.env.config().unwrap().is_none());
    assert!(t.env.keystore.get(DEVICE_KEY).unwrap().is_none());

    app::pair(&t.env, CODE, Some(&relay.url), None, None, None, None).unwrap();
    relay.add_code("ZZZZ0000");
    assert!(matches!(
        app::pair(&t.env, "ZZZZ0000", Some(&relay.url), None, None, None, None),
        Err(Error::AlreadyPaired)
    ));
}

#[test]
fn normalizes_codes() {
    assert_eq!(app::normalize_code("abcd-efgh").unwrap(), "ABCDEFGH");
    assert_eq!(
        app::normalize_code("oiL0 1234").unwrap(),
        "0110 1234".replace(' ', "")
    );
    assert!(app::normalize_code("ABCD-EFGU").is_err());
    assert!(app::normalize_code("ABCD-EFG").is_err());
}

#[test]
fn tomorrows_credential_is_collected_ahead_and_used_without_a_fetch() {
    use pai::keystore::NEXT_CREDENTIAL;
    let relay = FakeRelay::start(CODE);
    let mut t = paired(&relay);

    // Yesterday, the background fetch collects the credential for today.
    t.env.clock = Clock::Fixed(now_unix() - 86_400);
    let zero = std::time::Duration::ZERO;
    assert!(!app::prefetch(&t.env, Plan::Chat, today() + 1, zero).unwrap(), "only for tomorrow");
    assert!(app::prefetch(&t.env, Plan::Chat, today(), zero).unwrap());
    assert!(!app::prefetch(&t.env, Plan::Chat, today(), zero).unwrap(), "already collected");
    assert_eq!(relay.issued(), 1);

    // Today, `pai token` uses it and asks the relay for nothing.
    t.env.clock = Clock::System;
    let requests_before = relay.state.lock().unwrap().requests.len();
    let token = app::credential(&t.env, false).unwrap();
    assert_eq!(Credential::decode(&token).unwrap().day, today());
    assert_eq!(relay.state.lock().unwrap().requests.len(), requests_before);
    assert_eq!(t.env.keystore.get(CREDENTIAL).unwrap().as_deref().map(String::as_str), Some(token.as_str()));
    assert!(t.env.keystore.get(NEXT_CREDENTIAL).unwrap().is_none());
}

#[test]
fn prefetch_waits_a_random_time_within_the_period() {
    use pai_proto::Period;
    let midnight = 20_000 * 86_400;
    for _ in 0..200 {
        let d = app::prefetch_delay(Period::Day, midnight + 3_600).unwrap().as_secs() as i64;
        assert!((0..86_400 - 3_600 - 900).contains(&d), "{d}");
        // On the 1-day trial, the next hour's credential is collected within this hour.
        let h = app::prefetch_delay(Period::Hour, midnight + 600).unwrap().as_secs() as i64;
        assert!((0..3_600 - 600 - 300).contains(&h), "{h}");
    }
    assert!(app::prefetch_delay(Period::Day, midnight + 86_400 - 600).is_none());
    assert!(app::prefetch_delay(Period::Hour, midnight + 3_600 - 120).is_none());
}
