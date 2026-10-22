# pai

Lets coding tools use Privacy AI without holding any credential. Each tool talks to `pai`'s local proxy at `http://127.0.0.1:47821`, with a key that only works on this machine. The proxy adds a day credential, which expires at UTC midnight and can't be linked to your account, and sends the request to the model API. `pai` runs on macOS, Windows and Linux.

**Why a proxy.** The relay that carries your connection holds the IP address the model API's name points to, so it could get a valid certificate for that name and read what your tool sends. The proxy checks the model API's public key against pins built into `pai`, and refuses to connect to anything else, whatever certificate it shows. Coding tools can't do that check themselves.

The account key never reaches this machine. What `pai` holds is a device key, which can only fetch day credentials, and which you can remove from the web app at any time.

## Install

macOS and Linux:

```sh
curl -fsSL https://privacyai.example/pai/install.sh -o /tmp/pai-install.sh && sh /tmp/pai-install.sh
```

Windows (PowerShell):

```powershell
irm https://privacyai.example/pai/install.ps1 -OutFile $env:TEMP\pai-install.ps1; powershell -ExecutionPolicy Bypass -File $env:TEMP\pai-install.ps1
```

Both download the binary for your OS and CPU, check it against `SHA256SUMS`, and install to `~/.local/bin` or `%LOCALAPPDATA%\pai`. `PAI_DOWNLOAD_BASE` changes where they download from, and `PAI_INSTALL_DIR` where they install.

Or paste `skill/privacy-ai/SKILL.md` into your coding agent, and it does all of this for you.

## Commands

| Command | Does |
|---|---|
| `pai pair <code> [--relay URL] [--api URL] [--name NAME]` | Links this machine using the 8-character code from **Connect a device** in the web app. Makes an Ed25519 device key and keeps it in the OS keychain. |
| `pai token` | What coding tools run. Makes sure there's a credential for today, starts the local proxy if it isn't running (or is from an older `pai`), and prints the local key and nothing else. |
| `pai proxy` | Runs the local proxy in the foreground. It only answers on 127.0.0.1, only with the local key, and refuses requests from web pages. Once a day, at a random time, it collects tomorrow's credential. |
| `pai status` | Paired or not, account, plan, device, credentials for today and tomorrow, pins, and whether the proxy is running. Never prints secrets. |
| `pai unpair` | Stops the proxy and removes the login service, removes the device from your account, then deletes its keys, credentials and config here, even if the relay can't be reached. |
| `pai setup <claude-code\|codex\|opencode\|omp\|hermes> [--apply]` | Prints the tool's config. `--apply` writes it and backs up any file it changes to `<file>.pai-backup-<time>`. |
| `pai service <install\|uninstall\|status>` | Runs the proxy whenever you're logged in, for tools that can't run `pai token` themselves. A LaunchAgent on macOS, a systemd user service on Linux. Not on Windows yet. |

The relay and API default to `https://relay.privacyai.example` and `https://api.privacyai.example`. Override them at pairing with the flags or `PAI_RELAY_URL` / `PAI_API_URL`.

| Variable | Does |
|---|---|
| `PAI_PROXY_PORT` | The proxy's port, `47821` by default. Run `pai setup` again after changing it. |
| `PAI_PREFETCH=0` | Stops the proxy collecting tomorrow's credential ahead. |

Each release carries the pins for the model API's key, plus a spare for when the box's key changes. Building or testing `pai` against a staging service is covered in `docs/TESTING.md`.

Errors go to stderr. `pai token` exits with 2 when not paired, 3 when the plan isn't paid, 4 when the device was removed, and 5 when the relay is unreachable.

## What's stored where

| What | Where |
|---|---|
| Device key, today's and tomorrow's credentials, the local proxy key | OS keychain, service `privacy-ai`: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux |
| Relay URL, API URL, device ID, account ID | `pai/config.json` in the OS config directory (`~/Library/Application Support`, `%APPDATA%`, `~/.config`) |
| Lock that stops parallel `pai token` calls fetching twice | `pai/token.lock` next to the config |

