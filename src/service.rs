//! `pai service`: runs the local proxy at login, for tools that can't run `pai token` themselves.
//! A LaunchAgent on macOS, a systemd user unit on Linux, and on Windows an entry in the user's
//! Run key that starts the proxy through `conhost.exe --headless`, so no console window opens.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::{self, Env};
use crate::error::{Error, Result, io_err};

pub const LABEL: &str = "ai.privacyai.pai-proxy";
pub const UNIT: &str = "pai-proxy.service";
/// The per-user Run key Windows starts programs from at login. Writing it needs no administrator.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// The value under `RUN_KEY`, the name Task Manager's Startup apps shows.
pub const RUN_VALUE: &str = "DawonAI pai proxy";

/// Carried into the service so it uses the same pairing, keystore and port as this shell.
const CARRIED_VARS: [&str; 4] = ["PAI_CONFIG_DIR", "PAI_KEYSTORE", "PAI_PROXY_PORT", "PAI_PREFETCH"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
    WindowsRun,
    Unsupported,
}

impl Manager {
    pub fn current() -> Manager {
        if cfg!(target_os = "macos") {
            Manager::Launchd
        } else if cfg!(target_os = "linux") {
            Manager::Systemd
        } else if cfg!(windows) {
            Manager::WindowsRun
        } else {
            Manager::Unsupported
        }
    }
}

/// How `pai` runs launchctl and systemctl. Tests use `Record`, which runs nothing and reports
/// success, so they never touch the real service manager.
#[derive(Debug, Clone)]
pub enum Commands {
    System,
    Record(Arc<Mutex<Vec<String>>>),
}

pub struct Ran {
    pub ok: bool,
    pub output: String,
}

impl Commands {
    pub fn record() -> Commands {
        Commands::Record(Arc::default())
    }

    pub fn recorded(&self) -> Vec<String> {
        match self {
            Commands::System => Vec::new(),
            Commands::Record(log) => log.lock().unwrap().clone(),
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Ran {
        match self {
            Commands::Record(log) => {
                log.lock().unwrap().push(std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" "));
                Ran { ok: true, output: String::new() }
            }
            Commands::System => match std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).output() {
                Ok(o) => Ran {
                    ok: o.status.success(),
                    output: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string(),
                },
                Err(e) => Ran { ok: false, output: format!("couldn't run {program}: {e}") },
            },
        }
    }

    /// Starts `program` without waiting for it, for handing a URL to the OS's browser opener.
    /// Returns whether it started.
    pub(crate) fn start(&self, program: &str, args: &[&str]) -> bool {
        match self {
            Commands::Record(_) => self.run(program, args).ok,
            Commands::System => {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    cmd.creation_flags(CREATE_NO_WINDOW);
                }
                match cmd.spawn() {
                    Ok(mut child) => {
                        // Reaped in the background, so a long-running proxy keeps no zombie.
                        std::thread::spawn(move || child.wait());
                        true
                    }
                    Err(_) => false,
                }
            }
        }
    }

    fn must(&self, program: &str, args: &[&str]) -> Result<()> {
        let ran = self.run(program, args);
        if ran.ok {
            return Ok(());
        }
        Err(Error::Setup(format!(
            "`{program} {}` failed{}",
            args.join(" "),
            if ran.output.is_empty() { String::new() } else { format!(": {}", ran.output) }
        )))
    }
}

fn unsupported() -> Error {
    Error::Setup(
        "pai can't run its proxy as a login service on this OS yet. Run `pai token` once after you log in to start it, \
         or set the tool up with `pai token` as its key command"
            .into(),
    )
}

/// Whether this Linux runs under Windows Subsystem for Linux. WSL sets `WSL_DISTRO_NAME` in its
/// shells, and its kernel names Microsoft in its release string.
pub fn wsl(env: &Env) -> bool {
    if env.var("WSL_DISTRO_NAME").is_some() {
        return true;
    }
    matches!(env.commands, Commands::System)
        && cfg!(target_os = "linux")
        && fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"))
}

