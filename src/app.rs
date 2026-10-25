use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use pai_proto::ed25519_dalek::SigningKey;
use pai_proto::{
    CredentialRequest, DeviceKind, NewDevice, PairRequest, Period, PendingCredential, Plan, b64,
    day_number, period_now, unb64,
};
use zeroize::Zeroizing;

use crate::config::{
    Config, DEFAULT_API_URL, DEFAULT_RELAY_URL, Paths, TestMode, app_url_for_relay, openable_url, trim_url,
};
use crate::error::{Error, Result, io_err};
use crate::keystore::{CREDENTIAL, DEVICE_KEY, Keystore, NEXT_CREDENTIAL, PROXY_KEY};
use crate::relay::Relay;
use crate::service::{Commands, Manager};

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
    /// Where pai before 0.1.6 may have kept the secrets, checked in order the first time the
    /// device key is needed and missing (`adopt_old_secrets`). Empty in tests.
    pub old_stores: Vec<Keystore>,
    pub clock: Clock,
    pub relay_timeout: Duration,
    /// The path coding tools should run, written into their config by `pai setup`.
    pub pai_bin: PathBuf,
    /// Environment variables, captured once.
    pub vars: HashMap<String, String>,
    /// Whether the proxy collects tomorrow's credential ahead. `PAI_PREFETCH=0` turns it off.
    pub prefetch: bool,
    /// Which login service `pai service` installs, and how it runs launchctl or systemctl.
    pub service_manager: Manager,
    pub commands: Commands,
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
            None | Some("") | Some("file") => Keystore::File(paths.secrets_file.clone()),
            Some(other) => {
                return Err(Error::Io(format!(
                    "PAI_KEYSTORE={other} is not supported: pai keeps its secrets in {}",
                    paths.secrets_file.display()
                )));
            }
        };
        let pai_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pai"));
        let prefetch = vars.get("PAI_PREFETCH").map(|v| v.trim()) != Some("0");
        let old_stores = paths.old_secrets_file.iter().map(|p| Keystore::File(p.clone())).chain([Keystore::Os]).collect();
        Ok(Env {
            paths,
            keystore,
            old_stores,
            clock: Clock::System,
            relay_timeout: Duration::from_secs(30),
            pai_bin,
            vars,
            prefetch,
            service_manager: Manager::current(),
            commands: Commands::System,
        })
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    /// The pairing. A release build refuses one made in test mode by a staging build, rather than
    /// quietly running it with the release's trust: the user has to know to pair again.
    pub fn config(&self) -> Result<Option<Config>> {
        let config = Config::load(&self.paths.config_file())?;
        #[cfg(not(feature = "test-mode"))]
        if config.as_ref().is_some_and(|c| c.test.is_some()) {
            return Err(Error::Setup(format!(
                "{} was made in test mode by a staging build of pai. This pai is a release build, which trusts \
                 only the public certificate authorities and the model API pins built into it. Run `pai unpair`, \
                 then pair again from Account > Set up a coding tool in the DawonAI web app",
                self.paths.config_file().display()
            )));
        }
        Ok(config)
    }

    pub fn default_api_url(&self) -> String {
        trim_url(self.var("PAI_API_URL").unwrap_or(DEFAULT_API_URL))
    }

    /// Finds the device key where pai before 0.1.6 kept it, the first time this pai needs it, and
    /// moves every secret from there into the secrets file, so an update never asks anyone to pair
    /// again. Errors from the old store are reported, not skipped: "not paired" would send the user
    /// to pair again, which a paired computer refuses.
    fn adopt_old_secrets(&self) -> Result<Option<Zeroizing<String>>> {
        for old in &self.old_stores {
            if !matches!(old, Keystore::File(p) if !p.exists()) {
                old.check()?;
            }
            let Some(key) = old.get(DEVICE_KEY)? else { continue };
            self.keystore.set(DEVICE_KEY, &key)?;
            for name in [CREDENTIAL, NEXT_CREDENTIAL, PROXY_KEY] {
                if let Ok(Some(value)) = old.get(name) {
                    let _ = self.keystore.set(name, &value);
                }
            }
            for name in [DEVICE_KEY, CREDENTIAL, NEXT_CREDENTIAL, PROXY_KEY] {
                let _ = old.delete(name);
            }
            return Ok(Some(key));
        }
        Ok(None)
    }

    pub fn paired(&self) -> Result<Config> {
        self.config()?.ok_or(Error::NotPaired)
    }

    fn device_key(&self) -> Result<SigningKey> {
        let stored = match self.keystore.get(DEVICE_KEY)? {
            Some(s) => s,
            None if self.paths.config_file().exists() => self.adopt_old_secrets()?.ok_or(Error::DeviceKeyMissing)?,
            None => return Err(Error::NotPaired),
        };
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
    app_url: Option<&str>,
    name: Option<&str>,
    test: Option<(&std::path::Path, &[String])>,
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
    let app_url = app_url
        .or(env.var("PAI_APP_URL"))
        .map(trim_url)
        .unwrap_or_else(|| app_url_for_relay(&relay_url));
    if !openable_url(&app_url) {
        return Err(Error::Setup(format!(
            "the DawonAI web app address {app_url} isn't one pai can open: use an https:// address"
        )));
    }
    let test: Option<TestMode> = match test {
        None => None,
        #[cfg(not(feature = "test-mode"))]
        Some(_) => return Err(Error::Setup("this pai is a release build, which has no test mode".into())),
        #[cfg(feature = "test-mode")]
        Some((ca, pins)) => {
            if pins.is_empty() {
                return Err(Error::Setup("test mode needs the API's pins as well as the test CA".into()));
            }
            let pem = fs::read(ca).map_err(|e| io_err("read", ca, e))?;
            fs::create_dir_all(&env.paths.config_dir).map_err(|e| io_err("create", &env.paths.config_dir, e))?;
            let copy = env.paths.test_ca_file();
            fs::write(&copy, pem).map_err(|e| io_err("write", &copy, e))?;
            Some(TestMode { ca: copy, api_pins: pins.to_vec() })
        }
    };
    let relay = Relay::new(&relay_url, env.relay_timeout, test.as_ref().map(|t| t.ca.as_path()))?;

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    // Saved before the code is spent: a code works once, so a failure to save must cost nothing.
    env.keystore.set(DEVICE_KEY, &Zeroizing::new(b64(key.to_bytes())))?;
    env.keystore.delete(CREDENTIAL)?;
    env.keystore.delete(NEXT_CREDENTIAL)?;
    let request = PairRequest {
        code,
        device: NewDevice {
            public_key: b64(key.verifying_key().to_bytes()),
            name: name.map(str::to_string).unwrap_or_else(default_device_name),
            kind: DeviceKind::Tool,
        },
    };
    let registered = match relay.pair(&request) {
        Ok(r) => r,
        Err(e) => {
            let _ = env.keystore.delete(DEVICE_KEY);
            return Err(e);
        }
    };

    let config = Config {
        relay_url,
        api_url,
        app_url: Some(app_url),
        device_id: registered.device_id.clone(),
        account_id: registered.account_id,
        test,
    };
    if let Err(e) = config.save(&env.paths.config_file()) {
        // Don't leave a registered device behind that nothing on this machine can use.
        let _ = relay.delete_self(&key, &registered.device_id);
        let _ = env.keystore.delete(DEVICE_KEY);
        return Err(e);
    }
    Ok(config)
}

