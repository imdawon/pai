use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io_err};

pub const DEFAULT_RELAY_URL: &str = "https://relay.privacyai.example";
pub const DEFAULT_API_URL: &str = "https://api.privacyai.example";

/// Non-secret settings written by `pai pair`. Secrets live in the keystore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub relay_url: String,
    pub api_url: String,
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
