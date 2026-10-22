//! Runs `pai` against a real pai-relay. Opt in with `PAI_TEST_RELAY=http://127.0.0.1:8787` and
//! `cargo test --test real_relay -- --ignored`; the relay needs `PAI_DEV_PAYMENTS=1`.

mod common;

use common::TestEnv;
use pai::app;
use pai::error::Error;
use pai_proto::ed25519_dalek::SigningKey;
use pai_proto::{Credential, DeviceAuth, b64, sha256_hex};
use rand::RngCore;
use serde_json::{Value, json};

struct Browser {
    relay: String,
    client: reqwest::blocking::Client,
    key: SigningKey,
    id: String,
}

impl Browser {
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let body = body
            .map(|b| serde_json::to_vec(&b).unwrap())
            .unwrap_or_default();
        for _ in 0..2 {
            let auth = DeviceAuth::sign(&self.key, &self.id, method, path, &body);
            let resp = self
                .client
                .request(method.parse().unwrap(), format!("{}{path}", self.relay))
                .header("authorization", auth.header())
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .unwrap();
            let status = resp.status();
            let v: Value = resp.json().unwrap_or(Value::Null);
            if v["error"]["type"] == "replayed_request" {
                std::thread::sleep(std::time::Duration::from_millis(1100));
                continue;
            }
            assert!(status.is_success(), "{method} {path}: {v}");
            return v;
        }
        panic!("replayed twice");
    }
}

fn leading_zero_bits(hex: &str) -> u32 {
    let mut bits = 0;
    for c in hex.chars() {
        let n = c.to_digit(16).unwrap();
        if n == 0 {
            bits += 4;
        } else {
            return bits + n.leading_zeros() - 28;
        }
    }
    bits
}

fn sign_up(relay: &str) -> Browser {
    let client = reqwest::blocking::Client::new();
    let c: Value = client
        .get(format!("{relay}/v1/challenge"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let (salt, bits) = (
        c["salt"].as_str().unwrap(),
        c["bits"].as_u64().unwrap() as u32,
    );
    let nonce = (0u64..)
        .find(|n| leading_zero_bits(&sha256_hex(format!("{salt}{n}"))) >= bits)
        .unwrap();
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let mut login_key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut login_key);
    let v: Value = client
        .post(format!("{relay}/v1/accounts"))
        .json(&json!({
            "login_key": b64(login_key),
            "device": {"public_key": b64(key.verifying_key().to_bytes()), "name": "test browser", "kind": "browser"},
            "pow": {"id": c["id"], "nonce": nonce},
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let id = v["device_id"]
        .as_str()
        .unwrap_or_else(|| panic!("signup failed: {v}"))
        .to_string();
    Browser {
        relay: relay.into(),
        client,
        key,
        id,
    }
}

#[test]
#[ignore]
fn against_real_relay() {
    let relay = std::env::var("PAI_TEST_RELAY").expect("set PAI_TEST_RELAY");
    let browser = sign_up(&relay);
    let t = TestEnv::new();

    let code = browser.call("POST", "/v1/pairings", None)["code"]
        .as_str()
        .unwrap()
        .to_string();
    let spaced = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    app::pair(&t.env, &spaced, Some(&relay), None, None, &[]).unwrap();

    let err = app::credential(&t.env, false).unwrap_err();
    assert!(matches!(err, Error::NotPaid { .. }), "{err:?}");

    browser.call("POST", "/v1/dev/pay", Some(json!({"plan": "agent"})));
    let first = app::credential(&t.env, false).unwrap();
    assert_eq!(
        Credential::decode(&first).unwrap().plan,
        pai_proto::Plan::Agent
    );
    assert_eq!(app::credential(&t.env, false).unwrap(), first);
    assert!(app::status(&t.env).unwrap().plan.starts_with("agent"));

    app::credential(&t.env, true).unwrap();
    app::credential(&t.env, true).unwrap();
    let err = app::credential(&t.env, true).unwrap_err();
    assert!(
        matches!(
            err,
            Error::IssuanceLimit {
                retry_after: Some(_)
            }
        ),
        "{err:?}"
    );
    eprintln!("issuance limit message: {err}");

    let devices = browser.call("GET", "/v1/devices", None);
    let tool = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["kind"] == "tool")
        .unwrap()
        .clone();
    let tool_id = tool["device_id"].as_str().unwrap();
    browser.call("DELETE", &format!("/v1/devices/{tool_id}"), None);
    let mut cfg = t.env.config().unwrap().unwrap();
    assert_eq!(cfg.device_id, tool_id);
    let err = app::status(&t.env).unwrap().plan;
    assert!(err.contains("device removed"), "{err}");
    cfg.relay_url = relay.clone();
    t.env.keystore.delete(pai::keystore::CREDENTIAL).unwrap();
    let err = app::credential(&t.env, false).unwrap_err();
    assert!(matches!(err, Error::DeviceRemoved), "{err:?}");

    let notes = app::unpair(&t.env).unwrap();
    assert!(notes[0].contains("already removed"), "{notes:?}");

    let code = browser.call("POST", "/v1/pairings", None)["code"]
        .as_str()
        .unwrap()
        .to_string();
    app::pair(&t.env, &code, Some(&relay), None, None, &[]).unwrap();
    app::credential(&t.env, false).unwrap();
    let notes = app::unpair(&t.env).unwrap();
    assert!(notes[0].contains("Removed this device"), "{notes:?}");
}