/// The credential for the current period (offset 0) or the next (offset 1). A period is a UTC
/// day, or a UTC hour on the 1-day trial; the stored credential says which plan it's for.
fn stored_credential(env: &Env, name: &str, offset: u32) -> Result<Option<String>> {
    let Some(stored) = env.keystore.get(name)? else {
        return Ok(None);
    };
    match pai_proto::Credential::decode(&stored) {
        Ok(c) if c.day == period_now(c.plan, env.clock.now()) + offset => Ok(Some(stored.to_string())),
        _ => Ok(None),
    }
}

fn cached_credential(env: &Env) -> Result<Option<String>> {
    stored_credential(env, CREDENTIAL, 0)
}

/// The plan of the credential this computer holds, without asking the relay.
pub fn held_plan(env: &Env) -> Option<Plan> {
    let stored = env.keystore.get(CREDENTIAL).ok()??;
    pai_proto::Credential::decode(&stored).ok().map(|c| c.plan)
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

/// Returns the credential for the current period. It comes from the cache, or from the one
/// collected in the background during the last period; only when neither is there (or `refresh`
/// is set) is one fetched now.
pub fn credential(env: &Env, refresh: bool) -> Result<String> {
    let config = env.paired()?;
    let got = current_credential(env, &config, refresh);
    if matches!(got, Err(Error::DeviceRemoved)) {
        open_web_app(env, &config);
    }
    got
}

/// When this device's link is gone, the user has to pair again from Account in the DawonAI web
/// app, so `pai` opens the web app in the default browser. At most once a UTC day, so a coding
/// tool retrying `pai token` doesn't open a tab each time. Returns whether it opened it now;
/// never fails the command. `PAI_OPEN_BROWSER=0` turns this off.
pub fn open_web_app(env: &Env, config: &Config) -> bool {
    if env.var("PAI_OPEN_BROWSER") == Some("0") {
        return false;
    }
    // The web app has no link to its Account screen; it opens on the chat, with Account a click away.
    let url = format!("{}/#account", config.app_url());
    if !openable_url(&url) {
        return false;
    }
    // Several `pai token` calls fail together; the lock lets only the first one open the browser.
    let Ok(_lock) = lock(env) else {
        return false;
    };
    let today = day_number(env.clock.now()).to_string();
    let marker = env.paths.web_app_opened_file();
    if fs::read_to_string(&marker).is_ok_and(|day| day.trim() == today) || fs::write(&marker, &today).is_err() {
        return false;
    }
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[url.as_str()])
    } else if cfg!(windows) {
        ("cmd", &["/c", "start", "", url.as_str()])
    } else {
        ("xdg-open", &[url.as_str()])
    };
    env.commands.start(program, args)
}

