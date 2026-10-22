// What a release build (no `test-mode` feature) does with test mode: nothing, except refuse a
// pairing a staging build made in test mode.
#![cfg(not(feature = "test-mode"))]

mod common;

use common::{ACCOUNT_ID, FakeRelay, TestEnv};
use pai::app;
use pai::config::{Config, TestMode};

const CODE: &str = "ABCD2345";

#[test]
fn refuses_a_pairing_made_in_test_mode_until_unpaired() {
    let t = TestEnv::new();
    let ca = t.env.paths.test_ca_file();
    std::fs::create_dir_all(&t.env.paths.config_dir).unwrap();
    std::fs::write(&ca, "attacker CA").unwrap();
    Config {
        relay_url: "http://127.0.0.1:9".into(),
        api_url: "https://api.attacker.example".into(),
        app_url: None,
        device_id: "dev_1".into(),
        account_id: ACCOUNT_ID.into(),
        test: Some(TestMode { ca: ca.clone(), api_pins: vec!["AAAA".into()] }),
    }
    .save(&t.env.paths.config_file())
    .unwrap();

    for err in [
        t.env.config().unwrap_err(),
        app::token(&t.env).unwrap_err(),
        app::status(&t.env).err().unwrap(),
        app::pair(&t.env, CODE, None, None, None, None, None).unwrap_err(),
    ] {
        let msg = err.to_string();
        assert!(msg.contains("staging build") && msg.contains("`pai unpair`"), "{msg}");
    }

    app::unpair(&t.env).unwrap();
    assert!(t.env.config().unwrap().is_none());
    assert!(!ca.exists(), "unpairing removes the test CA");
}

#[test]
fn has_no_test_mode() {
    let relay = FakeRelay::start(CODE);
    let mut t = TestEnv::new();
    let ca = t.dir.path().join("ca.pem");
    std::fs::write(&ca, "x").unwrap();
    let pins = ["AAAA".to_string()];
    let err = app::pair(&t.env, CODE, Some(&relay.url), None, None, None, Some((ca.as_path(), &pins[..]))).unwrap_err();
    assert!(err.to_string().contains("no test mode"), "{err}");
    assert!(t.env.config().unwrap().is_none());

    app::pair(&t.env, CODE, Some(&relay.url), Some("http://api.test"), None, None, None).unwrap();
    let cfg = t.env.config().unwrap().unwrap();
    let normal = app::trust(&t.env, Some(&cfg));
    t.env.vars.insert("PAI_TEST_CA".into(), ca.display().to_string());
    t.env.vars.insert("PAI_TEST_API_PINS".into(), "AAAA".into());
    let with_vars = app::trust(&t.env, Some(&cfg));
    assert_eq!(with_vars, normal, "PAI_TEST_CA and PAI_TEST_API_PINS do nothing");
    assert!(!with_vars.is_test());
    assert!(app::test_mode_warning(&with_vars).is_none());
}
