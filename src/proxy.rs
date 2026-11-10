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
use crate::report;

pub const DEFAULT_PORT: u16 = 47821;
const MAX_BODY: usize = 32 * 1024 * 1024;
/// How many times a request is sent again after an error that came before any of the answer: no
/// connection, a 429 (the API frees a stream a moment after pai does, or it's busy), or a 500,
/// 502, 503 or 504. Pauses start at 1 s and double, up to 30 s, each with up to half again added
/// at random so many devices don't retry in step.
const RETRIES: u32 = 5;

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
    /// The plan's streams and how many there are, so requests over the limit wait here in the
    /// order they came instead of coming back from the model API as 429s.
    streams: Mutex<Option<(usize, Arc<Semaphore>)>>,
    /// The stream limit the model API last named in `pai-streams`, and the plan it was for.
    learned_streams: Mutex<Option<(Plan, usize)>>,
    /// The context limit the model API last named in `pai-context`.
    learned_context: Mutex<Option<u32>>,
    stop: Notify,
    /// Requests still being answered, streamed replies included until their last byte.
    in_flight: Arc<AtomicUsize>,
    /// Set when the proxy stopped to make way for an updated `pai`.
    restart: AtomicBool,
    /// Woken when a response announced a newer release and an update was started.
    update_poke: Notify,
    /// A second handle on the listening socket, for the updated `pai` to take over.
    handover: Mutex<Option<Handover>>,
    /// The grants time last checked against (`pai-grants`), and whether a check is waiting to run.
    grants: Mutex<(Option<i64>, bool)>,
    /// The latest refusal's flagged piece, for `dawonai report`. In memory only.
    report: Mutex<Option<report::Held>>,
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
    raise_file_limit();
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
        let env_for_grants = env.clone();
        let proxy = Arc::new(Proxy {
            api: config.api_url.clone(),
            env,
            port,
            local_key,
            fingerprint,
            http,
            streams: Mutex::new(None),
            learned_streams: Mutex::new(None),
            learned_context: Mutex::new(crate::setup::learned_context(&env_for_grants)),
            stop: Notify::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            restart: AtomicBool::new(false),
            update_poke: Notify::new(),
            handover: Mutex::new(handover),
            grants: Mutex::new((app::checked_grants(&env_for_grants), false)),
            report: Mutex::new(None),
        });
        if proxy.env.prefetch {
            tokio::spawn(prefetch_loop(proxy.env.clone()));
        }
        tokio::spawn(update_loop(proxy.clone()));
        tokio::spawn(drop_plain_copy_later(proxy.env.clone()));
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

/// How long a new proxy waits before removing the plain secrets file a pai before 0.1.22 left: the
/// proxy it replaced reads only that file, and may still be finishing answers. privacy-ai
/// docs/SETTINGS.md, "Plain secrets file kept after sealing".
const PLAIN_COPY_GRACE: Duration = Duration::from_secs(600);

