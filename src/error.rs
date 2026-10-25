use std::fmt;

/// Every failure `pai` reports. The `Display` text is what the user reads on stderr, so it says
/// what happened and what to do, never internals.
#[derive(Debug)]
pub enum Error {
    NotPaired,
    AlreadyPaired,
    BadPairingCode,
    PairingRejected(String),
    NotPaid { detail: String },
    Banned,
    DeviceRemoved,
    IssuanceLimit { retry_after: Option<u64> },
    ClockSkew,
    WrongDay(String),
    RelayUnreachable(String),
    Relay { status: u16, message: String },
    BadIssuerResponse(String),
    KeychainUnavailable(String),
    #[cfg(target_os = "macos")]
    KeychainLocked,
    DeviceKeyMissing,
    Keystore(String),
    Io(String),
    Setup(String),
    Update(String),
}

impl Error {
    /// Distinct exit codes let scripts and the skill branch without parsing text.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::NotPaired => 2,
            Error::NotPaid { .. } | Error::Banned => 3,
            Error::DeviceRemoved => 4,
            Error::RelayUnreachable(_) => 5,
            _ => 1,
        }
    }

    /// The keystore itself can't be used, which no command should mistake for "nothing stored".
    pub fn is_keystore_unavailable(&self) -> bool {
        match self {
            Error::KeychainUnavailable(_) => true,
            #[cfg(target_os = "macos")]
            Error::KeychainLocked => true,
            _ => false,
        }
    }
}

/// Relay messages are full sentences; they get embedded mid-sentence here.
fn clause(msg: &str) -> &str {
    msg.trim().trim_end_matches('.')
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotPaired => write!(
                f,
                "not paired: open Account > Set up a coding tool in the DawonAI web app, then run `pai pair <code>`"
            ),
            Error::AlreadyPaired => {
                write!(
                    f,
                    "this machine is already paired: run `pai unpair` first to pair it again"
                )
            }
            Error::BadPairingCode => write!(
                f,
                "that doesn't look like a pairing code: it's 8 letters and digits, shown in the DawonAI web app under Account > Set up a coding tool"
            ),
            Error::PairingRejected(msg) => write!(
                f,
                "pairing failed: {}. Pairing codes work once and expire after 10 minutes; get a new one from Account > Set up a coding tool",
                clause(msg)
            ),
            Error::NotPaid { detail } => write!(
                f,
                "plan not paid ({}): choose a plan in the DawonAI web app, then try again",
                clause(detail)
            ),
            Error::Banned => write!(f, "this account is suspended and can't get new credentials"),
            Error::DeviceRemoved => write!(
                f,
                "device removed, pair again: this computer's DawonAI device link was removed, because it went unused for 30 days \
                 or because it was removed in Account in the DawonAI web app. Run `pai unpair`, then pair again from \
                 Account > Set up a coding tool in the DawonAI web app"
            ),
            Error::IssuanceLimit { retry_after } => {
                write!(
                    f,
                    "this device has already collected 3 credentials today, the daily limit"
                )?;
                match retry_after {
                    Some(secs) => write!(f, "; it can get a new one in {}", human_duration(*secs)),
                    None => write!(f, "; it can get a new one after midnight UTC"),
                }
            }
            Error::WrongDay(msg) => write!(
                f,
                "the relay refused today's date: {}. Check this computer's date and time zone",
                clause(msg)
            ),
            Error::ClockSkew => write!(
                f,
                "this computer's clock is more than 2 minutes off; fix the system time and try again"
            ),
            Error::RelayUnreachable(url) => {
                write!(
                    f,
                    "relay unreachable: couldn't connect to {url}. Check your connection and try again"
                )
            }
            Error::Relay { status, message } => {
                write!(
                    f,
                    "the relay refused the request ({status}): {}",
                    clause(message)
                )
            }
            Error::BadIssuerResponse(msg) => {
                write!(f, "the relay sent an invalid credential: {msg}")
            }
            #[cfg(target_os = "macos")]
            Error::KeychainUnavailable(detail) => write!(
                f,
                "pai couldn't read your login keychain ({detail}), where an earlier pai kept this computer's device key. \
                 Run this command once in a Terminal window on the Mac, or run `pai unpair` and pair again"
            ),
            #[cfg(target_os = "macos")]
            Error::KeychainLocked => write!(
                f,
                "your login keychain is locked, and an earlier pai kept this computer's device key in it. Unlock it and run \
                 the same command again (over SSH: `security unlock-keychain`), or run `pai unpair` and pair again"
            ),
            #[cfg(not(target_os = "macos"))]
            Error::KeychainUnavailable(detail) => write!(
                f,
                "pai couldn't read the OS credential store ({detail}), where pai before 0.1.6 kept this computer's \
                 device key. On Linux that's the Secret Service: run this command once in your desktop session. \
                 Or run `pai unpair` and pair again"
            ),
            Error::DeviceKeyMissing => write!(
                f,
                "this computer is paired, but pai can't find its device key. Run `pai unpair`, then pair again \
                 from Account > Set up a coding tool in the DawonAI web app"
            ),
            Error::Keystore(msg) => write!(f, "couldn't use the keychain: {msg}"),
            Error::Io(msg) => write!(f, "{msg}"),
            Error::Setup(msg) => write!(f, "{msg}"),
            Error::Update(msg) => write!(f, "couldn't update pai: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

fn human_duration(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    match (h, m) {
        (0, 0) => format!("{secs} seconds"),
        (0, m) => format!("{m} minutes"),
        (h, m) => format!("{h} h {m} min"),
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn io_err(what: &str, path: &std::path::Path, e: std::io::Error) -> Error {
    Error::Io(format!("couldn't {what} {}: {e}", path.display()))
}