/// Whether systemd runs this Linux, the check systemd's own tools use. WSL leaves it off unless
/// `/etc/wsl.conf` turns it on. Tests say no with `PAI_TEST_NO_SYSTEMD=1`.
pub fn systemd_running(env: &Env) -> bool {
    match env.commands {
        Commands::System => std::path::Path::new("/run/systemd/system").is_dir(),
        Commands::Record(_) => env.var("PAI_TEST_NO_SYSTEMD") != Some("1"),
    }
}

/// How to turn systemd on under WSL, for `pai service install` and `pai pair --setup`.
pub const WSL_SYSTEMD_HOWTO: &str = "WSL has systemd turned off, and pai's login service needs it. To turn it on, run \
     `printf '[boot]\\nsystemd=true\\n' | sudo tee -a /etc/wsl.conf` here, then `wsl --shutdown` in PowerShell, \
     then open WSL again and run `pai service install`. Until then, your harness starts pai's proxy itself";

fn no_systemd(env: &Env) -> Error {
    if wsl(env) {
        Error::Setup(WSL_SYSTEMD_HOWTO.into())
    } else {
        Error::Setup(
            "this Linux doesn't run systemd, so pai can't install its login service here. \
             Run `pai token` once after you log in to start the proxy, or set the tool up with `pai token` as its key command"
                .into(),
        )
    }
}

/// Where the service file goes. Under `Paths.home`, so tests keep it in a temp dir. Windows keeps
/// the service in the registry, not a file.
pub fn file(env: &Env) -> Option<PathBuf> {
    match env.service_manager {
        Manager::Launchd => Some(env.paths.home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))),
        Manager::Systemd => Some(
            env.var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| env.paths.home.join(".config"))
                .join("systemd")
                .join("user")
                .join(UNIT),
        ),
        Manager::WindowsRun | Manager::Unsupported => None,
    }
}

/// Whether this OS can run the proxy at login.
pub fn supported(env: &Env) -> bool {
    env.service_manager != Manager::Unsupported
}

pub fn installed(env: &Env) -> bool {
    match env.service_manager {
        Manager::WindowsRun => windows_installed(env),
        _ => file(env).is_some_and(|f| f.exists()),
    }
}

/// Whether the Run key has pai's value. Tests read what they recorded instead of the registry.
fn windows_installed(env: &Env) -> bool {
    match &env.commands {
        Commands::System => env.commands.run("reg", &["query", RUN_KEY, "/v", RUN_VALUE]).ok,
        Commands::Record(log) => log
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|c| c.starts_with("reg add ") || c.starts_with("reg delete "))
            .is_some_and(|c| c.starts_with("reg add ")),
    }
}