async fn drop_plain_copy_later(env: Env) {
    #[cfg(feature = "test-mode")]
    let grace = env.var("PAI_TEST_PLAIN_COPY_GRACE").and_then(|v| v.parse().ok()).map_or(PLAIN_COPY_GRACE, Duration::from_secs);
    #[cfg(not(feature = "test-mode"))]
    let grace = PLAIN_COPY_GRACE;
    tokio::time::sleep(grace).await;
    let _ = tokio::task::spawn_blocking(move || app::drop_plain_copy(&env)).await;
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

/// An error pai makes itself. It's sent in the Anthropic API's shape; `handle` reshapes it for a
/// tool that called an OpenAI endpoint (`openai_error`).
#[derive(Clone)]
struct PaiError {
    kind: String,
    message: String,
}

/// pai's own name for an error, beside `error`, as the gateway sends `dawonai_error`.
const KIND_FIELD: &str = "dawonai_error";

fn error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    let class = match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    let body = json!({"type": "error", "error": {"type": class, "message": message}, KIND_FIELD: kind});
    let mut resp = (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response();
    resp.extensions_mut().insert(PaiError { kind: kind.to_string(), message });
    resp
}

/// The same error in the OpenAI API's shape: its class in `type`, its case in `code`.
fn openai_error(status: StatusCode, e: &PaiError) -> Response {
    let (class, code) = match status.as_u16() {
        401 => ("invalid_request_error", Some("invalid_api_key")),
        404 => ("invalid_request_error", Some("unknown_url")),
        429 => ("requests", Some("rate_limit_exceeded")),
        400..=499 => ("invalid_request_error", None),
        _ => ("server_error", None),
    };
    let body = json!({"error": {"message": e.message, "type": class, "param": null, "code": code}, KIND_FIELD: e.kind});
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// Whether the tool called the Anthropic API (`/v1/messages`) rather than an OpenAI one.
fn anthropic_api(req: &Request) -> bool {
    req.uri().path().starts_with("/v1/messages") || req.headers().contains_key("anthropic-version")
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
    let anthropic = anthropic_api(&req);
    let mut resp = route(state, req).await;
    if !anthropic && let Some(e) = resp.extensions().get::<PaiError>().cloned() {
        let headers = std::mem::take(resp.headers_mut());
        let status = resp.status();
        resp = openai_error(status, &e);
        for (name, value) in &headers {
            if name != header::CONTENT_LENGTH && name != header::CONTENT_TYPE {
                resp.headers_mut().insert(name.clone(), value.clone());
            }
        }
    }
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
        // The OpenCode and OMP plugins ask for the limit when they load.
        (&Method::GET, "/pai/context") => {
            let learned = *p.learned_context.lock().unwrap();
            let context = match learned {
                Some(n) => n,
                None => {
                    let env = p.env.clone();
                    tokio::task::spawn_blocking(move || crate::setup::context_for(&env, app::held_plan(&env)).0)
                        .await
                        .unwrap_or(Plan::Chat.max_context() as u32)
                }
            };
            ([(header::CONTENT_TYPE, "application/json")], json!({"context": context}).to_string()).into_response()
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
    // The error is the response sent straight back to the coding harness.
    #[allow(clippy::result_large_err)]
    async fn credential(&self, refresh: bool) -> std::result::Result<String, Response> {
        // Read from pai's store on every request, not kept in memory: `pai status` and the daily
        // collection replace a held pass when the account's adult content setting changes.
        let env = self.env.clone();
        let got = tokio::task::spawn_blocking(move || app::credential(&env, refresh))
            .await
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "pai's proxy hit an internal error."))?;
        match got {
            Ok(c) => Ok(c),
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

    /// Waits for one of the plan's streams: the number the model API last named for this plan, or
    /// before it has named one, the number built into pai.
    // The error is the response sent straight back to the coding harness.
    #[allow(clippy::result_large_err)]
    async fn stream_slot(&self) -> std::result::Result<OwnedSemaphorePermit, Response> {
        let credential = self.credential(false).await?;
        let plan = Credential::decode(&credential).map_or(Plan::Chat, |c| c.plan);
        let max = match *self.learned_streams.lock().unwrap() {
            Some((p, n)) if p == plan => n,
            _ => plan.max_streams(),
        };
        let streams = {
            let mut s = self.streams.lock().unwrap();
            match &*s {
                Some((n, streams)) if *n == max => streams.clone(),
                // A new plan starts a new count. Streams still open under the old one finish on
                // their own, and the model API's 429 covers any overlap.
                _ => s.insert((max, Arc::new(Semaphore::new(max)))).1.clone(),
            }
        };
        // First come, first served, with no time limit: Tokio's semaphore serves waiters in the
        // order they came, and a request keeps its place until it has a stream or its coding tool
        // leaves. (Before 0.1.24 a request that had waited 10 minutes rejoined at the back, behind
        // newer requests, and could wait for an hour; INCIDENTS.md, 2026-11-08.)
        streams.acquire_owned().await.map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "stream_limit", "pai's queue closed. Try again."))
    }
}

/// A random wait from zero to `max`.
fn jitter(max: Duration) -> Duration {
    max.mul_f64(rand::random::<f64>())
}

/// The pause before retry `attempt` (0 for the first): `first` (1 s) doubling to 30 times it, plus
/// up to half again.
fn backoff(attempt: u32, first: Duration) -> Duration {
    let base = (first * (1u32 << attempt.min(5))).min(first * 30);
    base + jitter(base / 2)
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
    !DROPPED.contains(name)
        && !matches!(name.as_str(), "x-api-key" | "keep-alive" | LATEST | STREAMS | CONTEXT | GRANTS)
}

/// The model API names the latest pai release on every response, so a proxy hears of one on the
/// next request rather than at the next daily check. Meant for the proxy, not the coding tool.
const LATEST: &str = "pai-latest";

/// The model API names the plan's stream limit on responses to stream requests, so the local queue
/// follows the gateway rather than the number built into pai. Meant for the proxy, not the coding
/// tool.
const STREAMS: &str = "pai-streams";

/// Most streams pai will take a `pai-streams` header to allow. The gateway enforces its own limit
/// either way; this only bounds the local queue.
const MAX_LEARNED_STREAMS: usize = 32;

fn note_stream_limit(p: &Proxy, headers: &HeaderMap, credential: &str) {
    let Some(n) = headers.get(STREAMS).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<usize>().ok()) else {
        return;
    };
    let Ok(c) = Credential::decode(credential) else { return };
    *p.learned_streams.lock().unwrap() = Some((c.plan, n.clamp(1, MAX_LEARNED_STREAMS)));
}

