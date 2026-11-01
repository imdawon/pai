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
    announce: Option<String>,
    /// How long a request that takes a stream waits before it's answered.
    slow: Option<Duration>,
    /// Stream requests open now, and the most there have been at once.
    open: usize,
    most_open: usize,
    /// How many stream requests to refuse with `429 stream_limit` first.
    refuse_streams: usize,
    /// The stream limit to name in `pai-streams` on every response.
    streams_header: Option<String>,
    /// The context limit to name in `pai-context` on every response.
    context_header: Option<String>,
    /// How many requests to fail with `503` first.
    fail_first: usize,
    /// The relay's grants time to name in `pai-grants` on every response.
    grants_header: Option<String>,
    /// Refuse stream requests with a `dawonai report` piece: 1 in a 400 body, 2 in an SSE stream.
    refusal: u8,
    /// Bodies sent to `/v1/safety/report`.
    reports: Vec<String>,
    /// Whether each request said it handles reports (`x-pai-report: 1`).
    report_header: Vec<bool>,
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
                            let path = req.uri().path().to_string();
                            let stream = matches!(path.as_str(), "/v1/chat/completions" | "/v1/messages" | "/v1/responses");
                            let auth = req.headers().get("authorization").map(|v| v.to_str().unwrap().to_string());
                            seen.lock().unwrap().report_header.push(req.headers().get("x-pai-report").is_some_and(|v| v == "1"));
                            if path == "/v1/safety/report" {
                                use http_body_util::BodyExt;
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                seen.lock().unwrap().reports.push(String::from_utf8_lossy(&body).to_string());
                                let ok = hyper::Response::builder().status(200).header("content-type", "application/json");
                                return Ok::<_, Infallible>(ok.body(http_body_util::Full::new(bytes::Bytes::from(r#"{"received":true,"bytes":9}"#))).unwrap());
                            }
                            let refusal = if stream { std::mem::take(&mut seen.lock().unwrap().refusal) } else { 0 };
                            if refusal > 0 {
                                seen.lock().unwrap().requests.push((path.clone(), auth.clone(), false));
                                let report = r#"{"parts":["flag","ged!!"],"hour":1,"tag":"ab"}"#;
                                let (status, ctype, body) = if refusal == 1 {
                                    (400, "application/json", format!(r#"{{"type":"error","error":{{"type":"refused","message":"blocked"}},"dawonai_report":{report}}}"#))
                                } else {
                                    (200, "text/event-stream", format!("event: message_start\ndata: {{}}\n\n: dawonai-report {report}\n\nevent: error\ndata: {{\"type\":\"error\",\"error\":{{\"message\":\"blocked\"}}}}\n\n"))
                                };
                                let r = hyper::Response::builder().status(status).header("content-type", ctype);
                                return Ok::<_, Infallible>(r.body(http_body_util::Full::new(bytes::Bytes::from(body))).unwrap());
                            }
                            let (status, body, slow, announce, limit) = {
                                let mut s = seen.lock().unwrap();
                                s.requests.push((path, auth, req.headers().contains_key("x-api-key")));
                                let (status, body) = if s.reject_first && s.requests.len() == 1 {
                                    (401, r#"{"error":{"type":"invalid_credential","message":"stale"}}"#)
                                } else if s.fail_first > 0 {
                                    s.fail_first -= 1;
                                    (503, r#"{"error":{"type":"server_error","message":"down"}}"#)
                                } else if stream && s.refuse_streams > 0 {
                                    s.refuse_streams -= 1;
                                    (429, r#"{"type":"error","error":{"type":"stream_limit","message":"busy"}}"#)
                                } else {
                                    (200, r#"{"ok":true}"#)
                                };
                                let open = stream && status == 200;
                                if open {
                                    s.open += 1;
                                    s.most_open = s.most_open.max(s.open);
                                }
                                (status, body, open.then_some(s.slow).flatten(), s.announce.clone(), (s.streams_header.clone(), s.grants_header.clone(), s.context_header.clone()))
                            };
                            let (limit, grants, context) = limit;
                            if let Some(d) = slow {
                                tokio::time::sleep(d).await;
                            }
                            if stream && status == 200 {
                                seen.lock().unwrap().open -= 1;
                            }
                            let mut builder = hyper::Response::builder().status(status).header("content-type", "application/json");
                            if status == 429 {
                                builder = builder.header("retry-after", "0");
                            }
                            if let Some(v) = &announce {
                                builder = builder.header("pai-latest", v.as_str());
                            }
                            if let Some(v) = &limit {
                                builder = builder.header("pai-streams", v.as_str());
                            }
                            if let Some(v) = &grants {
                                builder = builder.header("pai-grants", v.as_str());
                            }
                            if let Some(v) = &context {
                                builder = builder.header("pai-context", v.as_str());
                            }
                            Ok::<_, Infallible>(builder.body(http_body_util::Full::new(bytes::Bytes::from(body))).unwrap())
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

impl Running {
    fn send(&self, path: &str, body: Value) -> (u16, String) {
        let r = self.http.post(format!("http://127.0.0.1:{}{path}", self.port)).header("x-api-key", &self.key).json(&body).send().unwrap();
        (r.status().as_u16(), r.text().unwrap())
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
    app::pair(&t.env, CODE, Some(&relay.url), Some(&api), None, None, Some((ca.as_path(), &pins[..]))).unwrap();
    let port = free_port();
    t.env.vars.insert("PAI_PROXY_PORT".into(), port.to_string());
    t.env.vars.insert("PAI_TEST_RETRY_MS".into(), "1".into());
    t.env.vars.insert("PAI_TEST_GRANTS_DELAY_MS".into(), "1".into());
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
fn keeps_the_announced_release_from_the_tool() {
    let seen = Arc::new(Mutex::new(Seen { announce: Some("99.0.0".into()), ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);

    let r = p
        .http
        .post(format!("http://127.0.0.1:{}/v1/messages", p.port))
        .header("x-api-key", &p.key)
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["x-pai-proxy"], "1");
    assert!(r.headers().get("pai-latest").is_none(), "the announcement is for the proxy, not the coding tool");
}

#[test]
fn a_new_grants_time_replaces_a_pass_with_the_old_adult_setting() {
    let seen = Arc::new(Mutex::new(Seen { grants_header: Some("100".into()), ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, relay) = start(api_port, &ca, &pin);
    let adult_of = |i: usize| {
        let s = seen.lock().unwrap();
        let auth = s.requests[i].1.clone().unwrap();
        pai_proto::Credential::decode(auth.trim_start_matches("Bearer ")).unwrap().adult
    };
    let settle = |issued: u32| {
        for _ in 0..200 {
            if relay.issued() == issued && relay.state.lock().unwrap().requests.iter().filter(|r| r.as_str() == "GET /v1/account").count() >= 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // The first time pai hears a grants time it checks the account once; nothing has changed.
    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert!(!adult_of(0));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(relay.issued(), 1, "the pass still matches the account");

    // The operator turns adult content on; the model API names a new grants time.
    relay.state.lock().unwrap().adult = true;
    seen.lock().unwrap().grants_header = Some("200".into());
    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert!(!adult_of(1), "the request that brings the news still uses the old pass");
    settle(2);
    assert_eq!(relay.issued(), 2, "pai collected a pass with the new setting");

    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert!(adult_of(2), "the next request carries the adult bit");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(relay.issued(), 2, "the same grants time isn't checked twice");
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

    // pai's own errors come in the shape of the API the tool called.
    let (_, body) = p.post("/v1/chat/completions", Some("pai-local-wrong"), &[]);
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert_eq!(body["error"]["code"], "invalid_api_key");
    assert_eq!(body["error"]["param"], Value::Null);
    assert_eq!(body["dawonai_error"], "authentication_error");
    assert!(body.get("type").is_none(), "no Anthropic wrapper on an OpenAI endpoint: {body}");
    let (_, body) = p.post("/v1/messages", Some("pai-local-wrong"), &[]);
    assert_eq!((body["type"].as_str(), body["error"]["type"].as_str()), (Some("error"), Some("authentication_error")));
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
fn holds_requests_over_the_plans_stream_limit() {
    let seen = Arc::new(Mutex::new(Seen { slow: Some(Duration::from_millis(300)), ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);

    // The fake relay issues Chat passes, and this API names no limit, so pai uses its own: 1.
    let statuses = send_at_once(&p, 3);
    assert_eq!(statuses, [200; 3]);
    let s = seen.lock().unwrap();
    assert_eq!(s.requests.len(), 3, "each request is sent once");
    assert_eq!(s.most_open, 1, "never more than the plan's 1 stream at a time");
}

#[test]
fn follows_the_stream_limit_the_api_names() {
    let seen = Arc::new(Mutex::new(Seen {
        slow: Some(Duration::from_millis(300)),
        streams_header: Some("2".into()),
        ..Seen::default()
    }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);

    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert_eq!(send_at_once(&p, 4), [200; 4]);
    assert_eq!(seen.lock().unwrap().most_open, 2, "the API's number, not the one built into pai");
}

#[test]
fn follows_the_context_limit_the_api_names() {
    let seen = Arc::new(Mutex::new(Seen { context_header: Some("65536".into()), ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);
    let env = p._t.env.clone();
    let settings = p._t.home().join(".claude").join("settings.json");
    let written = || -> Value { serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap() };
    let asked = || -> Value {
        p.http.get(format!("http://127.0.0.1:{}/pai/context", p.port)).bearer_auth(&p.key).send().unwrap().json().unwrap()
    };
    pai::setup::run(&env, pai::setup::Tool::ClaudeCode, &pai::setup::Target::resolve(&env, Some(pai_proto::Plan::Chat)), true).unwrap();
    assert_eq!(written()["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "131072");
    assert_eq!(asked()["context"], 131072, "before any header, the plan's limit built into pai");

    let r = p.http.post(format!("http://127.0.0.1:{}/v1/messages", p.port)).header("x-api-key", &p.key).body("{}").send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.headers().get("pai-context").is_none(), "the limit is for pai, not the coding tool");
    for _ in 0..100 {
        if written()["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"] == "65536" {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(written()["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "65536", "pai rewrote its own settings");
    assert_eq!(asked()["context"], 65536);
    assert_eq!(pai::setup::learned_context(&env), Some(65536));

    // A broken gateway can't write nonsense into the harness settings.
    for bad in ["100", "99999999", "lots"] {
        seen.lock().unwrap().context_header = Some(bad.into());
        assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(written()["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "65536");
    assert_eq!(asked()["context"], 65536);
}

#[test]
fn a_named_limit_of_zero_still_allows_one() {
    let seen = Arc::new(Mutex::new(Seen {
        slow: Some(Duration::from_millis(200)),
        streams_header: Some("0".into()),
        ..Seen::default()
    }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);

    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert_eq!(send_at_once(&p, 3), [200; 3]);
    assert_eq!(seen.lock().unwrap().most_open, 1);
}

/// Sends `n` stream requests at once and returns their statuses.
fn send_at_once(p: &Running, n: usize) -> Vec<u16> {
    let paths = ["/v1/messages", "/v1/chat/completions", "/v1/responses"];
    std::thread::scope(|scope| {
        let sent: Vec<_> = (0..n).map(|i| scope.spawn(move || p.post(paths[i % 3], Some(&p.key), &[]).0)).collect();
        sent.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn lets_requests_that_take_no_stream_past_the_queue() {
    let seen = Arc::new(Mutex::new(Seen { slow: Some(Duration::from_secs(2)), ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);
    // Collect the credential first, so the slow request below doesn't race the token count for it.
    p.post("/v1/models", Some(&p.key), &[]);

    std::thread::scope(|scope| {
        let slow = scope.spawn(|| p.post("/v1/messages", Some(&p.key), &[]).0);
        std::thread::sleep(Duration::from_millis(300));
        let started = std::time::Instant::now();
        assert_eq!(p.post("/v1/messages/count_tokens", Some(&p.key), &[]).0, 200);
        assert!(started.elapsed() < Duration::from_secs(1), "a token count doesn't wait for a stream");
        assert_eq!(slow.join().unwrap(), 200);
    });
}

#[test]
fn sends_again_when_the_api_refuses_for_its_stream_limit() {
    let seen = Arc::new(Mutex::new(Seen { refuse_streams: 2, ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, relay) = start(api_port, &ca, &pin);

    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200);
    assert_eq!(seen.lock().unwrap().requests.len(), 3);
    assert_eq!(relay.issued(), 1, "the same credential every time");

    // After 5 more tries the refusal goes to the coding tool, which has its own retries.
    seen.lock().unwrap().refuse_streams = 100;
    let (status, body) = p.post("/v1/messages", Some(&p.key), &[]);
    assert_eq!(status, 429);
    assert_eq!(body["error"]["type"], "stream_limit");
    assert_eq!(seen.lock().unwrap().requests.len(), 3 + 6);
}

#[test]
fn sends_again_after_an_error_before_the_answer() {
    let seen = Arc::new(Mutex::new(Seen { fail_first: 2, ..Seen::default() }));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);
    assert_eq!(p.post("/v1/messages", Some(&p.key), &[]).0, 200, "two 503s, then answered");
    assert_eq!(seen.lock().unwrap().requests.len(), 3);
    // A token count (no stream) is sent again too.
    seen.lock().unwrap().fail_first = 1;
    assert_eq!(p.post("/v1/messages/count_tokens", Some(&p.key), &[]).0, 200);
}

#[test]
fn test_mode_is_on_only_when_paired_with_it_and_says_so() {
    let relay = FakeRelay::start(CODE);
    let t = TestEnv::new();
    app::pair(&t.env, CODE, Some(&relay.url), None, None, None, None).unwrap();
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
    let err = app::pair(&t.env, CODE, Some(&relay.url), None, None, None, Some((ca.as_path(), &[]))).unwrap_err();
    assert!(err.to_string().contains("pins"), "{err}");
}

// `dawonai report`: pai takes the blocked text out of a refusal, keeps it, and sends it only when
// the person types the command, which never reaches the model.
#[test]
fn reports_a_refusal_when_the_person_types_dawonai_report() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (api_port, ca, pin) = fake_gateway(seen.clone());
    let (p, _relay) = start(api_port, &ca, &pin);
    let say = |text: &str| serde_json::json!({"max_tokens": 10, "messages": [{"role": "user", "content": text}]});

    let (status, body) = p.send("/v1/messages", say("dawonai report"));
    assert_eq!(status, 400);
    assert!(body.contains("report_unavailable"), "nothing to report yet: {body}");
    assert!(seen.lock().unwrap().requests.is_empty(), "the command never reaches the model API");

    // A refusal in a 400 body: the harness gets it without the piece.
    seen.lock().unwrap().refusal = 1;
    let (status, body) = p.send("/v1/messages", say("hello"));
    assert_eq!(status, 400);
    assert!(body.contains("blocked") && !body.contains("dawonai_report") && !body.contains("ged!!"), "{body}");
    assert!(seen.lock().unwrap().report_header.iter().all(|h| *h), "pai says it handles reports");

    let (status, body) = p.send("/v1/messages", serde_json::json!({"messages": [
        {"role": "user", "content": "hello"},
        {"role": "user", "content": [{"type": "text", "text": "<system-reminder>r</system-reminder>"}, {"type": "text", "text": "dawonai report"}]},
    ]}));
    assert_eq!(status, 400);
    assert!(body.contains("report_sent") && body.contains("9 bytes"), "{body}");
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.reports.len(), 1);
        let sent: Value = serde_json::from_str(&s.reports[0]).unwrap();
        assert_eq!(sent, serde_json::json!({"parts": ["flag", "ged!!"], "hour": 1, "tag": "ab"}));
        assert_eq!(s.requests.iter().filter(|r| r.0 == "/v1/messages").count(), 1, "only the refused request reached it");
    }
    // The same command again (a retry, a title request) gets the same answer, not a second report.
    let (_, body) = p.send("/v1/messages", say("dawonai report"));
    assert!(body.contains("report_sent"));
    assert_eq!(seen.lock().unwrap().reports.len(), 1);

    // A streamed refusal: the comment is taken out of the stream.
    seen.lock().unwrap().refusal = 2;
    let (status, body) = p.send("/v1/chat/completions", say("hello"));
    assert_eq!(status, 200);
    assert!(body.contains("blocked") && !body.contains("dawonai-report"), "{body}");
    let (_, body) = p.send("/v1/responses", serde_json::json!({"input": "dawonai report"}));
    assert!(body.contains("report_sent"));
    assert_eq!(seen.lock().unwrap().reports.len(), 2);
}
