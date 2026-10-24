use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io_err};

// A release build sets these from its domain (release/build.sh); other builds use the placeholder.
pub const DEFAULT_RELAY_URL: &str = match option_env!("PAI_RELEASE_RELAY_URL") {
    Some(url) => url,
    None => "https://relay.privacyai.example",
};
pub const DEFAULT_API_URL: &str = match option_env!("PAI_RELEASE_API_URL") {
    Some(url) => url,
    None => "https://api.privacyai.example",
};
pub const DEFAULT_APP_URL: &str = match option_env!("PAI_RELEASE_APP_URL") {
    Some(url) => url,
    None => "https://app.privacyai.example",
};

/// Non-secret settings written by `pai pair`. Secrets live in the keystore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub relay_url: String,
    pub api_url: String,
    /// The DawonAI web app, which `pai` opens when this device's link is gone. Pairings made
    /// before it was kept get it from the relay URL: see `Config::app_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_url: Option<String>,
    pub device_id: String,
    pub account_id: String,
    /// Test mode, for staging only: see docs/TESTING.md. Absent for real use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestMode>,
}

/// A test CA trusted instead of the public certificate authorities, for the relay and the API,
/// and pins that replace the release's. It never turns pin checking off.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestMode {
    pub ca: PathBuf,
    pub api_pins: Vec<String>,
}

impl Config {
    /// The DawonAI web app: the one saved at pairing, or the one the relay URL implies.
    pub fn app_url(&self) -> String {
        self.app_url.clone().unwrap_or_else(|| app_url_for_relay(&self.relay_url))
    }

    pub fn load(path: &Path) -> Result<Option<Config>> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("read", path, e)),
        };
        serde_json::from_str(&text).map(Some).map_err(|e| {
            Error::Io(format!(
                "{} is not valid ({e}); run `pai unpair` and pair again",
                path.display()
            ))
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
        }
        let mut text = serde_json::to_string_pretty(self).expect("config serializes");
        text.push('\n');
        fs::write(path, text).map_err(|e| io_err("write", path, e))
    }
}

pub fn trim_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// The web app lives at `app.<domain>` beside the relay's `relay.<domain>`, on the same scheme
/// and port. Any other relay URL gets the default web app.
pub fn app_url_for_relay(relay_url: &str) -> String {
    let derived = reqwest::Url::parse(relay_url).ok().and_then(|mut url| {
        let domain = url.host_str()?.strip_prefix("relay.")?.to_string();
        if domain.is_empty() {
            return None;
        }
        url.set_host(Some(&format!("app.{domain}"))).ok()?;
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        Some(trim_url(url.as_str()))
    });
    derived.unwrap_or_else(|| DEFAULT_APP_URL.to_string())
}

/// Whether `pai` will hand `url` to the OS to open. Only http(s) addresses, and none of the
/// characters `cmd /c start` treats specially on Windows, so a URL from config.json can only
/// ever open a web page.
pub fn openable_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "https" | "http")
        && parsed.host_str().is_some()
        && url.chars().all(|c| c.is_ascii_graphic() && !"&|<>^\"%`'".contains(c))
}

/// Where `pai` keeps its files. Tests point every path at a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    /// Only used with `PAI_KEYSTORE=file`.
    pub secrets_file: PathBuf,
    /// The user's home, where coding tools keep their config.
    pub home: PathBuf,
}

impl Paths {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    /// Where `pai pair --test-ca` keeps its copy of the test CA.
    pub fn test_ca_file(&self) -> PathBuf {
        self.config_dir.join("test-ca.pem")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.config_dir.join("token.lock")
    }

    /// Holds the UTC day `pai` last opened the web app because this device's link was gone.
    pub fn web_app_opened_file(&self) -> PathBuf {
        self.config_dir.join("web-app-opened")
    }

    pub fn from_env() -> Result<Paths> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Io("couldn't find your home directory".into()))?;
        if let Some(dir) = std::env::var_os("PAI_CONFIG_DIR").filter(|v| !v.is_empty()) {
            let dir = PathBuf::from(dir);
            return Ok(Paths {
                secrets_file: dir.join("secrets.json"),
                config_dir: dir,
                home,
            });
        }
        let config_dir = dirs::config_dir()
            .ok_or_else(|| Error::Io("couldn't find the OS config directory".into()))?
            .join("pai");
        let secrets_file = home.join(".config").join("pai").join("secrets.json");
        Ok(Paths {
            config_dir,
            secrets_file,
            home,
        })
    }
}