/// The model API names the plan's context limit beside `pai-streams`. When it changes, pai rewrites
/// the settings `pai setup` wrote, so each harness uses it the next time it starts. Values outside
/// 8K to 1M are ignored. Meant for the proxy, not the coding tool.
const CONTEXT: &str = "pai-context";

fn note_context_limit(p: &Arc<Proxy>, headers: &HeaderMap) {
    let Some(n) = headers.get(CONTEXT).and_then(|v| v.to_str().ok()).and_then(crate::setup::parse_context) else {
        return;
    };
    {
        let mut learned = p.learned_context.lock().unwrap();
        if *learned == Some(n) {
            return;
        }
        *learned = Some(n);
    }
    let p = p.clone();
    tokio::task::spawn_blocking(move || {
        if crate::setup::save_learned_context(&p.env, n).is_err() {
            // The next response tries again.
            *p.learned_context.lock().unwrap() = None;
            return;
        }
        let _ = crate::setup::refresh(&p.env, app::held_plan(&p.env));
    });
}

/// The model API repeats the relay's grants time on every response. When it differs from the one
/// this computer last checked, the account's adult content setting may have changed, so pai checks
/// its account and replaces passes that carry the old setting. Meant for the proxy, not the tool.
const GRANTS: &str = pai_proto::GRANTS_HEADER;

/// The longest pai waits, at random, before that check, so the relay can't match the check to
/// the request that announced the change.
const GRANTS_MAX_DELAY: Duration = Duration::from_secs(180);