/// Which certificates `pai` trusts: the public certificate authorities and the pins this release
/// was built with. A staging build (the `test-mode` feature, docs/TESTING.md) can swap in a test
/// CA and test pins, from the pairing or from `PAI_TEST_CA` and `PAI_TEST_API_PINS`; it never
/// turns pin checking off. In a release build `test_ca` is always `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust {
    pub test_ca: Option<PathBuf>,
    pub api_pins: Vec<String>,
}

impl Trust {
    pub fn is_test(&self) -> bool {
        self.test_ca.is_some()
    }
}

fn pin_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

fn release_pins() -> Vec<String> {
    pin_list(option_env!("PAI_RELEASE_API_PINS").unwrap_or(""))
}

#[cfg_attr(not(feature = "test-mode"), allow(unused_variables))]
pub fn trust(env: &Env, config: Option<&Config>) -> Trust {
    #[cfg(feature = "test-mode")]
    if let Some(t) = test_trust(env, config) {
        return t;
    }
    Trust { test_ca: None, api_pins: release_pins() }
}

#[cfg(feature = "test-mode")]
fn test_trust(env: &Env, config: Option<&Config>) -> Option<Trust> {
    let test = config.and_then(|c| c.test.as_ref());
    let test_ca = env.var("PAI_TEST_CA").map(PathBuf::from).or_else(|| test.map(|t| t.ca.clone()))?;
    let api_pins = match (env.var("PAI_TEST_API_PINS"), test) {
        (Some(v), _) => pin_list(v),
        (None, Some(t)) => t.api_pins.clone(),
        (None, None) => release_pins(),
    };
    Some(Trust { test_ca: Some(test_ca), api_pins })
}

/// Printed on stderr by every command while test mode is on.
pub fn test_mode_warning(t: &Trust) -> Option<String> {
    t.test_ca.as_ref().map(|ca| {
        format!(
            "pai: TEST MODE: trusting only {} and test pins, instead of the real service's. \
             If you didn't set this up yourself for testing, run `pai unpair` and pair again",
            ca.display()
        )
    })
}

impl Env {
    fn relay(&self, config: &Config, timeout: Duration) -> Result<Relay> {
        Relay::new(&config.relay_url, timeout, trust(self, Some(config)).test_ca.as_deref())
    }
}

/// This machine's key for the local proxy, made on first use.
pub fn local_key(env: &Env) -> Result<String> {
    if let Some(k) = env.keystore.get(PROXY_KEY)? {
        return Ok(k.to_string());
    }
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let key = format!("pai-local-{}", b64(bytes));
    env.keystore.set(PROXY_KEY, &key)?;
    Ok(key)
}

fn local_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("a plain HTTP client builds")
}

/// What the local proxy answers at `/pai/health`: `Some(fingerprint)`, or `None` when nothing
/// of ours is listening.
pub(crate) fn proxy_health(env: &Env, key: &str) -> Option<String> {
    let r = local_client()
        .get(format!("{}/pai/health", crate::proxy::url(env)))
        .bearer_auth(key)
        .send()
        .ok()?;
    let v: serde_json::Value = r.json().ok()?;
    v["pai"].as_str().map(str::to_string)
}

