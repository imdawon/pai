use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use pai_proto::ed25519_dalek::SigningKey;
use pai_proto::{
    CredentialRequest, DeviceKind, NewDevice, PairRequest, PendingCredential, Plan, b64,
    day_number, unb64,
};
use zeroize::Zeroizing;

use crate::config::{Config, DEFAULT_API_URL, DEFAULT_RELAY_URL, Paths, trim_url};
use crate::error::{Error, Result, io_err};
use crate::keystore::{CREDENTIAL, DEVICE_KEY, Keystore};
use crate::relay::Relay;

#[derive(Debug, Clone, Copy)]
pub enum Clock {
    System,
    Fixed(i64),
}

impl Clock {
    pub fn now(self) -> i64 {
        match self {
            Clock::System => pai_proto::now_unix(),
            Clock::Fixed(t) => t,
        }
    }
}

/// Everything a command reads from the outside world, so tests can replace all of it.
#[derive(Debug, Clone)]
pub struct Env {
    pub paths: Paths,
    pub keystore: Keystore,
    pub clock: Clock,
    pub relay_timeout: Duration,
    /// The path coding tools should run, written into their config by `pai setup`.
    pub pai_bin: PathBuf,
    /// Environment variables, captured once.
    pub vars: HashMap<String, String>,
}

impl Env {
    pub fn from_process() -> Result<Env> {
        let paths = Paths::from_env()?;
        let vars: HashMap<String, String> = std::env::vars().collect();
        let keystore = match vars
            .get("PAI_KEYSTORE")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("os") | Some("keychain") => Keystore::Os,
            Some("file") => Keystore::File(paths.secrets_file.clone()),
            Some(other) => {
                return Err(Error::Io(format!(
                    "PAI_KEYSTORE={other} is not supported; use `file` or leave it unset"
                )));
            }
        };
        let pai_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pai"));
        Ok(Env {
            paths,
            keystore,
            clock: Clock::System,
            relay_timeout: Duration::from_secs(30),
            pai_bin,
            vars,
        })
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    pub fn config(&self) -> Result<Option<Config>> {
        Config::load(&self.paths.config_file())
    }

    pub fn default_api_url(&self) -> String {
        trim_url(self.var("PAI_API_URL").unwrap_or(DEFAULT_API_URL))
    }

    fn paired(&self) -> Result<Config> {
        self.config()?.ok_or(Error::NotPaired)
    }

    fn device_key(&self) -> Result<SigningKey> {
        let stored = self.keystore.get(DEVICE_KEY)?.ok_or(Error::NotPaired)?;
        let bytes = Zeroizing::new(unb64(&stored).ok_or_else(|| {
            Error::Keystore(
                "the stored device key is corrupt; run `pai unpair` and pair again".into(),
            )
        })?);
        let seed: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            Error::Keystore(
                "the stored device key is corrupt; run `pai unpair` and pair again".into(),
            )
        })?;
        Ok(SigningKey::from_bytes(seed))
    }
}

const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PAIRING_CODE_LEN: usize = 8;

/// Codes are read aloud or retyped, so accept the usual Crockford confusions and separators.
pub fn normalize_code(code: &str) -> Result<String> {
    let out: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '_'))
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        })
        .collect();
    if out.len() != PAIRING_CODE_LEN || !out.chars().all(|c| CROCKFORD.contains(c)) {
        return Err(Error::BadPairingCode);
    }
    Ok(out)
}

fn default_device_name() -> String {
    let host = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    let host = if host.is_empty() {
        "this computer".to_string()
    } else {
        host
    };
    let name = format!("pai on {host}");
    name.chars().take(64).collect()
}

