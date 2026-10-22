# pai

Lets coding tools use Privacy AI without a long-lived API key. Each tool runs `pai token`, which prints a day credential that expires at UTC midnight and can't be linked to your account. `pai` runs on macOS, Windows and Linux.

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
| `pai token [--refresh]` | Prints today's credential and nothing else. Reuses the cached one; otherwise collects a new blind-signed one from the relay. |
| `pai status` | Paired or not, account, plan, device, and whether there's a credential for today. Never prints secrets. |
| `pai unpair` | Removes the device from your account, then deletes its key, credential and config here, even if the relay can't be reached. |
| `pai setup <claude-code\|codex\|opencode\|omp> [--apply]` | Prints the tool's config. `--apply` writes it and backs up any file it changes to `<file>.pai-backup-<time>`. |

The relay and API default to `https://relay.privacyai.example` and `https://api.privacyai.example`. Override them at pairing with the flags or `PAI_RELAY_URL` / `PAI_API_URL`.

Errors go to stderr. `pai token` exits with 2 when not paired, 3 when the plan isn't paid, 4 when the device was removed, and 5 when the relay is unreachable.

## What's stored where

| What | Where |
|---|---|
| Device key, today's credential | OS keychain, service `privacy-ai`: macOS Keychain, Windows Credential Manager, or the Secret Service on Linux |
| Relay URL, API URL, device ID, account ID | `pai/config.json` in the OS config directory (`~/Library/Application Support`, `%APPDATA%`, `~/.config`) |
| Lock that stops parallel `pai token` calls fetching twice | `pai/token.lock` next to the config |

On a headless Linux machine without a Secret Service, `pai` stops with an error. Set `PAI_KEYSTORE=file` to keep the secrets in `~/.config/pai/secrets.json` instead, with mode 0600 and no encryption. `PAI_CONFIG_DIR` moves all of these files to one directory.

## Coding tools

Run `pai setup <tool>` to see the exact config, with the path to `pai`, your API URL and your plan's context window filled in. `--apply` writes it.

- **Claude Code**: `~/.claude/settings.json` gets `"apiKeyHelper": "<pai> token"`, `env.ANTHROPIC_BASE_URL`, the model `qwen3.8-27b` for every model alias, the context and output limits, and switches that keep Claude Code from sending traffic to Anthropic. Claude Code reruns the helper every hour and after a 401. An `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_BASE_URL` in the environment Claude Code starts in overrides these settings; `pai setup` warns about them.
- **Codex**: `~/.codex/config.toml` gets a `[model_providers.privacy-ai]` table using the Responses API, whose `[model_providers.privacy-ai.auth]` runs `pai token`. The model goes in a profile file, `~/.codex/privacy-ai.config.toml`, so start Codex with `codex --profile privacy-ai`.
- **OpenCode**: the plugin in `integrations/opencode/` goes to `~/.config/opencode/plugins/privacy-ai.ts`, and `opencode.json` gets `"model": "privacy-ai/qwen3.8-27b"`. The plugin registers the provider and runs `pai token`, again after a 401.
- **OMP**: the extension in `integrations/omp/` goes to `~/.omp/agent/extensions/privacy-ai.ts`. It registers the provider with `pai token` as its key command. Pick `privacy-ai/qwen3.8-27b` with `/model`.

## Build

```sh
cargo build --release
cargo test
```

`pai-proto` comes from `../pai-relay/crates/pai-proto`, so check out `pai-relay` next to this repo. Release builds for Linux target `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, which give a static binary. Release files are named `pai-<arch>-<target>`, for example `pai-aarch64-apple-darwin` or `pai-x86_64-pc-windows-msvc.exe`, next to a `SHA256SUMS` file.

`cargo test` runs a fake relay with real issuer keys. To run against a real relay started with `PAI_DEV_PAYMENTS=1`:

```sh
PAI_TEST_RELAY=http://127.0.0.1:8787 cargo test --test real_relay -- --ignored
```
