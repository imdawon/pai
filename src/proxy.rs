//! The local proxy coding tools talk to. It listens on 127.0.0.1 only, accepts requests that
//! carry this machine's local key, adds the day credential, and forwards them to the model API
//! over TLS checked against `pai`'s pins. So coding tools never hold a credential, and nobody
//! holding a certificate for `api.<domain>`, the relay included, can read what they send.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use pai_proto::{Credential, Period, Plan, period_now};
use serde_json::json;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::app::{self, Env};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::pin;

pub const DEFAULT_PORT: u16 = 47821;
const MAX_BODY: usize = 32 * 1024 * 1024;
/// How long a request waits for one of the plan's streams before it gets the 429 the model API
/// would have given.
const STREAM_WAIT: Duration = Duration::from_secs(600);
/// How many times a request the model API refused for its stream limit is sent again. The API
/// can free a stream a moment after pai has.
const STREAM_RETRIES: u32 = 5;

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
    /// The plan's streams and how many there are, so requests over the limit wait here in the
    /// order they came instead of coming back from the model API as 429s.
    streams: Mutex<Option<(usize, Arc<Semaphore>)>>,
    stop: Notify,
    /// Requests still being answered, streamed replies included until their last byte.
    in_flight: Arc<AtomicUsize>,
    /// Set when the proxy stopped to make way for an updated `pai`.
    restart: AtomicBool,
    /// Woken when a response announced a newer release and an update was started.
    update_poke: Notify,
    /// A second handle on the listening socket, for the updated `pai` to take over.
    handover: Mutex<Option<Handover>>,
}

/// The listening socket a proxy hands to the updated `pai` that replaces it, so connections that
/// arrive during the restart wait in the socket's queue instead of being refused. Unix only:
/// Windows starts the new proxy as a new process, which binds the port again.
pub(crate) const LISTENER_FD: &str = "PAI_PROXY_LISTENER_FD";

#[cfg(unix)]
type Handover = std::os::fd::OwnedFd;
#[cfg(not(unix))]
type Handover = ();

/// A copy of the listening socket's handle, closed when this process starts another program.
#[cfg(unix)]
fn keep_for_handover(listener: &tokio::net::TcpListener) -> Option<Handover> {
    use std::os::fd::AsFd;
    listener.as_fd().try_clone_to_owned().ok()
}

#[cfg(not(unix))]
fn keep_for_handover(_: &tokio::net::TcpListener) -> Option<Handover> {
    None
}

/// The socket the proxy before this one handed over, if it's what it should be: a TCP socket
/// listening on 127.0.0.1 at this port. Anything else is left alone.
#[cfg(unix)]
fn handed_over(env: &Env, port: u16) -> Option<tokio::net::TcpListener> {
    use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
    let fd: RawFd = env.var(LISTENER_FD)?.parse().ok()?;
    if fd < 3 {
        return None;
    }
    // SAFETY: `fd` was handed to this process to own. If it isn't the socket we expect, it's
    // released again below without being closed.
    let std = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    // A socket at our address with no peer is the listening one, not a connection to it. A
    // descriptor that isn't a socket fails both.
    let has_peer = || {
        // SAFETY: getpeername only reads the descriptor and writes into `addr` and `len`.
        let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        unsafe { libc::getpeername(fd, (&raw mut addr).cast(), &mut len) == 0 }
    };
    let ours = std.local_addr().ok() == Some(std::net::SocketAddr::from(([127, 0, 0, 1], port))) && !has_peer();
    if !ours {
        let _ = std.into_raw_fd();
        return None;
    }
    // Programs this proxy starts, such as `pai update`, mustn't hold the port.
    // SAFETY: sets a flag on a descriptor this process owns.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    std.set_nonblocking(true).ok()?;
    tokio::net::TcpListener::from_std(std).ok()
}

#[cfg(not(unix))]
fn handed_over(_: &Env, _: u16) -> Option<tokio::net::TcpListener> {
    None
}