On a headless Linux machine without a Secret Service, `pai` stops with an error. Set `PAI_KEYSTORE=file` to keep the secrets in `~/.config/pai/secrets.json` instead, with mode 0600 and no encryption. `PAI_CONFIG_DIR` moves all of these files to one directory.

**Why tomorrow's credential is collected ahead.** Collecting a credential is signed with this device's key, so the relay knows which account asked. If `pai` collected it just before your tool used it, someone watching the relay could match the two by time and tie that day's requests to your account. Collecting it at a random time the day before breaks that link. Set `PAI_PREFETCH=0` to turn this off.

## Coding tools

Run `pai setup <tool>` to see the exact config, with the path to `pai`, the proxy's address and your plan's context window filled in. `--apply` writes it. Every tool talks to the proxy, never to the model API directly.

- **Claude Code**: `~/.claude/settings.json` gets `"apiKeyHelper": "<pai> token"`, `env.ANTHROPIC_BASE_URL`, the model `qwen3.8-27b` for every model alias, the context and output limits, and switches that keep Claude Code from sending traffic to Anthropic. Claude Code reruns the helper every hour and after a 401. An `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_BASE_URL` in the environment Claude Code starts in overrides these settings; `pai setup` warns about them.
- **Codex**: `~/.codex/config.toml` gets a `[model_providers.privacy-ai]` table using the Responses API, whose `[model_providers.privacy-ai.auth]` runs `pai token`. The model goes in a profile file, `~/.codex/privacy-ai.config.toml`, so start Codex with `codex --profile privacy-ai`.
- **OpenCode**: the plugin in `integrations/opencode/` goes to `~/.config/opencode/plugins/privacy-ai.ts`, and `opencode.json` gets `"model": "privacy-ai/qwen3.8-27b"`. The plugin registers the provider and runs `pai token`, again after a 401.
- **OMP**: the extension in `integrations/omp/` goes to `~/.omp/agent/extensions/privacy-ai.ts`. It registers the provider with `pai token` as its key command. Pick `privacy-ai/qwen3.8-27b` with `/model`.
- **Hermes Agent**: `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`) gets a `privacy-ai` entry under `providers:`, with the local key, between a `# privacy-ai` and an `# end privacy-ai` comment. `pai` edits only those lines and leaves the rest of the file as it was. Hermes doesn't run `pai token`, so `--apply` also runs `pai service install`. Your default model stays as it is: pick Privacy AI with `/model custom:privacy-ai:qwen3.8-27b` (add `--global` to keep it), or with `hermes model`.

## Build

```sh
cargo build --release
cargo test
```

`pai-proto` comes from `../pai-relay/crates/pai-proto`, so check out `pai-relay` next to this repo.

`release/build.sh` makes a release on a Mac with Docker running. It builds macOS natively, Linux as static musl binaries in `rust:1.95-alpine`, and Windows with `cargo-xwin` (static CRT, so no Visual C++ runtime needed). It writes to `dist/`, which is what gets hosted at `https://<domain>/pai/`:

| Path | Holds |
|---|---|
| `dist/<version>/` | `pai-{aarch64,x86_64}-apple-darwin`, `pai-{aarch64,x86_64}-unknown-linux-musl`, `pai-{x86_64,aarch64}-pc-windows-msvc.exe`, `privacy-ai-skill.zip`, `opencode-privacy-ai.ts`, `omp-privacy-ai.ts` and `SHA256SUMS` |
| `dist/latest/` | A copy of the newest version, which the installers download from |
| `dist/index.html`, `install.sh`, `install.ps1` | The release page and the installers |

`release/index.sh <version>` redoes only the page. The binaries aren't signed; `release/SIGNING.md` covers what signing takes and what users can do meanwhile.

`cargo test` runs a fake relay with real issuer keys. To run against a real relay started with `PAI_DEV_PAYMENTS=1`:

```sh
PAI_TEST_RELAY=http://127.0.0.1:8787 cargo test --test real_relay -- --ignored
```
