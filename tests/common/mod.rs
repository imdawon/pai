#![allow(dead_code, clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use pai::app::{Clock, Env};
use pai::config::Paths;
use pai::keystore::Keystore;
use pai::service::{Commands, Manager};
use pai_proto::{
    AccountView, ApiError, ApiErrorBody, CredentialRequest, CredentialResponse, DeviceAuth,
    DeviceKind, DeviceRegistered, IssuerKeyInfo, IssuerKeyPair, IssuerKeys, PairRequest, Plan, SharedPass, b64,
    day_number, now_unix, unb64,
};
use serde::Serialize;

pub const ACCOUNT_ID: &str = "acc_test";

pub fn today() -> u32 {
    day_number(now_unix())
}

/// RSA-2048 keygen is slow in debug builds, so every test shares the same issuer keys: the Chat
/// plan's for yesterday and today, its adult twin's for both, and the Agent plan's for today.
fn issuer_keys() -> &'static HashMap<(Plan, u32, bool), IssuerKeyPair> {
    static KEYS: OnceLock<HashMap<(Plan, u32, bool), IssuerKeyPair>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let today = today();
        [(Plan::Chat, today - 1, false), (Plan::Chat, today, false), (Plan::Chat, today - 1, true), (Plan::Chat, today, true), (Plan::Agent, today, false)]
            .into_iter()
            .map(|(plan, day, adult)| ((plan, day, adult), pai_proto::generate_issuer_key().unwrap()))
            .collect()
    })
}

pub fn issuer_public_key(plan: Plan, day: u32) -> &'static pai_proto::IssuerPublicKey {
    issuer_public_key_for(plan, day, false)
}

pub fn issuer_public_key_for(plan: Plan, day: u32, adult: bool) -> &'static pai_proto::IssuerPublicKey {
    &issuer_keys()[&(plan, day, adult)].pk
}

#[derive(Default)]
pub struct State {
    pub codes: HashSet<String>,
    pub devices: HashMap<String, Vec<u8>>,
    pub next_device: u32,
    pub credential_plan: Option<Plan>,
    pub status: String,
    /// The account's adult content setting, which the operator can change at any time.
    pub adult: bool,
    /// The relay-wide rotate time (`pai-relay rotate`).
    pub rotate: i64,
    pub issued: u32,
    pub requests: Vec<String>,
    pub seen_signatures: HashSet<Vec<u8>>,
    /// Pretend the next signed write was already seen, as the relay does for a same-second repeat.
    pub replay_next_write: bool,
    /// One day pass per account is in force (`PAI_SHARED_PASS_FROM` has passed).
    pub shared_enforced: bool,
    /// Sealed passes the account's devices left, by plan, adult and period.
    pub shared: HashMap<(Plan, bool, u32), String>,
    /// Passes signed for the account, by plan, adult and period, while enforced.
    pub account_issued: HashMap<(Plan, bool, u32), u32>,
}

pub struct FakeRelay {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl FakeRelay {
    pub fn start(code: &str) -> FakeRelay {
        issuer_keys();
        let state = Arc::new(Mutex::new(State {
            codes: HashSet::from([code.to_string()]),
            credential_plan: Some(Plan::Chat),
            status: "active".into(),
            ..State::default()
        }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shared = state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = Router::new().fallback(
                    move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                        let state = shared.clone();
                        async move { handle(&state, method, uri, headers, body) }
                    },
                );
                axum::serve(listener, app).await.unwrap();
            });
        });
        FakeRelay { url, state }
    }

    pub fn issued(&self) -> u32 {
        self.state.lock().unwrap().issued
    }

    pub fn remove_all_devices(&self) {
        self.state.lock().unwrap().devices.clear();
    }

    pub fn set_unpaid(&self) {
        let mut s = self.state.lock().unwrap();
        s.credential_plan = None;
        s.status = "lapsed".into();
    }

    pub fn add_code(&self, code: &str) {
        self.state.lock().unwrap().codes.insert(code.to_string());
    }
}

fn error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = ApiError {
        error: ApiErrorBody {
            kind: kind.into(),
            message: message.into(),
        },
    };
    (status, axum::Json(body)).into_response()
}

fn ok<T: Serialize>(v: T) -> Response {
    axum::Json(v).into_response()
}

/// Checks the PAI-Device signature exactly as the relay must.
fn authenticate(
    state: &mut State,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<String, Response> {
    let header = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing device signature",
            )
        })?;
    let auth = DeviceAuth::parse(header)
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "unauthorized", "bad header"))?;
    let public_key = state.devices.get(&auth.device_id).ok_or_else(|| {
        error(
            StatusCode::UNAUTHORIZED,
            "unknown_device",
            "this device was removed",
        )
    })?;
    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    auth.verify(public_key, method.as_str(), path, body, now_unix())
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "bad_signature", "bad signature"))?;
    if method != Method::GET {
        let fresh = state.seen_signatures.insert(auth.signature.clone());
        if !fresh || std::mem::take(&mut state.replay_next_write) {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "replayed_request",
                "This exact request was already received.",
            ));
        }
    }
    Ok(auth.device_id)
}