/// Counts a request as in flight until it's dropped.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn new(n: &Arc<AtomicUsize>) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        InFlight(n.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
    let restart = runtime.block_on(async move {
        let http = upstream_client(&env, &config)?;
        let port = port(&env);
        let listener = match handed_over(&env, port) {
            Some(l) => l,
            None => match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Ok(None),
                Err(e) => return Err(Error::Io(format!("couldn't listen on 127.0.0.1:{port}: {e}"))),
            },
        };
        let handover = keep_for_handover(&listener);
        let proxy = Arc::new(Proxy {
            api: config.api_url.clone(),
            env,
            port,
            local_key,
            fingerprint,
            http,
            credential: Mutex::new(None),
            streams: Mutex::new(None),
            stop: Notify::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            restart: AtomicBool::new(false),
            update_poke: Notify::new(),
            handover: Mutex::new(handover),
        });
        if proxy.env.prefetch {
            tokio::spawn(prefetch_loop(proxy.env.clone()));
        }
        tokio::spawn(update_loop(proxy.clone()));
        let app = Router::new().fallback(handle).with_state(proxy.clone());
        let stopping = proxy.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                stopping.stop.notified().await;
                // Told to stop, not restarting: let go of the port at once, so `pai token` can
                // start another proxy while answers still in flight finish here.
                if !stopping.restart.load(Ordering::SeqCst) {
                    stopping.handover.lock().unwrap().take();
                }
            })
            .await
            .map_err(|e| Error::Io(format!("the proxy stopped: {e}")))?;
        let restart = proxy.restart.load(Ordering::SeqCst);
        Ok::<_, Error>(restart.then(|| (proxy.env.clone(), proxy.handover.lock().unwrap().take())))
    })?;
    drop(runtime);
    match restart {
        Some((env, handover)) => restart_proxy(&env, handover),
        None => Ok(()),
    }
}

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Starts background update checks when they're due, for machines where the proxy runs for days
/// and `pai token` rarely does, and watches closely once a response announced a release and an
/// update was started. Once `pai` on disk has been replaced, waits for a moment with no request
/// in flight and stops, so `run` can start the new one in its place.
async fn update_loop(p: Arc<Proxy>) {
    let started_as = modified(&p.env.pai_bin);
    #[cfg(feature = "test-mode")]
    let every = p.env.var("PAI_TEST_UPDATE_POLL").and_then(|v| v.parse().ok()).map_or(Duration::from_secs(600), Duration::from_secs);
    #[cfg(not(feature = "test-mode"))]
    let every = Duration::from_secs(600);
    let replaced = || started_as.is_some() && modified(&p.env.pai_bin) != started_as;
    loop {
        let announced = tokio::select! {
            _ = tokio::time::sleep(every) => false,
            _ = p.update_poke.notified() => true,
        };
        if announced {
            // The download takes seconds; give it ten minutes before going back to the slow loop.
            let watch = every.min(Duration::from_secs(10));
            for _ in 0..60 {
                tokio::time::sleep(watch).await;
                if replaced() {
                    break;
                }
            }
        } else {
            let env = p.env.clone();
            let _ = tokio::task::spawn_blocking(move || crate::update::start_if_due(&env)).await;
        }
        if replaced() {
            break;
        }
    }
    while p.in_flight.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    p.restart.store(true, Ordering::SeqCst);
    p.stop.notify_one();
}

/// Replaces this proxy with the `pai` now on disk. On Unix it keeps the process ID, so a login
/// service never sees it exit, and the listening socket, so no connection is refused meanwhile.
fn restart_proxy(env: &Env, handover: Option<Handover>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::IntoRawFd;
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&env.pai_bin);
        cmd.arg("proxy").env_remove(LISTENER_FD);
        if let Some(fd) = handover {
            let fd = fd.into_raw_fd();
            // Kept open across exec only now, so no program started earlier holds the port.
            // SAFETY: clears a flag on a descriptor this process owns.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == 0 {
                cmd.env(LISTENER_FD, fd.to_string());
            }
        }
        let e = cmd.exec();
        Err(Error::Io(format!("couldn't start the updated pai: {e}")))
    }
    #[cfg(not(unix))]
    {
        let _ = handover;
        app::spawn_detached(env, &["proxy"]).map_err(|e| Error::Io(format!("couldn't start the updated pai: {e}")))
    }
}

