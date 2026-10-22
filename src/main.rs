use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use pai::app::{self, Env};
use pai::error::Error;
use pai::setup::{self, Tool};

/// Connects coding tools to Privacy AI through a local proxy, so they never hold a credential.
#[derive(Parser)]
#[command(name = "pai", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Link this machine using the code from Connect a device in the web app
    Pair {
        /// The 8-character pairing code (dashes and case don't matter)
        code: String,
        /// Relay URL [env: PAI_RELAY_URL] [default: https://relay.privacyai.example]
        #[arg(long)]
        relay: Option<String>,
        /// Model API URL [env: PAI_API_URL] [default: https://api.privacyai.example]
        #[arg(long)]
        api: Option<String>,
        /// Name shown in the web app's device list [default: pai on <hostname>]
        #[arg(long)]
        name: Option<String>,
        /// Pin for a test box's API key (base64 SHA-256 of its public key). Releases carry their own
        #[arg(long = "pin")]
        pins: Vec<String>,
    },
    /// Start the local proxy if needed and print this machine's key for it. Coding tools run this.
    Token,
    /// Run the local proxy in the foreground. `pai token` starts it in the background.
    Proxy,
    /// Show whether this machine is paired, and the plan. Never prints secrets.
    Status,

    /// Remove this device from the account and delete its key and config
    Unpair,
    /// Print the config a coding tool needs, or write it with --apply
    Setup {
        #[arg(value_enum)]
        tool: Tool,
        /// Write the config, backing up any file it changes
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pai: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn run(command: Command) -> Result<(), Error> {
    let env = Env::from_process()?;
    match command {
        Command::Pair {
            code,
            relay,
            api,
            name,
            pins,
        } => {
            let cfg = app::pair(
                &env,
                &code,
                relay.as_deref(),
                api.as_deref(),
                name.as_deref(),
                &pins,
            )?;
            println!(
                "Paired. Account {}, device {}.",
                cfg.account_id, cfg.device_id
            );
            println!("Next: `pai setup <claude-code|codex|opencode|omp> --apply`.");
        }
        Command::Token => {
            let key = app::token(&env)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{key}")
                .and_then(|_| out.flush())
                .map_err(|e| Error::Io(format!("couldn't write the key: {e}")))?;
        }
        Command::Proxy => pai::proxy::run(env)?,
        Command::Status => print!("{}", app::status(&env)?),

        Command::Unpair => {
            for line in app::unpair(&env)? {
                println!("{line}");
            }
        }
        Command::Setup { tool, apply } => {
            let target = setup::Target::resolve(&env, app::plan_hint(&env));
            print!("{}", setup::run(&env, tool, &target, apply)?);
        }
    }
    Ok(())
}