fn note_grants(p: &Arc<Proxy>, headers: &HeaderMap) {
    let Some(grants) = headers.get(GRANTS).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok()) else {
        return;
    };
    {
        let mut g = p.grants.lock().unwrap();
        if g.1 || g.0 == Some(grants) {
            return;
        }
        g.1 = true;
    }
    let max = p.env.var("PAI_TEST_GRANTS_DELAY_MS").and_then(|v| v.parse().ok()).map_or(GRANTS_MAX_DELAY, Duration::from_millis);
    let delay = Duration::from_millis(rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..=max.as_millis() as u64));
    let p = p.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let env = p.env.clone();
        let done = tokio::task::spawn_blocking(move || app::recheck_grants(&env, grants)).await;
        let mut g = p.grants.lock().unwrap();
        // On failure the next response tries again.
        if matches!(done, Ok(Ok(_))) {
            g.0 = Some(grants);
        }
        g.1 = false;
    });
}

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
    // The person typed `dawonai report`: this request never goes to the model.
    if parts.method == Method::POST && report::is_report_request(&body) {
        return hold(send_report(&p).await, slot);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if forwardable(name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(report::HEADER, HeaderValue::from_static("1"));
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{path}", p.api);

    // A refused credential is replaced once; an error before any of the answer is sent again a
    // few times.
    let (mut refresh, mut refreshed, mut attempts) = (false, false, 0u32);
    let first = p.env.var("PAI_TEST_RETRY_MS").and_then(|v| v.parse().ok()).map_or(Duration::from_secs(1), Duration::from_millis);
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
            Err(_) if attempts < RETRIES => {
                tokio::time::sleep(backoff(attempts, first)).await;
                attempts += 1;
                continue;
            }
            Err(e) => return unreachable(&p.api, &e),
        };
        note_announced_release(&p, resp.headers());
        note_grants(&p, resp.headers());
        note_stream_limit(&p, resp.headers(), &credential);
        note_context_limit(&p, resp.headers());
        let status = resp.status();
        let resp_headers = resp.headers().clone();
        let transient = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504 | 529);
        let may_retry = (status == StatusCode::UNAUTHORIZED && !refreshed) || (transient && attempts < RETRIES);
        if !may_retry {
            // A refusal carries its flagged piece for `dawonai report`; the harness never sees it.
            if status == StatusCode::BAD_REQUEST {
                let (text, rest) = read_capped(resp).await;
                let mut headers = resp_headers.clone();
                headers.remove(header::CONTENT_LENGTH);
                if rest.is_some() {
                    return hold(respond(status, &headers, capped_body(text, rest)), slot);
                }
                let (stripped, held) = report::take_from_body(&text);
                if let Some(payload) = held {
                    *p.report.lock().unwrap() = Some(report::Held { payload, sent: None });
                }
                let body = stripped.map_or_else(|| Body::from(text), Body::from);
                return hold(respond(status, &headers, body), slot);
            }
            // A streamed reply that breaks off ends with the API's own error event, so the harness
            // shows an error instead of a dropped connection. Other replies just stop.
            let event_stream = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("text/event-stream"));
            let broken_off = event_stream.then(|| stream_error_event(parts.uri.path()));
            let filter = Arc::new(Mutex::new(report::SseFilter::default()));
            let (keep, last) = (p.clone(), filter.clone());
            let stream = resp
                .bytes_stream()
                .map(move |chunk| {
                    let mut f = filter.lock().unwrap();
                    let chunk = match (chunk, &broken_off) {
                        (Ok(c), _) => c,
                        (Err(_), Some(event)) => return Ok(Bytes::from(f.fail(event))),
                        (Err(e), None) => return Err(std::io::Error::other(e)),
                    };
                    let out = Bytes::from(f.push(&chunk));
                    if let Some(payload) = f.report.take() {
                        *keep.report.lock().unwrap() = Some(report::Held { payload, sent: None });
                    }
                    Ok(out)
                })
                .chain(futures_util::stream::once(async move { Ok(Bytes::from(last.lock().unwrap().finish())) }));
            let mut headers = resp_headers.clone();
            headers.remove(header::CONTENT_LENGTH);
            return hold(respond(status, &headers, Body::from_stream(stream)), slot);
        }
        let (text, rest) = read_capped(resp).await;
        let kind = String::from_utf8_lossy(&text).into_owned();
        if status == StatusCode::UNAUTHORIZED && kind.contains("invalid_credential") {
            (refresh, refreshed) = (true, true);
            continue;
        }
        if status == StatusCode::UNAUTHORIZED {
            return hold(respond(status, &resp_headers, capped_body(text, rest)), slot);
        }
        // A refusal the safety check gave on purpose won't change: pass it on.
        if kind.contains("safety_unavailable") && status == StatusCode::SERVICE_UNAVAILABLE && attempts > 0 {
            return hold(respond(status, &resp_headers, capped_body(text, rest)), slot);
        }
        tokio::time::sleep(retry_after(&resp_headers).max(backoff(attempts, first))).await;
        attempts += 1;
    }
}

/// The most of an error reply pai reads into memory, to look at its kind and take out a report.
/// Real ones are well under a kilobyte; the rest of a longer one is passed on as it arrives.
const MAX_ERROR_BODY: usize = 1 << 20;