/// Once a period (a day, or an hour on the 1-day trial), at a random time, collect the next
/// period's credential. The plan comes from the credential this computer already holds.
async fn prefetch_loop(env: Env) {
    loop {
        let now = env.clock.now();
        let plan = {
            let e = env.clone();
            tokio::task::spawn_blocking(move || app::held_plan(&e)).await.ok().flatten()
        };
        let Some(plan) = plan else {
            tokio::time::sleep(Duration::from_secs(300)).await;
            continue;
        };
        let period = Period::of(plan);
        if let Some(delay) = app::prefetch_delay(period, now) {
            let target = period_now(plan, now) + 1;
            tokio::time::sleep(delay).await;
            let e = env.clone();
            let _ = tokio::task::spawn_blocking(move || app::prefetch(&e, plan, target, Duration::ZERO)).await;
        }
        let now = env.clock.now();
        let until_next = period.secs() - now.rem_euclid(period.secs()) + 5;
        tokio::time::sleep(Duration::from_secs(until_next as u64)).await;
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

async fn handle(state: State<Arc<Proxy>>, req: Request) -> Response {
    let mut resp = route(state, req).await;
    // One request per connection, so a coding tool never sends on a kept-open connection that a
    // restarting proxy is closing. A new connection to 127.0.0.1 costs next to nothing.
    resp.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

async fn route(State(p): State<Arc<Proxy>>, req: Request) -> Response {
    let host =req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
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
        (_, path) if path.starts_with("/v1/") => {
            let guard = InFlight::new(&p.in_flight);
            let (parts, body) = forward(p, req).await.into_parts();
            let body = body.into_data_stream().map(move |chunk| {
                let _ = &guard;
                chunk
            });
            Response::from_parts(parts, Body::from_stream(body))
        }
        _ => error(StatusCode::NOT_FOUND, "not_found_error", "pai's proxy only forwards the model API, under /v1/."),
    }
}

impl Proxy {
    async fn credential(&self, refresh: bool) -> std::result::Result<String, Response> {
        let now = self.env.clock.now();
        let current = |c: &str| Credential::decode(c).is_ok_and(|c| c.day == period_now(c.plan, now));
        if !refresh && let Some((_, c)) = self.credential.lock().unwrap().clone() && current(&c) {
            return Ok(c);
        }
        let env = self.env.clone();
        let got = tokio::task::spawn_blocking(move || app::credential(&env, refresh))
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "pai's proxy hit an internal error."))?;
        match got {
            Ok(c) => {
                *self.credential.lock().unwrap() = Some((0, c.clone()));
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

    /// Waits for one of the plan's streams, read from the credential this computer holds.
    async fn stream_slot(&self) -> std::result::Result<OwnedSemaphorePermit, Response> {
        let credential = self.credential(false).await?;
        let max = Credential::decode(&credential).map_or(Plan::Chat, |c| c.plan).max_streams();
        let streams = {
            let mut s = self.streams.lock().unwrap();
            match &*s {
                Some((n, streams)) if *n == max => streams.clone(),
                // A new plan starts a new count. Streams still open under the old one finish on
                // their own, and the model API's 429 covers any overlap.
                _ => s.insert((max, Arc::new(Semaphore::new(max)))).1.clone(),
            }
        };
        match tokio::time::timeout(STREAM_WAIT, streams.acquire_owned()).await {
            Ok(Ok(slot)) => Ok(slot),
            _ => {
                let noun = if max == 1 { "request" } else { "requests" };
                let mut r = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "stream_limit",
                    format!("Your plan allows {max} {noun} at a time, and that many have been running for 10 minutes. Try again when one finishes."),
                );
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
                Err(r)
            }
        }
    }
}

/// The requests that hold one of the plan's streams on the model API until their answer ends.
fn takes_stream(method: &Method, path: &str) -> bool {
    method == Method::POST && matches!(path, "/v1/chat/completions" | "/v1/messages" | "/v1/responses")
}

/// Keeps a stream slot until the response's last byte, or until the coding tool drops it.
fn hold(resp: Response, slot: Option<OwnedSemaphorePermit>) -> Response {
    let Some(slot) = slot else { return resp };
    let (parts, body) = resp.into_parts();
    let body = body.into_data_stream().map(move |chunk| {
        let _ = &slot;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(body))
}

fn retry_after(headers: &HeaderMap) -> Duration {
    let secs = headers.get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
    Duration::from_secs(secs.unwrap_or(2).min(5))
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
    !DROPPED.contains(name) && name.as_str() != "x-api-key" && name.as_str() != "keep-alive" && name.as_str() != LATEST
}

/// The model API names the latest pai release on every response, so a proxy hears of one on the
/// next request rather than at the next daily check. Meant for the proxy, not the coding tool.
const LATEST: &str = "pai-latest";

fn note_announced_release(p: &Arc<Proxy>, headers: &HeaderMap) {
    let Some(announced) = headers.get(LATEST).and_then(|v| v.to_str().ok()) else { return };
    let announced = announced.trim().to_string();
    if !crate::update::newer(&announced, crate::update::VERSION) {
        return;
    }
    let p = p.clone();
    tokio::task::spawn_blocking(move || {
        if crate::update::start_for_announced(&p.env, &announced) {
            p.update_poke.notify_one();
        }
    });
}

async fn forward(p: Arc<Proxy>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // The body is read only once the request has a stream, so a waiting request holds nothing
    // but its place in line.
    let slot = if takes_stream(&parts.method, parts.uri.path()) {
        match p.stream_slot().await {
            Ok(s) => Some(s),
            Err(r) => return r,
        }
    } else {
        None
    };
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

    // A refused credential is replaced once; a stream-limit refusal is sent again a few times.
    let (mut refresh, mut refreshed, mut refusals) = (false, false, 0);
    loop {
        let credential = match p.credential(std::mem::take(&mut refresh)).await {
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
        note_announced_release(&p, resp.headers());
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let may_retry = (status == StatusCode::UNAUTHORIZED && !refreshed)
            || (status == StatusCode::TOO_MANY_REQUESTS && slot.is_some() && refusals < STREAM_RETRIES);
        if !may_retry {
            let stream = resp.bytes_stream().map_err(std::io::Error::other);
            return hold(respond(status, &resp_headers, Body::from_stream(stream)), slot);
        }
        let text = resp.bytes().await.unwrap_or_default();
        let kind = String::from_utf8_lossy(&text);
        if status == StatusCode::UNAUTHORIZED && kind.contains("invalid_credential") {
            (refresh, refreshed) = (true, true);
            continue;
        }
        if status == StatusCode::TOO_MANY_REQUESTS && kind.contains("stream_limit") {
            refusals += 1;
            tokio::time::sleep(retry_after(&resp_headers)).await;
            continue;
        }
        return hold(respond(status, &resp_headers, Body::from(text)), slot);
    }
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