pub fn pair(
    env: &Env,
    code: &str,
    relay_url: Option<&str>,
    api_url: Option<&str>,
    name: Option<&str>,
) -> Result<Config> {
    let code = normalize_code(code)?;
    if env.config()?.is_some() {
        return Err(Error::AlreadyPaired);
    }
    env.keystore.check()?;

    let relay_url = trim_url(
        relay_url
            .or(env.var("PAI_RELAY_URL"))
            .unwrap_or(DEFAULT_RELAY_URL),
    );
    let api_url = api_url
        .map(trim_url)
        .unwrap_or_else(|| env.default_api_url());
    let relay = Relay::new(&relay_url, env.relay_timeout);

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let request = PairRequest {
        code,
        device: NewDevice {
            public_key: b64(key.verifying_key().to_bytes()),
            name: name.map(str::to_string).unwrap_or_else(default_device_name),
            kind: DeviceKind::Tool,
        },
    };
    let registered = relay.pair(&request)?;

    let config = Config {
        relay_url,
        api_url,
        device_id: registered.device_id.clone(),
        account_id: registered.account_id,
    };
    let saved = (|| {
        let seed = Zeroizing::new(b64(key.to_bytes()));
        env.keystore.set(DEVICE_KEY, &seed)?;
        env.keystore.delete(CREDENTIAL)?;
        config.save(&env.paths.config_file())
    })();
    if let Err(e) = saved {
        // Don't leave a registered device behind that nothing on this machine can use.
        let _ = relay.delete_self(&key, &registered.device_id);
        let _ = env.keystore.delete(DEVICE_KEY);
        return Err(e);
    }
    Ok(config)
}

fn cached_credential(env: &Env, day: u32) -> Result<Option<String>> {
    let Some(stored) = env.keystore.get(CREDENTIAL)? else {
        return Ok(None);
    };
    match pai_proto::Credential::decode(&stored) {
        Ok(c) if c.day == day => Ok(Some(stored.to_string())),
        _ => Ok(None),
    }
}

/// Serializes credential fetches across processes: tools start several `pai token` at once,
/// and each device may only collect a few credentials a day.
fn lock(env: &Env) -> Result<fs::File> {
    let dir = &env.paths.config_dir;
    fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
    let path = env.paths.lock_file();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io_err("open", &path, e))?;
    file.lock().map_err(|e| io_err("lock", &path, e))?;
    Ok(file)
}

/// Returns today's credential, fetching a fresh one only when the cache is not for today
/// (or `refresh` is set).
pub fn token(env: &Env, refresh: bool) -> Result<String> {
    let config = env.paired()?;
    let today = day_number(env.clock.now());
    if !refresh && let Some(c) = cached_credential(env, today)? {
        return Ok(c);
    }

    let _lock = lock(env)?;
    if !refresh && let Some(c) = cached_credential(env, today)? {
        return Ok(c);
    }

    let key = env.device_key()?;
    let relay = Relay::new(&config.relay_url, env.relay_timeout);
    let account = relay.account(&key, &config.device_id)?;
    if account.status == "banned" {
        return Err(Error::Banned);
    }
    let plan = account.credential_plan.ok_or_else(|| Error::NotPaid {
        detail: format!("status: {}", account.status),
    })?;

    let keys = relay.keys()?;
    let info = keys
        .keys
        .iter()
        .find(|k| k.plan == plan && k.day == today)
        .ok_or_else(|| {
            Error::BadIssuerResponse(format!(
                "it has no {} key for UTC day {today}; check that this computer's date is right",
                plan.as_str()
            ))
        })?;
    let issuer = pai_proto::issuer_public_key_from_spki(&info.spki)
        .map_err(|e| Error::BadIssuerResponse(format!("unreadable issuer key: {e}")))?;

    let pending = PendingCredential::new(&issuer, plan, today)
        .map_err(|e| Error::BadIssuerResponse(e.to_string()))?;
    let request = CredentialRequest {
        plan,
        day: today,
        blind_message: b64(pending.blind_message()),
    };
    let response = relay.credentials(&key, &config.device_id, &request)?;
    let blind_signature = unb64(&response.blind_signature)
        .ok_or_else(|| Error::BadIssuerResponse("the blind signature isn't base64url".into()))?;
    let credential = pending
        .finalize(&issuer, &blind_signature)
        .map_err(|_| Error::BadIssuerResponse("the signature doesn't verify".into()))?;
    credential
        .verify(&issuer)
        .map_err(|_| Error::BadIssuerResponse("the signature doesn't verify".into()))?;

    let encoded = credential.encode();
    env.keystore.set(CREDENTIAL, &encoded)?;
    Ok(encoded)
}

