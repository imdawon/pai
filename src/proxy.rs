//! The local proxy coding tools talk to. It listens on 127.0.0.1 only, accepts requests that
//! carry this machine's local key, adds the day credential, and forwards them to the model API
//! over TLS checked against `pai`'s pins. So coding tools never hold a credential, and nobody
//! holding a certificate for `api.<domain>`, the relay included, can read what they send.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use pai_proto::day_number;
use serde_json::json;
use tokio::sync::Notify;

use crate::app::{self, Env};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::pin;

pub const DEFAULT_PORT: u16 = 47821;
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn port(env: &Env) -> u16 {
    env.var("PAI_PROXY_PORT").and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// The base URL coding tools are set up with.
pub fn url(env: &Env) -> String {
    format!("http://127.0.0.1:{}", port(env))
}

/// What a running proxy reports at `/pai/health`, so `pai token` can tell whether it was started
/// by this version of `pai` with this pairing, or needs replacing.
pub fn fingerprint(config: &Config, trust: &app::Trust) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(env!("CARGO_PKG_VERSION"));
    // A rebuilt or reinstalled binary replaces a proxy started by the old one.
    if let Some(t) = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok()) {
        h.update(format!("{t:?}"));
    }
    for part in [&config.api_url, &config.device_id, &config.relay_url] {
        h.update([0]);
        h.update(part);
    }
    for p in &trust.api_pins {
        h.update([1]);
        h.update(p);
    }
    if let Some(ca) = &trust.test_ca {
        h.update([2]);
        h.update(ca.to_string_lossy().as_bytes());
    }
    pai_proto::hex(h.finalize())
}

/// The model API client: pinned TLS for https, plain HTTP for a local test gateway.
pub fn upstream_client(env: &Env, config: &Config) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("pai/", env!("CARGO_PKG_VERSION")));
    let builder = if config.api_url.starts_with("https://") {
        let trust = app::trust(env, Some(config));
        builder.use_preconfigured_tls(pin::client_config(&trust.api_pins, trust.test_ca.as_deref())?)
    } else if config.api_url.starts_with("http://") {
        builder
    } else {
        return Err(Error::Setup(format!("the model API URL {} must start with https://", config.api_url)));
    };
    builder.build().map_err(|e| Error::Setup(format!("couldn't set up the connection to the model API: {e}")))
}

struct Proxy {
    env: Env,
    api: String,
    port: u16,
    local_key: String,
    fingerprint: String,
    http: reqwest::Client,
    credential: Mutex<Option<(u32, String)>>,
    stop: Notify,
}

/// Runs the proxy until it's told to stop. Returns at once if another proxy already has the port.
pub fn run(env: Env) -> Result<()> {
    let config = env.config()?.ok_or(Error::NotPaired)?;
    let local_key = app::local_key(&env)?;
    let fingerprint = fingerprint(&config, &app::trust(&env, Some(&config)));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .map_err(|e| Error::Io(format!("couldn't start the proxy: {e}")))?;
    runtime.block_on(async move {
        let http = upstream_client(&env, &config)?;
        let port = port(&env);
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Ok(()),
            Err(e) => return Err(Error::Io(format!("couldn't listen on 127.0.0.1:{port}: {e}"))),
        };
        let proxy = Arc::new(Proxy {
            api: config.api_url.clone(),
            env,
            port,
            local_key,
            fingerprint,
            http,
            credential: Mutex::new(None),
            stop: Notify::new(),
        });
        if proxy.env.prefetch {
            tokio::spawn(prefetch_daily(proxy.env.clone()));
        }
        let app = Router::new().fallback(handle).with_state(proxy.clone());
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { proxy.stop.notified().await })
            .await
            .map_err(|e| Error::Io(format!("the proxy stopped: {e}")))
    })
}

/// Once a day, at a random time, collect tomorrow's credential.
async fn prefetch_daily(env: Env) {
    loop {
        let now = env.clock.now();
        if let Some(delay) = app::prefetch_delay(now) {
            let tomorrow = day_number(now) + 1;
            tokio::time::sleep(delay).await;
            let e = env.clone();
            let _ = tokio::task::spawn_blocking(move || app::prefetch(&e, tomorrow, Duration::ZERO)).await;
        }
        let now = env.clock.now();
        let until_tomorrow = 86_400 - now.rem_euclid(86_400) + 5;
        tokio::time::sleep(Duration::from_secs(until_tomorrow as u64)).await;
    }
}

