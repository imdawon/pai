use std::time::Duration;

use pai_proto::ed25519_dalek::SigningKey;
use pai_proto::{AccountView, ApiError, DeviceAuth, DeviceRegistered, IssuerKeys, PairRequest};
use reqwest::blocking::Client;
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

fn innermost(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cause: &dyn std::error::Error = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

pub struct Relay {
    base: String,
    client: Client,
}

struct Reply {
    status: StatusCode,
    retry_after: Option<u64>,
    body: Vec<u8>,
}

/// A non-2xx answer, reduced to what `pai` needs to pick a message.
#[derive(Debug)]
pub struct Failure {
    pub status: StatusCode,
    pub kind: String,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl Reply {
    fn failure(self) -> Failure {
        match serde_json::from_slice::<ApiError>(&self.body) {
            Ok(e) => Failure {
                status: self.status,
                kind: e.error.kind,
                message: e.error.message,
                retry_after: self.retry_after,
            },
            Err(_) => Failure {
                status: self.status,
                kind: String::new(),
                message: self
                    .status
                    .canonical_reason()
                    .unwrap_or("error")
                    .to_string(),
                retry_after: self.retry_after,
            },
        }
    }
}

impl Relay {
    /// `test_ca`, in test mode, is trusted instead of the public certificate authorities.
    pub fn new(base: &str, timeout: Duration, test_ca: Option<&std::path::Path>) -> Result<Relay> {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .user_agent(concat!("pai/", env!("CARGO_PKG_VERSION")))
            .use_preconfigured_tls(crate::pin::relay_config(test_ca)?)
            .build()
            .map_err(|e| Error::Setup(format!("couldn't set up the connection to the relay: {e}")))?;
        Ok(Relay {
            base: crate::config::trim_url(base),
            client,
        })
    }

    fn exchange_once(
        &self,
        method: &Method,
        path: &str,
        body: &[u8],
        signer: Option<(&SigningKey, &str)>,
    ) -> Result<Reply> {
        let url = Url::parse(&format!("{}{path}", self.base))
            .map_err(|e| Error::Io(format!("the relay URL {} is invalid: {e}", self.base)))?;
        let mut req = self.client.request(method.clone(), url.clone());
        if let Some((key, device_id)) = signer {
            // The relay verifies the path it received, which includes any prefix in the relay URL.
            let mut signed_path = url.path().to_string();
            if let Some(q) = url.query() {
                signed_path.push('?');
                signed_path.push_str(q);
            }
            let auth = DeviceAuth::sign(key, device_id, method.as_str(), &signed_path, body);
            req = req.header(reqwest::header::AUTHORIZATION, auth.header());
        }
        if !body.is_empty() {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        let resp = req.send().map_err(|e| {
            let cause = innermost(&e);
            if cause.contains("certificate") || cause.contains("UnknownIssuer") {
                Error::Setup(format!(
                    "the relay at {} showed a certificate pai doesn't trust ({cause}), so pai sent nothing. \
                     Check the relay address, and your network",
                    self.base
                ))
            } else if e.is_connect() || e.is_timeout() || e.is_request() {
                Error::RelayUnreachable(format!("{} ({cause})", self.base))
            } else {
                Error::Io(format!("request to the relay failed: {e}"))
            }
        })?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let body = resp
            .bytes()
            .map_err(|_| Error::RelayUnreachable(self.base.clone()))?
            .to_vec();
        Ok(Reply {
            status,
            retry_after,
            body,
        })
    }

    fn exchange(
        &self,
        method: Method,
        path: &str,
        body: &[u8],
        signer: Option<(&SigningKey, &str)>,
    ) -> Result<Reply> {
        let reply = self.exchange_once(&method, path, body, signer)?;
        // Ed25519 is deterministic and the signed timestamp has one-second resolution, so an
        // identical write repeated within a second carries the same signature and the relay
        // refuses it as a replay. Waiting past the second gives a fresh signature.
        if signer.is_some() && reply.status == StatusCode::UNAUTHORIZED && is_replay(&reply.body) {
            std::thread::sleep(Duration::from_millis(1100));
            return self.exchange_once(&method, path, body, signer);
        }
        Ok(reply)
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
        signer: Option<(&SigningKey, &str)>,
    ) -> std::result::Result<T, CallError> {
        let body = body
            .map(|b| serde_json::to_vec(b).expect("request serializes"))
            .unwrap_or_default();
        let reply = self
            .exchange(method, path, &body, signer)
            .map_err(CallError::Pai)?;
        if !reply.status.is_success() {
            return Err(CallError::Http(reply.failure()));
        }
        serde_json::from_slice(&reply.body).map_err(|e| {
            CallError::Pai(Error::Io(format!(
                "the relay sent an unexpected response to {path}: {e}"
            )))
        })
    }

    pub fn pair(&self, req: &PairRequest) -> Result<DeviceRegistered> {
        self.call(Method::POST, "/v1/pair", Some(req), None)
            .map_err(|e| match e {
                CallError::Pai(e) => e,
                CallError::Http(f) => match f.kind.as_str() {
                    "invalid_pairing_code" => Error::PairingRejected(f.message),
                    "device_limit" | "rate_limited" | "invalid_body" => generic(f),
                    _ if matches!(f.status.as_u16(), 400 | 403 | 404 | 410) => {
                        Error::PairingRejected(f.message)
                    }
                    _ => generic(f),
                },
            })
    }

    pub fn account(&self, key: &SigningKey, device_id: &str) -> Result<AccountView> {
        self.call(
            Method::GET,
            "/v1/account",
            None::<&()>,
            Some((key, device_id)),
        )
        .map_err(device_error)
    }

    pub fn keys(&self) -> Result<IssuerKeys> {
        // ?all=1 includes Lite, which the relay leaves out of the plain list for older pai.
        self.call(Method::GET, "/v1/keys?all=1", None::<&()>, None)
            .map_err(|e| match e {
                CallError::Pai(e) => e,
                CallError::Http(f) => generic(f),
            })
    }

    pub fn credentials(
        &self,
        key: &SigningKey,
        device_id: &str,
        req: &pai_proto::CredentialRequest,
    ) -> Result<pai_proto::CredentialResponse> {
        self.call(
            Method::POST,
            "/v1/credentials",
            Some(req),
            Some((key, device_id)),
        )
        .map_err(device_error)
    }

    /// The account's sealed day pass for `plan` and period `day`, if another of its devices left
    /// it. None when none has, or the relay is older than shared passes.
    pub fn shared_pass(
        &self,
        key: &SigningKey,
        device_id: &str,
        plan: pai_proto::Plan,
        adult: bool,
        day: u32,
    ) -> Result<Option<pai_proto::SharedPass>> {
        let path = format!("/v1/credentials/shared/{day}?plan={}{}", plan.as_str(), if adult { "&adult=1" } else { "" });
        match self.call(Method::GET, &path, None::<&()>, Some((key, device_id))) {
            Ok(p) => Ok(Some(p)),
            Err(CallError::Http(f)) if f.status == StatusCode::NOT_FOUND => Ok(None),
            Err(e) => Err(device_error(e)),
        }
    }

    /// Leaves the account's day pass, sealed with the pass key, for its other devices.
    pub fn put_shared_pass(&self, key: &SigningKey, device_id: &str, day: u32, pass: &pai_proto::SharedPass) -> Result<()> {
        let body = serde_json::to_vec(pass).expect("request serializes");
        let reply = self.exchange(Method::PUT, &format!("/v1/credentials/shared/{day}"), &body, Some((key, device_id)))?;
        if reply.status.is_success() {
            return Ok(());
        }
        Err(device_error(CallError::Http(reply.failure())))
    }

    /// Ok(false) when the relay no longer knows this device, which is fine when unpairing.
    pub fn delete_self(&self, key: &SigningKey, device_id: &str) -> Result<bool> {
        let reply = self.exchange(
            Method::DELETE,
            "/v1/devices/self",
            &[],
            Some((key, device_id)),
        )?;
        if reply.status.is_success() {
            return Ok(true);
        }
        let f = reply.failure();
        if f.status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        match device_error(CallError::Http(f)) {
            Error::DeviceRemoved => Ok(false),
            e => Err(e),
        }
    }
}

pub enum CallError {
    Pai(Error),
    Http(Failure),
}

fn is_replay(body: &[u8]) -> bool {
    serde_json::from_slice::<ApiError>(body).is_ok_and(|e| e.error.kind == "replayed_request")
}

fn generic(f: Failure) -> Error {
    Error::Relay {
        status: f.status.as_u16(),
        message: f.message,
    }
}

/// Maps a failed device-authenticated call to the situations a user can act on, by the relay's
/// error `type` (see pai-relay's `src/api/mod.rs` and `credentials.rs`), then by HTTP status.
fn device_error(e: CallError) -> Error {
    let f = match e {
        CallError::Pai(e) => return e,
        CallError::Http(f) => f,
    };
    match f.kind.as_str() {
        "clock_skew" => Error::ClockSkew,
        "unknown_device" | "device_expired" | "bad_signature" | "unauthorized"
        | "unknown_account" => Error::DeviceRemoved,
        "banned" => Error::Banned,
        "payment_required" => Error::NotPaid {
            detail: "no active plan".into(),
        },
        "issuance_limit" => Error::IssuanceLimit {
            retry_after: f.retry_after,
        },
        "wrong_day" => Error::WrongDay(f.message),
        "pass_key_required" => Error::PassKeyRequired,
        "pass_already_issued" => Error::PassAlreadyIssued,
        _ => match f.status {
            StatusCode::UNAUTHORIZED => Error::DeviceRemoved,
            StatusCode::PAYMENT_REQUIRED => Error::NotPaid { detail: f.message },
            _ => generic(f),
        },
    }
}
