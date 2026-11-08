use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pai::app::{self, Env};
use pai::error::Error;
use pai::service;
use pai::setup::{self, Tool};
use pai::update;

/// Connects coding tools to DawonAI through a local proxy, so they never hold a credential.
#[cfg(not(feature = "test-mode"))]
const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(feature = "test-mode")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (staging build, with test mode)");

#[derive(Parser)]
#[command(
    name = "pai",
    version = VERSION,
    after_help = "pai is part of DawonAI, a service of Dawon Science and Technology Laboratories LLC."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Link this machine using the code from Account > Set up a coding tool in the DawonAI web app
    Pair {
        /// The pairing code from the web app: 8 characters (dashes and case don't matter), then a dot
        /// and the account's pass key
        code: String,
        /// Relay URL [env: PAI_RELAY_URL] [default: this release's relay]
        #[arg(long)]
        relay: Option<String>,
        /// Model API URL [env: PAI_API_URL] [default: this release's model API]
        #[arg(long)]
        api: Option<String>,
        /// DawonAI web app URL, which pai opens if this device's link is removed [env: PAI_APP_URL] [default: app.<domain> for a relay at relay.<domain>]
        #[arg(long)]
        app: Option<String>,
        /// Name shown in the web app's device list [default: pai on <hostname>]
        #[arg(long)]
        name: Option<String>,
        /// Then set up this harness (as `pai setup <harness> --apply`), collect today's credential,
        /// start the proxy and show `pai status`. On a computer that's already paired, sets up the
        /// harness with that pairing.
        #[arg(long, value_enum, value_name = "HARNESS")]
        setup: Option<Tool>,
        // Test mode, in staging builds only (docs/TESTING.md). Hidden from --help on purpose.
        #[cfg(feature = "test-mode")]
        #[arg(long = "test-ca", hide = true, requires = "test_pins")]
        test_ca: Option<std::path::PathBuf>,
        #[cfg(feature = "test-mode")]
        #[arg(long = "test-pin", hide = true, requires = "test_ca")]
        test_pins: Vec<String>,
    },
    /// Start the local proxy if needed and print this machine's key for it. Coding tools run this.
    Token,
    /// Run the local proxy in the foreground. `pai token` starts it in the background.
    Proxy,
    /// Show whether this machine is paired, and the plan. Never prints secrets.
    Status,

    /// Save the account's pass key on a computer that's already paired, so it shares the account's
    /// day pass. The web app's `pai pair` command does the same.
    #[command(name = "pass-key")]
    PassKey {
        /// The pass key, the part after the dot in the web app's `pai pair` command
        key: String,
    },
    /// Remove this device from the account and delete its key and config
    Unpair,
    /// Run the local proxy at login, for tools that can't run `pai token` (macOS and Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print the config a coding tool needs and where the DawonAI skill goes, or write both with --apply
    Setup {
        #[arg(value_enum)]
        tool: Tool,
        /// Write the config, backing up any file it changes
        #[arg(long)]
        apply: bool,
        /// Put the tool's config back as it was before --apply, and remove pai's files
        #[arg(long, conflicts_with = "apply")]
        undo: bool,
        /// With `other`: print only a config file in this format
        #[arg(long, value_enum, conflicts_with_all = ["apply", "undo"])]
        format: Option<setup::ConfigFormat>,
        /// With `other`: write the config file to this path (models-json unless --format says)
        #[arg(long, value_name = "FILE", conflicts_with_all = ["apply", "undo"])]
        out: Option<std::path::PathBuf>,
    },
    /// Install the latest signed pai release, if it's newer. Release builds also do this daily on
    /// their own; PAI_AUTO_UPDATE=0 turns that off.
    Update {
        // The daily background check: no output.
        #[arg(long, hide = true)]
        auto: bool,
        // Run by the new binary after an update: rewrite set-up tools' config as this version would.
        #[arg(long, hide = true, conflicts_with = "auto")]
        refresh: bool,
    },
    // Run by release/build.sh on each binary it builds: checks the signed release in DIR with every
    // release key built into this binary. Hidden: not for users.
    #[command(hide = true)]
    CheckRelease { dir: std::path::PathBuf },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the login service and start the proxy with it. Safe to run again.
    Install,
    /// Stop the login service and remove it
    Uninstall,
    /// Show whether the login service is installed and the proxy is running
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pai: {e}");
            if matches!(e, Error::DeviceRemoved)
                && let Some(cfg) = Env::from_process().ok().and_then(|env| env.config().ok().flatten())
            {
                eprintln!("pai: DawonAI web app: {}/", cfg.app_url());
            }
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn run(command: Command) -> Result<(), Error> {
    let env = Env::from_process()?;
    let config = env.config().ok().flatten();
    if let Some(warning) = app::test_mode_warning(&app::trust(&env, config.as_ref())) {
        eprintln!("{warning}");
    }
    match command {
        Command::Pair {
            code,
            relay,
            api,
            app,
            name,
            setup: harness,
            #[cfg(feature = "test-mode")]
            test_ca,
            #[cfg(feature = "test-mode")]
            test_pins,
        } => {
            #[cfg(feature = "test-mode")]
            let test = test_ca.as_deref().map(|ca| (ca, test_pins.as_slice()));
            #[cfg(not(feature = "test-mode"))]
            let test = None;
            match app::pair(&env, &code, relay.as_deref(), api.as_deref(), app.as_deref(), name.as_deref(), test) {
                Ok(cfg) => {
                    println!("Paired. Account {}, device {}.", cfg.account_id, cfg.device_id);
                    if let Some(warning) = app::test_mode_warning(&app::trust(&env, Some(&cfg))) {
                        eprintln!("{warning}");
                    }
                }
                Err(Error::AlreadyPaired) if harness.is_some() || code.contains('.') => {
                    let account = env.config().ok().flatten().map(|c| c.account_id).unwrap_or_default();
                    println!(
                        "This computer is already paired with account {account}, so the new code stays unused. \
                         To pair it with another account, run `pai unpair` first."
                    );
                    if code.contains('.') {
                        println!("Saved the account's pass key: this computer now shares your account's day pass.");
                    }
                }
                Err(e) => return Err(e),
            }
            let Some(tool) = harness else {
                println!("Next: `pai setup <claude-code|codex|opencode|omp|hermes> --apply`, or `pai setup other` for any other harness.");
                return Ok(());
            };
            println!();
            // A custom harness gets its config printed last, after the status, so it's on screen
            // when the command finishes.
            let custom = tool == Tool::Other;
            if !custom {
                let target = setup::Target::resolve(&env, app::plan_hint(&env));
                print!("{}", setup::run(&env, tool, &target, true)?);
            }
            // Collects today's credential and starts the proxy now, so a problem shows here rather
            // than in the harness. The key isn't printed.
            app::token(&env)?;
            println!("Collected today's credential and started pai's proxy.\n");
            for note in service::set_up_under_wsl(&env) {
                println!("{note}");
            }
            print_status(&env)?;
            if custom {
                // Resolved after the first request, so the context window is the plan's.
                let target = setup::Target::resolve(&env, app::plan_hint(&env));
                println!();
                print!("{}", setup::run(&env, tool, &target, false)?);
            }
        }
        Command::PassKey { key } => {
            env.paired()?;
            app::save_pass_key(&env, &key)?;
            println!("Saved the account's pass key: this computer now shares your account's day pass.");
        }
        Command::Token => {
            let key = app::token(&env)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{key}")
                .and_then(|_| out.flush())
                .map_err(|e| Error::Io(format!("couldn't write the key: {e}")))?;
            update::clean_up(&env);
            update::refresh_settings(&env);
            update::start_if_due(&env);
        }
        Command::Update { refresh: true, .. } => {
            for (tool, result) in setup::refresh(&env, app::held_plan(&env).or_else(|| app::plan_hint(&env))) {
                match result {
                    Ok(true) => println!("Updated {}'s settings for this version of pai.", tool.display_name()),
                    Ok(false) => {}
                    Err(e) => eprintln!("pai: couldn't update {}'s settings: {e}", tool.display_name()),
                }
            }
        }
        Command::Update { auto, .. } => {
            if auto && !update::auto_enabled(&env) {
                return Ok(());
            }
            match update::run(&env) {
                Ok(update::Outcome::UpToDate) if !auto => println!("pai {} is the latest release.", update::VERSION),
                Ok(update::Outcome::UpToDate) => {}
                Ok(update::Outcome::Updated { from, to }) => {
                    // The new binary rewrites the settings, so they come out as the new version writes them.
                    let refreshed = std::process::Command::new(&env.pai_bin).args(["update", "--refresh"]).output();
                    if !auto {
                        println!("Updated pai from {from} to {to}.");
                        if let Ok(out) = refreshed {
                            print!("{}", String::from_utf8_lossy(&out.stdout));
                            eprint!("{}", String::from_utf8_lossy(&out.stderr));
                        }
                    }
                }
                Err(_) if auto => {}
                Err(e) => return Err(e),
            }
        }
        Command::CheckRelease { dir } => {
            for id in pai::update::check_release(&env, &dir)? {
                println!("ok {id}");
            }
        }
        Command::Proxy => pai::proxy::run(env)?,
        Command::Status => print_status(&env)?,

        Command::Unpair => {
            for line in app::unpair(&env)? {
                println!("{line}");
            }
        }
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(&env)?.iter().for_each(|l| println!("{l}")),
            ServiceAction::Uninstall => service::uninstall(&env)?.iter().for_each(|l| println!("{l}")),
            ServiceAction::Status => print!("{}", service::status(&env)),
        },
        Command::Setup { tool, undo: true, .. } => print!("{}", setup::undo(&env, tool)?),
        Command::Setup { tool, format, out, .. } if format.is_some() || out.is_some() => {
            if tool != Tool::Other {
                return Err(Error::Setup(format!(
                    "--format and --out are for `pai setup other`. Run `pai setup {} --apply` to set up {}.",
                    tool.cli_name(),
                    tool.display_name()
                )));
            }
            let target = setup::Target::resolve(&env, app::plan_hint(&env));
            let format = format.unwrap_or(setup::ConfigFormat::ModelsJson);
            match out {
                Some(path) => print!("{}", setup::other_write(&env, &target, format, &path)?),
                None => print!("{}", setup::other_config(&target, format)),
            }
        }
        Command::Setup { tool, apply, .. } => {
            let target = setup::Target::resolve(&env, app::plan_hint(&env));
            print!("{}", setup::run(&env, tool, &target, apply)?);
        }
    }
    Ok(())
}

/// `pai status`, with its first line in color on a terminal that shows it.
fn print_status(env: &Env) -> Result<(), Error> {
    use std::io::IsTerminal;
    let color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        // The old Windows console prints escape codes as text; Windows Terminal sets WT_SESSION.
        && (cfg!(not(windows)) || std::env::var_os("WT_SESSION").is_some());
    print!("{}", app::status(env)?.render(color));
    Ok(())
}