fn error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    // Both shapes at once: Anthropic clients read error.type and error.message under a top-level
    // "type": "error"; OpenAI clients read error.message.
    let body = json!({"type": "error", "error": {"type": kind, "message": message.into()}});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = auth.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handle(State(p): State<Arc<Proxy>>, req: Request) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if host != format!("127.0.0.1:{}", p.port) && host != format!("localhost:{}", p.port) {
        // A web page that rebinds its own name to 127.0.0.1 arrives with its own host name.
        return error(StatusCode::FORBIDDEN, "permission_error", format!("pai's proxy only answers at 127.0.0.1:{}.", p.port));
    }
    if req.headers().contains_key(header::ORIGIN) {
        return error(StatusCode::FORBIDDEN, "permission_error", "pai's proxy doesn't answer web pages.");
    }
    if !presented_key(req.headers()).is_some_and(|k| same(k, &p.local_key)) {
        return error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "This isn't pai's local key. Set the tool's API key command to `pai token`, or run `pai setup <tool> --apply` again.",
        );
    }
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/pai/health") => {
            ([(header::CONTENT_TYPE, "application/json")], json!({"pai": p.fingerprint}).to_string()).into_response()
        }
        (&Method::POST, "/pai/stop") => {
            p.stop.notify_one();
            StatusCode::NO_CONTENT.into_response()
        }
        (_, path) if path.starts_with("/v1/") => forward(p, req).await,
        _ => error(StatusCode::NOT_FOUND, "not_found_error", "pai's proxy only forwards the model API, under /v1/."),
    }
}

impl Proxy {
    async fn credential(&self, refresh: bool) -> std::result::Result<String, Response> {
        let today = day_number(self.env.clock.now());
        if !refresh && let Some((day, c)) = self.credential.lock().unwrap().clone() && day == today {
            return Ok(c);
        }
        let env = self.env.clone();
        let got = tokio::task::spawn_blocking(move || app::credential(&env, refresh))
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "pai's proxy hit an internal error."))?;
        match got {
            Ok(c) => {
                *self.credential.lock().unwrap() = Some((today, c.clone()));
                Ok(c)
            }
            Err(e) => {
                let status = match e {
                    Error::NotPaired | Error::DeviceRemoved => StatusCode::UNAUTHORIZED,
                    Error::NotPaid { .. } | Error::Banned => StatusCode::PAYMENT_REQUIRED,
                    Error::IssuanceLimit { .. } => StatusCode::TOO_MANY_REQUESTS,
                    _ => StatusCode::SERVICE_UNAVAILABLE,
                };
                if matches!(e, Error::NotPaired) {
                    self.stop.notify_one();
                }
                Err(error(status, "authentication_error", format!("pai couldn't get today's credential: {e}.")))
            }
        }
    }
}

const DROPPED: [HeaderName; 9] = [
    header::HOST,
    header::AUTHORIZATION,
    header::CONNECTION,
    header::CONTENT_LENGTH,
    header::TRANSFER_ENCODING,
    header::TE,
    header::UPGRADE,
    header::COOKIE,
    header::PROXY_AUTHORIZATION,
];

fn forwardable(name: &HeaderName) -> bool {
    !DROPPED.contains(name) && name.as_str() != "x-api-key" && name.as_str() != "keep-alive"
}

async fn forward(p: Arc<Proxy>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body: Bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "The request is larger than 32 MB."),
    };
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if forwardable(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{path}", p.api);

    for attempt in 0..2 {
        let credential = match p.credential(attempt == 1).await {
            Ok(c) => c,
            Err(r) => return r,
        };
        let sent = p
            .http
            .request(parts.method.clone(), &url)
            .headers(headers.clone())
            .bearer_auth(&credential)
            .body(body.clone())
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) => return unreachable(&p.api, &e),
        };
        if resp.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
            let status = resp.status();
            let resp_headers = resp.headers().clone();
            let text = resp.bytes().await.unwrap_or_default();
            if String::from_utf8_lossy(&text).contains("invalid_credential") {
                continue;
            }
            return respond(status, &resp_headers, Body::from(text));
        }
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        return respond(status, &resp_headers, Body::from_stream(stream));
    }
    error(StatusCode::UNAUTHORIZED, "authentication_error", "The model API refused a fresh credential.")
}

fn respond(status: StatusCode, headers: &HeaderMap, body: Body) -> Response {
    let mut out = Response::new(body);
    *out.status_mut() = status;
    for (name, value) in headers {
        if forwardable(name) {
            out.headers_mut().append(name.clone(), value.clone());
        }
    }
    out.headers_mut().insert("x-pai-proxy", HeaderValue::from_static("1"));
    out
}

fn unreachable(api: &str, e: &reqwest::Error) -> Response {
    let mut source: Option<&dyn std::error::Error> = Some(e);
    let mut innermost = e.to_string();
    while let Some(s) = source {
        innermost = s.to_string();
        if innermost.contains(pin::PIN_MISMATCH) {
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!(
                    "{api} showed a certificate whose key isn't one pai trusts, so pai sent nothing. \
                     Something between you and the box may be intercepting the connection. If it keeps happening, \
                     update pai and tell us."
                ),
            );
        }
        source = s.source();
    }
    error(
        StatusCode::BAD_GATEWAY,
        "api_error",
        format!("pai couldn't reach the model API at {api} ({innermost}). Check your connection and try again."),
    )
}