pub struct Status {
    pub config: Option<Config>,
    pub plan: String,
    pub credential_today: bool,
    pub keystore: String,
}

pub fn status(env: &Env) -> Result<Status> {
    let config = env.config()?;
    let keystore = env.keystore.describe();
    let Some(cfg) = config.clone() else {
        return Ok(Status {
            config,
            plan: "-".into(),
            credential_today: false,
            keystore,
        });
    };
    let today = day_number(env.clock.now());
    let credential_today = cached_credential(env, today).ok().flatten().is_some();
    let plan = match env.device_key() {
        Err(e) => format!("unknown ({e})"),
        Ok(key) => match Relay::new(
            &cfg.relay_url,
            env.relay_timeout.min(Duration::from_secs(10)),
        )
        .account(&key, &cfg.device_id)
        {
            Ok(a) => match (&a.plan, a.credential_plan) {
                (Some(p), Some(_)) => format!("{p} ({})", a.status),
                (Some(p), None) => format!("{p} ({}, not paid)", a.status),
                (None, _) => format!("none ({})", a.status),
            },
            Err(Error::DeviceRemoved) => "unknown (device removed, pair again)".into(),
            Err(Error::RelayUnreachable(_)) => "unknown (relay unreachable)".into(),
            Err(e) => format!("unknown ({e})"),
        },
    };
    Ok(Status {
        config,
        plan,
        credential_today,
        keystore,
    })
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(c) = &self.config else {
            writeln!(f, "Paired:     no")?;
            return writeln!(f, "Keystore:   {}", self.keystore);
        };
        writeln!(f, "Paired:     yes")?;
        writeln!(f, "Account:    {}", c.account_id)?;
        writeln!(f, "Plan:       {}", self.plan)?;
        writeln!(f, "Device:     {}", c.device_id)?;
        writeln!(
            f,
            "Credential: {}",
            if self.credential_today {
                "yes, for today"
            } else {
                "none for today"
            }
        )?;
        writeln!(f, "Relay:      {}", c.relay_url)?;
        writeln!(f, "API:        {}", c.api_url)?;
        writeln!(f, "Keystore:   {}", self.keystore)
    }
}

/// Removes this device from the account, then forgets it locally whatever the relay said.
/// Returns lines to show the user.
pub fn unpair(env: &Env) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let config = env.config()?;
    let key = match env.device_key() {
        Ok(k) => Some(k),
        Err(Error::NotPaired) => None,
        Err(e @ Error::KeychainUnavailable(_)) => return Err(e),
        Err(_) => None,
    };
    match (&config, &key) {
        (Some(cfg), Some(key)) => match Relay::new(&cfg.relay_url, env.relay_timeout.min(Duration::from_secs(15)))
            .delete_self(key, &cfg.device_id)
        {
            Ok(true) => notes.push("Removed this device from your account.".into()),
            Ok(false) => notes.push("The relay had already removed this device.".into()),
            Err(e) => notes.push(format!(
                "Couldn't remove the device on the relay ({e}). Remove it under Devices in the web app."
            )),
        },
        (None, None) => {
            if env.keystore.get(CREDENTIAL)?.is_none() {
                notes.push("Not paired; nothing to remove.".into());
                return Ok(notes);
            }
        }
        _ => notes.push("The pairing was incomplete; removing what's left of it.".into()),
    }
    env.keystore.delete(DEVICE_KEY)?;
    env.keystore.delete(CREDENTIAL)?;
    for path in [env.paths.config_file(), env.paths.lock_file()] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("remove", &path, e)),
        }
    }
    notes.push(
        "Deleted the device key, the cached credential and the config from this machine.".into(),
    );
    Ok(notes)
}

/// Best-effort plan lookup for `pai setup`, which sizes context windows by plan.
pub fn plan_hint(env: &Env) -> Option<Plan> {
    let cfg = env.config().ok()??;
    let key = env.device_key().ok()?;
    Relay::new(&cfg.relay_url, Duration::from_secs(5))
        .account(&key, &cfg.device_id)
        .ok()?
        .credential_plan
}