fn stop_proxy(env: &Env, key: &str) {
    let _ = local_client().post(format!("{}/pai/stop", crate::proxy::url(env))).bearer_auth(key).send();
}

/// Stops a running proxy, if there is one, and waits until it has let go of the port.
pub(crate) fn stop_proxy_and_wait(env: &Env, key: &str) {
    if proxy_health(env, key).is_none() {
        return;
    }
    stop_proxy(env, key);
    for _ in 0..30 {
        if proxy_health(env, key).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_fingerprint(env: &Env, key: &str, want: &str) -> bool {
    for _ in 0..50 {
        if proxy_health(env, key).as_deref() == Some(want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Makes sure this version of `pai`'s proxy is running with this pairing, starting it if not.
/// Runs this `pai` with `args` in the background, with no terminal, outliving the caller.
pub fn spawn_detached(env: &Env, args: &[&str]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(&env.pai_bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so closing the coding tool doesn't take it down too.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map(|_| ())
}

/// The model API this machine uses: the pairing's, or this release's before pairing.
pub fn api_url(env: &Env) -> String {
    match env.config() {
        Ok(Some(c)) => c.api_url,
        _ => env.default_api_url(),
    }
}

pub fn ensure_proxy(env: &Env) -> Result<()> {
    let config = env.paired()?;
    let key = local_key(env)?;
    let want = crate::proxy::fingerprint(&config, &trust(env, Some(&config)));
    match proxy_health(env, &key) {
        Some(have) if have == want => return Ok(()),
        Some(_) => stop_proxy_and_wait(env, &key),
        None => {}
    }
    crate::proxy::upstream_client(env, &config)?;
    // With the login service installed, let it own the proxy, so the two don't fight over the port.
    if crate::service::restart(env) && wait_for_fingerprint(env, &key, &want) {
        return Ok(());
    }

    spawn_detached(env, &["proxy"]).map_err(|e| Error::Io(format!("couldn't start pai's local proxy: {e}")))?;
    if wait_for_fingerprint(env, &key, &want) {
        return Ok(());
    }
    Err(Error::Io(format!(
        "pai's local proxy didn't start on 127.0.0.1:{}. Run `pai proxy` to see why, or set PAI_PROXY_PORT if that port is taken",
        crate::proxy::port(env)
    )))
}

/// What coding tools run: checks there's a credential for today (so an unpaid plan or a removed
/// device shows up when the tool starts), makes sure the proxy is up, and prints this machine's
/// local key. The credential itself never leaves `pai`.
pub fn token(env: &Env) -> Result<String> {
    credential(env, false)?;
    ensure_proxy(env)?;
    local_key(env)
}

fn current_credential(env: &Env, config: &Config, refresh: bool) -> Result<String> {
    if !refresh && let Some(c) = cached_credential(env)? {
        return Ok(c);
    }
    let _lock = lock(env)?;
    if !refresh {
        if let Some(c) = cached_credential(env)? {
            return Ok(c);
        }
        if let Some(c) = stored_credential(env, NEXT_CREDENTIAL, 0)? {
            env.keystore.set(CREDENTIAL, &c)?;
            env.keystore.delete(NEXT_CREDENTIAL)?;
            return Ok(c);
        }
    }
    let (encoded, _) = fetch_credential(env, config, 0)?;
    env.keystore.set(CREDENTIAL, &encoded)?;
    Ok(encoded)
}

/// Asks the relay for a blind-signed credential for the current period (`offset` 0) or the next
/// (1). Returns it with its period number.
fn fetch_credential(env: &Env, config: &Config, offset: u32) -> Result<(String, u32)> {
    let key = env.device_key()?;
    let relay = env.relay(config, env.relay_timeout)?;
    let account = relay.account(&key, &config.device_id)?;
    if account.status == "banned" {
        return Err(Error::Banned);
    }
    let plan = account.credential_plan.ok_or_else(|| Error::NotPaid {
        detail: format!("status: {}", account.status),
    })?;
    let day = period_now(plan, env.clock.now()) + offset;
    // Adult accounts' credentials come from the adult keys, so the model server allows erotica.
    let adult = account.adult;

    let keys = relay.keys()?;
    let info = keys
        .keys
        .iter()
        .find(|k| k.plan == plan && k.day == day && k.adult == adult)
        .ok_or_else(|| {
            Error::BadIssuerResponse(format!(
                "it has no {} key for period {day}; check that this computer's date and time are right",
                plan.as_str()
            ))
        })?;
    let issuer = pai_proto::issuer_public_key_from_spki(&info.spki)
        .map_err(|e| Error::BadIssuerResponse(format!("unreadable issuer key: {e}")))?;

    let pending = PendingCredential::new_for(&issuer, plan, adult, day)
        .map_err(|e| Error::BadIssuerResponse(e.to_string()))?;
    let request = CredentialRequest {
        plan,
        adult,
        day,
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
    Ok((credential.encode(), day))
}

/// Leave the end of each period unused, so a late fetch still lands before the period ends.
fn prefetch_margin(period: Period) -> i64 {
    match period {
        Period::Day => 15 * 60,
        Period::Hour => 5 * 60,
    }
}

/// A uniformly random wait between now and shortly before the current period ends (a UTC day, or
/// an hour on the 1-day trial), or `None` when it's nearly over.
pub fn prefetch_delay(period: Period, now: i64) -> Option<Duration> {
    let left = period.secs() - now.rem_euclid(period.secs()) - prefetch_margin(period);
    if left <= 0 {
        return None;
    }
    Some(Duration::from_secs(rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..left as u64)))
}

/// Collects the next period's credential (`target`) for `plan` and keeps it for then. The proxy
/// runs it at a random time in the current period, so the time it's fetched, which the relay
/// sees, says nothing about when it's used.
pub fn prefetch(env: &Env, plan: Plan, target: u32, delay: Duration) -> Result<bool> {
    if period_now(plan, env.clock.now()) + 1 != target {
        return Ok(false);
    }
    std::thread::sleep(delay);
    // The computer may have slept through the end of the period.
    if period_now(plan, env.clock.now()) + 1 != target {
        return Ok(false);
    }
    let config = env.paired()?;
    let _lock = lock(env)?;
    if stored_credential(env, NEXT_CREDENTIAL, 1)?.is_some() {
        return Ok(false);
    }
    let (encoded, period) = fetch_credential(env, &config, 1)?;
    // The plan can change between periods; keep only what will be valid then.
    if period != target {
        return Ok(false);
    }
    env.keystore.set(NEXT_CREDENTIAL, &encoded)?;
    Ok(true)
}

/// The plan as DawonAI names it to users.
fn plan_label(id: &str) -> &str {
    match id {
        "day" => "1-day trial",
        "chat" => "Chat, monthly",
        "chat3" => "Chat, 3 months",
        "agent" => "Agent",
        other => other,
    }
}

pub struct Status {
    pub config: Option<Config>,
    pub plan: String,
    pub credential_today: bool,
    pub credential_tomorrow: bool,
    /// The held credential is a 1-day trial one, valid per hour rather than per day.
    pub hourly: bool,
    pub proxy: String,
    pub service: String,
    pub trust: Trust,
    pub keystore: String,
    /// The relay no longer knows this device: it went 30 days unused, or was removed in Account.
    pub device_removed: bool,
}

pub fn status(env: &Env) -> Result<Status> {
    let config = env.config()?;
    let keystore = env.keystore.describe();
    let Some(cfg) = config.clone() else {
        return Ok(Status {
            config,
            plan: "-".into(),
            credential_today: false,
            credential_tomorrow: false,
            hourly: false,
            proxy: "-".into(),
            service: crate::service::describe(env),
            trust: trust(env, None),
            keystore,
            device_removed: false,
        });
    };
    let credential_today = cached_credential(env).ok().flatten().is_some()
        || stored_credential(env, NEXT_CREDENTIAL, 0).ok().flatten().is_some();
    let credential_tomorrow = stored_credential(env, NEXT_CREDENTIAL, 1).ok().flatten().is_some();
    let hourly = held_plan(env).is_some_and(|p| Period::of(p) == Period::Hour);
    let mut device_removed = false;
    let plan = match env.device_key() {
        Err(e) => format!("unknown ({e})"),
        Ok(key) => match env
            .relay(&cfg, env.relay_timeout.min(Duration::from_secs(10)))
            .and_then(|r| r.account(&key, &cfg.device_id))
        {
            Ok(a) => match (&a.plan, a.credential_plan) {
                (Some(p), Some(_)) => format!("{} ({})", plan_label(p), a.status),
                (Some(p), None) => format!("{} ({}, not paid)", plan_label(p), a.status),
                (None, _) => format!("none ({})", a.status),
            },
            Err(Error::DeviceRemoved) => {
                device_removed = true;
                open_web_app(env, &cfg);
                "unknown (device removed, pair again)".into()
            }
            Err(Error::RelayUnreachable(_)) => "unknown (relay unreachable)".into(),
            Err(e) => format!("unknown ({e})"),
        },
    };
    Ok(Status {
        config,
        plan,
        credential_today,
        credential_tomorrow,
        hourly,
        proxy: match env.keystore.get(PROXY_KEY).ok().flatten().and_then(|k| proxy_health(env, &k)) {
            Some(_) => format!("running at {}", crate::proxy::url(env)),
            None => format!("not running (coding tools start it with `pai token`); {}", crate::proxy::url(env)),
        },
        service: crate::service::describe(env),
        trust: trust(env, Some(&cfg)),
        keystore,
        device_removed,
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
            match (self.hourly, self.credential_today, self.credential_tomorrow) {
                (false, true, true) => "yes, for today and tomorrow",
                (false, true, false) => "yes, for today",
                (false, false, true) => "none for today; tomorrow's is ready",
                (false, false, false) => "none for today",
                (true, true, true) => "yes, for this hour and the next (1-day trial)",
                (true, true, false) => "yes, for this hour (1-day trial)",
                (true, false, true) => "none for this hour; the next hour's is ready (1-day trial)",
                (true, false, false) => "none for this hour (1-day trial)",
            }
        )?;
        writeln!(f, "Relay:      {}", c.relay_url)?;
        writeln!(f, "API:        {}", c.api_url)?;
        if let Some(ca) = &self.trust.test_ca {
            writeln!(f, "TEST MODE:  trusting only {} and {} test pin(s). Not for real use", ca.display(), self.trust.api_pins.len())?;
        } else if !c.api_url.starts_with("https://") {
            writeln!(f, "Pins:       none needed (plain HTTP test gateway; nothing is encrypted)")?;
        } else if self.trust.api_pins.is_empty() {
            writeln!(f, "Pins:       none, so the proxy won't connect")?;
        } else {
            writeln!(f, "Pins:       {} for the API's key", self.trust.api_pins.len())?;
        }
        writeln!(f, "Proxy:      {}", self.proxy)?;
        writeln!(f, "Service:    {}", self.service)?;
        writeln!(f, "Keystore:   {}", self.keystore)?;
        if self.device_removed {
            writeln!(f)?;
            writeln!(f, "{}.", Error::DeviceRemoved)?;
            writeln!(f, "DawonAI web app: {}/", c.app_url())?;
        }
        Ok(())
    }
}

/// Removes this device from the account, then forgets it locally whatever the relay said.
/// Returns lines to show the user.
pub fn unpair(env: &Env) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    // Not env.config(), which a release build refuses for a test-mode pairing: unpairing is how
    // the user gets rid of one. The relay call below still uses this build's trust.
    let config = Config::load(&env.paths.config_file())?;
    // Unpairing has to work when the key can't be found or read, since it's how to recover.
    let key = match env.device_key() {
        Ok(k) => Some(k),
        Err(Error::NotPaired) => None,
        Err(e) => {
            notes.push(format!("Couldn't use this computer's device key ({e}), so the relay still lists it. Remove it under Devices in the web app."));
            None
        }
    };
    match (&config, &key) {
        (Some(cfg), Some(key)) => match env
            .relay(cfg, env.relay_timeout.min(Duration::from_secs(15)))
            .and_then(|r| r.delete_self(key, &cfg.device_id))
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
    // Before stopping the proxy, or the service would start it again.
    if crate::service::installed(env) {
        match crate::service::uninstall(env) {
            Ok(lines) => notes.extend(lines),
            Err(e) => notes.push(format!("Couldn't remove the login service ({e}). Run `pai service uninstall`.")),
        }
    }
    if let Ok(Some(k)) = env.keystore.get(PROXY_KEY) {
        stop_proxy(env, &k);
    }
    env.keystore.delete(DEVICE_KEY)?;
    env.keystore.delete(CREDENTIAL)?;
    env.keystore.delete(NEXT_CREDENTIAL)?;
    env.keystore.delete(PROXY_KEY)?;
    for path in [env.paths.config_file(), env.paths.lock_file(), env.paths.test_ca_file(), env.paths.web_app_opened_file()] {
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
    env.relay(&cfg, Duration::from_secs(5)).ok()?.account(&key, &cfg.device_id).ok()?.credential_plan
}