/// The command the Run key starts: pai's proxy under a console host with no window. Without
/// `conhost.exe --headless` (Windows 10 1809 and later), Windows would open a console window for
/// the proxy at every login and keep it open.
pub fn run_command(env: &Env) -> String {
    let root = env.var("SystemRoot").unwrap_or(r"C:\Windows");
    format!(r#""{root}\System32\conhost.exe" --headless "{}" proxy"#, env.pai_bin.to_string_lossy())
}

fn carried(env: &Env) -> Vec<(&'static str, &str)> {
    CARRIED_VARS.iter().filter_map(|&k| env.var(k).map(|v| (k, v))).collect()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plist(env: &Env) -> String {
    let mut vars = String::new();
    let carried = carried(env);
    if !carried.is_empty() {
        vars.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in carried {
            vars.push_str(&format!("        <key>{k}</key>\n        <string>{}</string>\n", xml(v)));
        }
        vars.push_str("    </dict>\n");
    }
    // KeepAlive restarts it only after a failure: the proxy exits cleanly when another proxy
    // has the port or when told to stop, and restarting it then would loop.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>proxy</string>
    </array>
{vars}    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
</dict>
</plist>
"#,
        xml(&env.pai_bin.to_string_lossy())
    )
}

/// systemd unit values: quoted, with `%` escaped since systemd expands specifiers.
fn systemd_word(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

pub fn unit(env: &Env) -> String {
    let mut out = format!(
        "[Unit]\nDescription=pai local proxy for DawonAI\n\n[Service]\nExecStart={} proxy\nRestart=on-failure\nRestartSec=5\n",
        systemd_word(&env.pai_bin.to_string_lossy())
    );
    for (k, v) in carried(env) {
        out.push_str(&format!("Environment={}\n", systemd_word(&format!("{k}={v}"))));
    }
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn domain() -> String {
    format!("gui/{}", uid())
}

fn wait_for_proxy(env: &Env) -> bool {
    if !matches!(env.commands, Commands::System) {
        return false;
    }
    let Ok(Some(key)) = env.keystore.get(crate::keystore::PROXY_KEY) else {
        return false;
    };
    for _ in 0..50 {
        if app::proxy_health(env, &key).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Installs the service and starts it, replacing a proxy `pai token` started. Safe to run again.
pub fn install(env: &Env) -> Result<Vec<String>> {
    if env.service_manager == Manager::WindowsRun {
        return install_windows(env);
    }
    let path = file(env).ok_or_else(unsupported)?;
    if env.service_manager == Manager::Systemd && !systemd_running(env) {
        return Err(no_systemd(env));
    }
    let config = env.paired()?;
    crate::proxy::upstream_client(env, &config)?;
    let key = app::local_key(env)?;
    let text = match env.service_manager {
        Manager::Launchd => plist(env),
        _ => unit(env),
    };
    let mut notes = Vec::new();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_err("create", dir, e))?;
    }
    fs::write(&path, text).map_err(|e| io_err("write", &path, e))?;
    notes.push(format!("Wrote {}", path.display()));

    app::stop_proxy_and_wait(env, &key);
    let run = &env.commands;
    match env.service_manager {
        Manager::Launchd => {
            let target = format!("{}/{LABEL}", domain());
            run.run("launchctl", &["bootout", &target]);
            let path = path.to_string_lossy();
            // bootout returns before launchd has let go of the label, so bootstrap can fail briefly.
            let mut tries = 0;
            loop {
                let ran = run.run("launchctl", &["bootstrap", &domain(), &path]);
                if ran.ok {
                    break;
                }
                tries += 1;
                if tries == 10 {
                    return Err(Error::Setup(format!(
                        "launchd wouldn't load {path} ({}). Run `pai service install` again, or `pai service uninstall` to remove it",
                        ran.output
                    )));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        _ => {
            run.must("systemctl", &["--user", "daemon-reload"])?;
            run.must("systemctl", &["--user", "enable", UNIT])?;
            run.must("systemctl", &["--user", "restart", UNIT])?;
        }
    }
    let url = crate::proxy::url(env);
    match env.commands {
        Commands::System if wait_for_proxy(env) => notes.push(format!("The proxy runs at {url} now, and starts when you log in.")),
        Commands::System => notes.push(format!(
            "Installed, but the proxy isn't answering at {url} yet. Run `pai service status` in a moment, or `pai proxy` to see why"
        )),
        Commands::Record(_) => notes.push(format!("The proxy will run at {url}, and start when you log in.")),
    }
    Ok(notes)
}

/// The Run key has no place for environment variables, so a service started from it would use
/// another pairing, keystore or port than this shell. pai says so instead of installing one.
fn install_windows(env: &Env) -> Result<Vec<String>> {
    if let Some((k, _)) = carried(env).first() {
        return Err(Error::Setup(format!(
            "{k} is set in this shell, and the Windows login service can't carry it, so the service would use another \
             setting. Unset {k} and run `pai service install` again, or keep using `pai token` to start the proxy"
        )));
    }
    let config = env.paired()?;
    crate::proxy::upstream_client(env, &config)?;
    let key = app::local_key(env)?;
    let command = run_command(env);
    env.commands.must("reg", &["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f"])?;
    let mut notes = vec![format!("Added \"{RUN_VALUE}\" to {RUN_KEY}")];
    app::stop_proxy_and_wait(env, &key);
    let url = crate::proxy::url(env);
    match env.commands {
        Commands::System => {
            app::spawn_detached(env, &["proxy"]).map_err(|e| Error::Io(format!("couldn't start pai's local proxy: {e}")))?;
            if wait_for_proxy(env) {
                notes.push(format!("The proxy runs at {url} now, and starts when you log in."));
            } else {
                notes.push(format!(
                    "Installed, but the proxy isn't answering at {url} yet. Run `pai service status` in a moment, or `pai proxy` to see why"
                ));
            }
        }
        Commands::Record(_) => notes.push(format!("The proxy will run at {url}, and start when you log in.")),
    }
    Ok(notes)
}

/// Under WSL, `pai pair --setup` also installs the login service, so systemd restarts the proxy
/// if it stops and keeps what it prints in the journal (`journalctl --user -u pai-proxy`). Returns
/// what to tell the person; nothing when this isn't WSL or the service is already there.
pub fn set_up_under_wsl(env: &Env) -> Vec<String> {
    if !wsl(env) || env.service_manager != Manager::Systemd || installed(env) {
        return Vec::new();
    }
    if !systemd_running(env) {
        return vec![format!("{WSL_SYSTEMD_HOWTO}.")];
    }
    match install(env) {
        Ok(notes) => notes,
        Err(e) => vec![format!("Couldn't install pai's login service ({e}). Run `pai service install` to try again.")],
    }
}

pub fn uninstall(env: &Env) -> Result<Vec<String>> {
    if env.service_manager == Manager::WindowsRun {
        if !windows_installed(env) {
            return Ok(vec!["The login service isn't installed.".into()]);
        }
        env.commands.must("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])?;
        // As launchctl bootout and systemctl disable --now do, stop the proxy the service ran.
        if let Ok(Some(key)) = env.keystore.get(crate::keystore::PROXY_KEY) {
            app::stop_proxy_and_wait(env, &key);
        }
        return Ok(vec![format!("Removed the login service (\"{RUN_VALUE}\" in {RUN_KEY}).")]);
    }
    let path = file(env).ok_or_else(unsupported)?;
    if !path.exists() {
        return Ok(vec!["The login service isn't installed.".into()]);
    }
    let run = &env.commands;
    match env.service_manager {
        Manager::Launchd => {
            run.run("launchctl", &["bootout", &format!("{}/{LABEL}", domain())]);
        }
        _ => {
            run.run("systemctl", &["--user", "disable", "--now", UNIT]);
        }
    }
    fs::remove_file(&path).map_err(|e| io_err("remove", &path, e))?;
    if env.service_manager == Manager::Systemd {
        run.run("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(vec![format!("Removed the login service ({}).", path.display())])
}

/// Restarts the service's proxy. False when it isn't installed or the service manager refused.
pub fn restart(env: &Env) -> bool {
    if !installed(env) {
        return false;
    }
    match env.service_manager {
        Manager::Launchd => env.commands.run("launchctl", &["kickstart", "-k", &format!("{}/{LABEL}", domain())]).ok,
        Manager::Systemd => env.commands.run("systemctl", &["--user", "restart", UNIT]).ok,
        // The Run key only starts the proxy at login, so `pai token` starts it as before.
        Manager::WindowsRun | Manager::Unsupported => false,
    }
}

/// One line for `pai status` and `pai service status`.
pub fn describe(env: &Env) -> String {
    if env.service_manager == Manager::WindowsRun {
        return if windows_installed(env) {
            format!("installed, starts at login (\"{RUN_VALUE}\" in {RUN_KEY})")
        } else {
            "not installed".into()
        };
    }
    let Some(path) = file(env) else {
        return "not available on this OS".into();
    };
    if !path.exists() {
        return "not installed".into();
    }
    let loaded = match env.service_manager {
        Manager::Launchd => env.commands.run("launchctl", &["print", &format!("{}/{LABEL}", domain())]).ok,
        _ => env.commands.run("systemctl", &["--user", "is-enabled", "--quiet", UNIT]).ok,
    };
    if loaded {
        format!("installed, starts at login ({})", path.display())
    } else {
        format!("installed but not loaded; run `pai service install` again ({})", path.display())
    }
}

pub fn status(env: &Env) -> String {
    let proxy = match env.keystore.get(crate::keystore::PROXY_KEY).ok().flatten().and_then(|k| app::proxy_health(env, &k)) {
        Some(_) => format!("running at {}", crate::proxy::url(env)),
        None => format!("not running at {}", crate::proxy::url(env)),
    };
    format!("Service:    {}\nProxy:      {proxy}\n", describe(env))
}