fn handle(
    state: &Mutex<State>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(format!("{method} {}", uri.path()));
    match (method.as_str(), uri.path()) {
        ("POST", "/v1/pair") => {
            let req: PairRequest = serde_json::from_slice(&body).unwrap();
            if req.device.kind != DeviceKind::Tool {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "pai must pair as a tool",
                );
            }
            if !s.codes.remove(&req.code) {
                return error(
                    StatusCode::NOT_FOUND,
                    "invalid_code",
                    "that code is wrong or expired",
                );
            }
            let pk = unb64(&req.device.public_key).unwrap();
            assert_eq!(pk.len(), 32);
            s.next_device += 1;
            let device_id = format!("dev_{}", s.next_device);
            s.devices.insert(device_id.clone(), pk);
            ok(DeviceRegistered {
                account_id: ACCOUNT_ID.into(),
                device_id,
            })
        }
        ("GET", "/v1/account") => {
            if let Err(r) = authenticate(&mut s, &method, &uri, &headers, &body) {
                return r;
            }
            ok(AccountView {
                account_id: ACCOUNT_ID.into(),
                plan: s.credential_plan.map(|p| p.as_str().to_string()),
                credential_plan: s.credential_plan,
                status: s.status.clone(),
                paid_until: None,
                delete_after_days: Some(365),
                devices_used: s.devices.len() as u32,
                device_limit: 5,
                adult: s.adult,
                beta: false,
                rotate: s.rotate,
                pass_key: None,
                brave_web_search_privacy_accepted_at: None,
                refund_until: None,
                pending_plan: None,
            })
        }
        ("GET", "/v1/keys") => ok(IssuerKeys {
            keys: issuer_keys()
                .iter()
                .map(|((plan, day, adult), kp)| IssuerKeyInfo::new_for(*plan, *adult, *day, &kp.pk).unwrap())
                .collect(),
            grants: 0,
        }),
        ("POST", "/v1/credentials") => {
            if let Err(r) = authenticate(&mut s, &method, &uri, &headers, &body) {
                return r;
            }
            let req: CredentialRequest = serde_json::from_slice(&body).unwrap();
            if s.credential_plan != Some(req.plan) {
                return error(
                    StatusCode::PAYMENT_REQUIRED,
                    "payment_required",
                    "the plan isn't paid",
                );
            }
            if req.adult != s.adult {
                return error(StatusCode::FORBIDDEN, "wrong_key", "the adult setting doesn't match the account's");
            }
            let Some(kp) = issuer_keys().get(&(req.plan, req.day, req.adult)) else {
                return error(StatusCode::BAD_REQUEST, "bad_day", "no key for that day");
            };
            if s.issued >= 3 {
                let mut r = error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "issuance_limit",
                    "This device has already collected today's credential 3 times.",
                );
                r.headers_mut()
                    .insert("retry-after", axum::http::HeaderValue::from(7_380u64));
                return r;
            }
            if s.shared_enforced {
                if !req.shared {
                    return error(StatusCode::FORBIDDEN, "pass_key_required", "This account shares one day pass between its devices.");
                }
                let n = s.account_issued.entry((req.plan, req.adult, req.day)).or_default();
                if *n >= 1 {
                    return error(StatusCode::CONFLICT, "pass_already_issued", "Another device on this account is collecting this period's day pass.");
                }
                *n += 1;
            }
            let blind = kp
                .sk
                .blind_sign(unb64(&req.blind_message).unwrap())
                .unwrap();
            s.issued += 1;
            ok(CredentialResponse {
                blind_signature: b64(&blind.0),
            })
        }
        ("DELETE", "/v1/devices/self") => {
            match authenticate(&mut s, &method, &uri, &headers, &body) {
                Ok(id) => {
                    s.devices.remove(&id);
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(r) => r,
            }
        }
        (m, p) if p.starts_with("/v1/credentials/shared/") => {
            if let Err(r) = authenticate(&mut s, &method, &uri, &headers, &body) {
                return r;
            }
            let day: u32 = p.trim_start_matches("/v1/credentials/shared/").parse().unwrap();
            if m == "PUT" {
                let pass: SharedPass = serde_json::from_slice(&body).unwrap();
                s.shared.insert((pass.plan, pass.adult, day), pass.sealed);
                return StatusCode::NO_CONTENT.into_response();
            }
            let q = uri.query().unwrap_or("");
            let plan = q.split('&').find_map(|kv| kv.strip_prefix("plan=")).and_then(|v| Plan::parse(v).ok()).unwrap();
            let adult = q.split('&').any(|kv| kv == "adult=1");
            match s.shared.get(&(plan, adult, day)) {
                Some(sealed) => ok(SharedPass { plan, adult, sealed: sealed.clone() }),
                None => error(StatusCode::NOT_FOUND, "no_shared_pass", "No device on this account has shared this period's day pass yet."),
            }
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", "no such endpoint"),
    }
}

pub struct TestEnv {
    pub dir: tempfile::TempDir,
    pub env: Env,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let paths = Paths {
            secrets_file: config_dir.join("secrets.json"),
            old_secrets_file: None,
            config_dir,
            home: dir.path().join("home"),
        };
        let env = Env {
            keystore: Keystore::File(paths.secrets_file.clone()),
            old_stores: Vec::new(),
            paths,
            clock: Clock::System,
            relay_timeout: Duration::from_secs(10),
            pai_bin: PathBuf::from("/opt/pai/bin/pai"),
            vars: HashMap::from([("PAI_PROXY_PORT".to_string(), free_port().to_string())]),
            prefetch: false,
            service_manager: Manager::current(),
            commands: Commands::record(),
        };
        TestEnv { dir, env }
    }

    pub fn home(&self) -> &Path {
        &self.env.paths.home
    }

    /// Paired with a plain-HTTP API, without a relay: enough for anything that only reads the config.
    pub fn with_config() -> TestEnv {
        let t = TestEnv::new();
        pai::config::Config {
            relay_url: "http://127.0.0.1:9".into(),
            api_url: "http://api.test".into(),
            app_url: None,
            device_id: "dev_1".into(),
            account_id: ACCOUNT_ID.into(),
            test: None,
        }
        .save(&t.env.paths.config_file())
        .unwrap();
        t
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
