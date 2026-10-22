// Every test here talks to a fake model API over TLS with its own CA, which only a staging
// build can trust: run them with `cargo test --features test-mode`.
#![cfg(feature = "test-mode")]

mod common;

use std::convert::Infallible;
use std::net::TcpListener as StdListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{FakeRelay, TestEnv};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use pai::app;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::Value;

const CODE: &str = "ABCD2345";

#[derive(Default)]
struct Seen {
    requests: Vec<(String, Option<String>, bool)>,
    reject_first: bool,
}

/// A fake model API over TLS with a self-signed certificate for `localhost`. It records each
/// request's path, Authorization header and whether an x-api-key reached it.
fn fake_gateway(seen: Arc<Mutex<Seen>>) -> (u16, String, String) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem = ck.cert.pem();
    let pin = pai::pin::spki_pin(&CertificateDer::from(ck.cert.der().to_vec())).unwrap();
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(ck.cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der())),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let std = StdListener::bind("127.0.0.1:0").unwrap();
    let port = std.local_addr().unwrap().port();
    std.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std).unwrap();
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let (acceptor, seen) = (acceptor.clone(), seen.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let svc = service_fn(move |req: hyper::Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            let auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string());
                            let mut s = seen.lock().unwrap();
                            s.requests.push((req.uri().path().to_string(), auth, req.headers().contains_key("x-api-key")));
                            let (status, body) = if s.reject_first && s.requests.len() == 1 {
                                (401, r#"{"error":{"type":"invalid_credential","message":"stale"}}"#)
                            } else {
                                (200, r#"{"ok":true}"#)
                            };
                            Ok::<_, Infallible>(
                                hyper::Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(http_body_util::Full::new(bytes::Bytes::from(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await;
                });
            }
        });
    });
    (port, cert_pem, pin)
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Running {
    _t: TestEnv,
    port: u16,
    key: String,
    http: reqwest::blocking::Client,
}

impl Running {
    fn post(&self, path: &str, key: Option<&str>, extra: &[(&str, &str)]) -> (u16, Value) {
        let mut req = self.http.post(format!("http://127.0.0.1:{}{path}", self.port)).body("{}");
        if let Some(k) = key {
            req = req.header("x-api-key", k);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let r = req.send().unwrap();
        let status = r.status().as_u16();
        (status, r.json().unwrap_or(Value::Null))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self
            .http
            .post(format!("http://127.0.0.1:{}/pai/stop", self.port))
            .bearer_auth(&self.key)
            .send();
    }
}

fn start(api_port: u16, ca_pem: &str, pin: &str) -> (Running, FakeRelay) {
    let relay = FakeRelay::start(CODE);
    let mut t = TestEnv::new();
    let api = format!("https://localhost:{api_port}");
    let ca = t.dir.path().join("ca.pem");
    std::fs::write(&ca, ca_pem).unwrap();
    let pins = [pin.to_string()];
    app::pair(&t.env, CODE, Some(&relay.url), Some(&api), None, Some((ca.as_path(), &pins[..]))).unwrap();
    let port = free_port();
    t.env.vars.insert("PAI_PROXY_PORT".into(), port.to_string());
    let key = app::local_key(&t.env).unwrap();
    let env = t.env.clone();
    std::thread::spawn(move || pai::proxy::run(env));
    let http = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap();
    for _ in 0..100 {
        if http.get(format!("http://127.0.0.1:{port}/pai/health")).bearer_auth(&key).send().is_ok_and(|r| r.status() == 200) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (Running { _t: t, port, key, http }, relay)
}

#[test]
fn forwards_with_the_credential_over_pinned_tls() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, relay) = start(api_port, &ca, &pin);

    let (status, body) = p.post("/v1/messages", Some(&p.key), &[("anthropic-version", "2023-06-01")]);
    assert_eq!(status, 200, "{body}");
    let s = seen.lock().unwrap();
    let (path, auth, had_key) = &s.requests[0];
    assert_eq!(path, "/v1/messages");
    assert!(auth.as_deref().unwrap().starts_with("Bearer pai1."), "the credential is added");
    assert!(!had_key, "the local key never leaves this machine");
    assert_eq!(relay.issued(), 1);
}

#[test]
fn refuses_a_server_whose_key_isnt_pinned() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (api_port, ca, _pin) = fake_gateway(seen.clone());
    let wrong = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let (p, _relay) = start(api_port, &ca, wrong);

    let (status, body) = p.post("/v1/chat/completions", Some(&p.key), &[]);
    assert_eq!(status, 502);
    assert!(body["error"]["message"].as_str().unwrap().contains("isn't one pai trusts"), "{body}");
    assert!(seen.lock().unwrap().requests.is_empty(), "nothing was sent");
}

#[test]
fn refuses_web_pages_other_hosts_and_wrong_keys() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);

    assert_eq!(p.post("/v1/chat/completions", None, &[]).0, 401);
    assert_eq!(p.post("/v1/chat/completions", Some("pai-local-wrong"), &[]).0, 401);
    assert_eq!(p.post("/v1/chat/completions", Some(&p.key), &[("origin", "https://evil.example")]).0, 403);
    assert_eq!(p.post("/v1/chat/completions", Some(&p.key), &[("host", "evil.example")]).0, 403);
    assert_eq!(p.post("/admin", Some(&p.key), &[]).0, 404);
    assert!(seen.lock().unwrap().requests.is_empty());
}

#[test]
fn replaces_a_rejected_credential_once() {
    let seen = Arc::new(Mutex::new(Seen { reject_first: true, ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, relay) = start(api_port, &ca, &pin);

    let (status, _) = p.post("/v1/chat/completions", Some(&p.key), &[]);
    assert_eq!(status, 200);
    let s = seen.lock().unwrap();
    assert_eq!(s.requests.len(), 2);
    assert_ne!(s.requests[0].1, s.requests[1].1, "a fresh credential on the retry");
    assert_eq!(relay.issued(), 2);
}

#[test]
fn test_mode_is_on_only_when_paired_with_it_and_says_so() {
    let relay = FakeRelay::start(CODE);
    let t = TestEnv::new();
    app::pair(&t.env, CODE, Some(&relay.url), None, None, None).unwrap();
    let cfg = t.env.config().unwrap().unwrap();
    let normal = app::trust(&t.env, Some(&cfg));
    assert!(!normal.is_test());
    assert!(app::test_mode_warning(&normal).is_none());
    assert!(!app::status(&t.env).unwrap().to_string().contains("TEST MODE"));

    // The environment variables alone need a CA too: pins alone don't change anything.
    let mut e = t.env.clone();
    e.vars.insert("PAI_TEST_API_PINS".into(), "AAAA".into());
    assert_eq!(app::trust(&e, Some(&cfg)), normal);
    e.vars.insert("PAI_TEST_CA".into(), "/tmp/ca.pem".into());
    let test = app::trust(&e, Some(&cfg));
    assert!(test.is_test());
    assert_eq!(test.api_pins, vec!["AAAA".to_string()]);
    assert!(app::test_mode_warning(&test).unwrap().contains("TEST MODE"));
    assert!(app::status(&e).unwrap().to_string().contains("TEST MODE"));
}

#[test]
fn test_mode_needs_pins() {
    let relay = FakeRelay::start(CODE);
    let t = TestEnv::new();
    let ca = t.dir.path().join("ca.pem");
    std::fs::write(&ca, "x").unwrap();
    let err = app::pair(&t.env, CODE, Some(&relay.url), None, None, Some((ca.as_path(), &[]))).unwrap_err();
    assert!(err.to_string().contains("pins"), "{err}");
}