/// Reads a reply up to `MAX_ERROR_BODY`. When there's more, returns the reply too, for the rest.
async fn read_capped(mut resp: reqwest::Response) -> (Bytes, Option<reqwest::Response>) {
    let mut read = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                read.extend_from_slice(&chunk);
                if read.len() > MAX_ERROR_BODY {
                    return (read.into(), Some(resp));
                }
            }
            _ => return (read.into(), None),
        }
    }
}

/// The body for a reply `read_capped` read: what it read, then the rest as it arrives.
fn capped_body(read: Bytes, rest: Option<reqwest::Response>) -> Body {
    match rest {
        None => Body::from(read),
        Some(rest) => Body::from_stream(futures_util::stream::once(async move { Ok(read) }).chain(rest.bytes_stream().map_err(std::io::Error::other))),
    }
}

/// The error event that ends a streamed reply the model server broke off, in the shape of the API
/// the harness called: Anthropic's `error` event, the Responses API's `error` event, or the error
/// object Chat Completions streams send. pai's own name for it goes beside, as `dawonai_error`.
fn stream_error_event(path: &str) -> Vec<u8> {
    const MESSAGE: &str = "DawonAI's model server stopped sending this reply before it finished. Try again.";
    let (event, data) = if path.starts_with("/v1/messages") {
        (Some("error"), json!({"type": "error", "error": {"type": "api_error", "message": MESSAGE}, KIND_FIELD: "stream_interrupted"}))
    } else if path.starts_with("/v1/responses") {
        (Some("error"), json!({"type": "error", "code": "server_error", "message": MESSAGE, "param": null, KIND_FIELD: "stream_interrupted"}))
    } else {
        (None, json!({"error": {"message": MESSAGE, "type": "server_error", "param": null, "code": null}, KIND_FIELD: "stream_interrupted"}))
    };
    match event {
        Some(e) => format!("event: {e}\ndata: {data}\n\n").into_bytes(),
        None => format!("data: {data}\n\n").into_bytes(),
    }
}

/// Raises the open-file limit to the most the system allows. Each waiting request holds a
/// connection, and Linux's usual soft limit of 1,024 left the proxy unable to accept any more,
/// its health check included, until some closed.
#[cfg(unix)]
fn raise_file_limit() {
    // SAFETY: getrlimit and setrlimit only read and write the struct passed to them.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 || limit.rlim_cur >= limit.rlim_max {
            return;
        }
        let mut want = limit.rlim_max;
        // macOS refuses a soft limit above OPEN_MAX, even when the hard limit is unlimited.
        if cfg!(target_os = "macos") {
            want = want.min(10240);
        }
        if want > limit.rlim_cur {
            limit.rlim_cur = want;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

#[cfg(not(unix))]
fn raise_file_limit() {}

/// Answers `dawonai report`: sends the latest refusal's flagged piece to the model API with the
/// day pass, and tells the harness so in an error, the way it showed the refusal.
async fn send_report(p: &Arc<Proxy>) -> Response {
    let held = p.report.lock().unwrap().clone();
    let Some(held) = held else {
        return error(
            StatusCode::BAD_REQUEST,
            "report_unavailable",
            "pai has no blocked request to report. It keeps only the most recent one, until pai restarts. This message wasn't sent to the model.",
        );
    };
    let sent = |n: usize| {
        error(
            StatusCode::BAD_REQUEST,
            "report_sent",
            format!("Sent the blocked text ({n} bytes) to DawonAI. Thank you: we'll review it to improve the classifier. This message wasn't sent to the model."),
        )
    };
    if held.sent.is_some_and(|at| at.elapsed() < report::SENT_ANSWER_FOR) {
        return sent(held.bytes());
    }
    let credential = match p.credential(false).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let url = format!("{}/v1/safety/report", p.api);
    match p.http.post(&url).bearer_auth(&credential).json(&held.payload).send().await {
        Ok(r) if r.status().is_success() => {
            if let Some(h) = p.report.lock().unwrap().as_mut() {
                h.sent = Some(std::time::Instant::now());
            }
            sent(held.bytes())
        }
        Ok(r) => {
            let status = r.status();
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            let message = v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("The model API didn't take the report.");
            error(status, "report_failed", format!("{message} This message wasn't sent to the model."))
        }
        Err(e) => unreachable(&p.api, &e),
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
