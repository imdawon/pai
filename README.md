# pai

Lets coding tools use DawonAI without holding any credential. Each tool talks to `pai`'s local proxy at `http://127.0.0.1:47821`, with a key that only works on this machine. The proxy adds a day credential, which expires at UTC midnight and can't be linked to your account through the credential itself, and sends the request to the model API. `pai` runs on macOS, Windows and Linux.

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

Both download the binary for your OS and CPU, check the signature on `SHA256SUMS` with the pai release key (below), check the binary against `SHA256SUMS`, and install to `~/.local/bin` or `%LOCALAPPDATA%\pai`. `PAI_DOWNLOAD_BASE` changes where they download from, and `PAI_INSTALL_DIR` where they install.

The installers check the signature with OpenSSL 3, or with `ssh-keygen` from OpenSSH 8.1 or later. macOS ships OpenSSH. Ubuntu 22.04, Debian 12, Fedora 36 and RHEL 9 and later ship OpenSSL 3. On Windows, `install.ps1` uses the Windows OpenSSH client (**Settings > System > Optional features**), or an OpenSSL 3 such as the one Git for Windows carries. With neither, the installers warn and install with only the checksum, which comes from the same server as the binary. A signature that doesn't match always stops them.

### Release key

Every release's `SHA256SUMS` is signed with this Ed25519 key, which is kept offline. A changed binary or `SHA256SUMS` fails the installers' check. The installers come from the same server as the binaries, so whoever controls that server could change the key in them too: compare the key in `install.sh` or `install.ps1` with this one, or download the installer from this repo. The release page shows the same key. `release/SIGNING.md` covers checking a download by hand.

<!-- release-key -->
```text
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAAaxr74KfLKGdOR5jPlOo1ngZVAIOOafRkstl+2po5Cs=
-----END PUBLIC KEY-----
```

The same key as an OpenSSH allowed-signers line:

```text
pai-release ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAGsa++CnyyhnTkeYz5TqNZ4GVQCDjmn0ZLLZftqaOQr
```

Key ID `f0a52c411efb5295`: its signatures are `SHA256SUMS.f0a52c411efb5295.sig` and `SHA256SUMS.f0a52c411efb5295.sshsig`.
<!-- /release-key -->

Or paste `skill/dawonai/SKILL.md` into your coding agent, and it does all of this for you.

## Commands

| Command | Does |
|---|---|
| `pai pair <code> [--setup HARNESS] [--relay URL] [--api URL] [--app URL] [--name NAME]` | Links this machine using the 8-character code from **Account > Set up a coding tool** in the DawonAI web app. Makes an Ed25519 device key. With `--setup`, it then sets up that harness (as `pai setup <harness> --apply`), collects today's credential, starts the proxy and prints `pai status`. The installers pass `pair …` on to it: `sh install.sh pair <code> --setup <harness>` is the one command the web app shows. |
| `pai token` | What coding tools run. Makes sure there's a credential for today, starts the local proxy if it isn't running (or is from an older `pai`), and prints the local key and nothing else. |
| `pai proxy` | Runs the local proxy in the foreground. It only answers on 127.0.0.1, only with the local key, and refuses requests from web pages. Once a day, at a random time, it collects tomorrow's credential. It sends your plan's number of model requests at a time (1 on Chat, 3 on Agent and the 1-day trial, as the model API names it in each response) and holds the rest, in order, until one finishes, so a tool that sends requests in parallel waits instead of getting "too many requests". A request held for 10 minutes rejoins the line after a random pause of up to 30 seconds. A request that fails before any of the answer arrives (no connection, a 429, or a 500, 502, 503 or 504) is sent again up to 5 times, after 1 s doubling to 30 s plus a random part. |
| `pai status` | Starts with `Configured properly: yes` (paired, plan paid, pins present, a harness set up) or `no` with what to fix. Then account, plan, device, credentials for today and tomorrow, pins, the proxy and the harnesses set up. Never prints secrets. |
| `pai unpair` | Stops the proxy and removes the login service, removes the device from your account, then deletes its keys, credentials and config here, even if the relay can't be reached. |
| `pai setup <claude-code\|codex\|opencode\|omp\|hermes\|other> [--apply\|--undo]` | Prints the tool's config and where the DawonAI skill goes. `other` prints the settings for any other harness (base URLs, model, context window, max output, key command) and writes nothing. `--apply` writes both and backs up any file it changes to `<file>.pai-backup-<time>`. `--undo` puts each changed file back from its first backup (or removes it, if pai made it) and deletes pai's own files and the skill. |
| `pai update` | Installs the latest release if it's newer. Release builds also do this on their own, once a day, in the background. See [Updates](#updates). |
| `pai service <install\|uninstall\|status>` | Runs the proxy whenever you're logged in, for tools that can't run `pai token` themselves. A LaunchAgent on macOS, a systemd user service on Linux. Not on Windows yet. |

The relay and API default to `https://relay.privacyai.example` and `https://api.privacyai.example`. Override them at pairing with the flags or `PAI_RELAY_URL` / `PAI_API_URL`. The DawonAI web app's address is `app.<domain>` for a relay at `relay.<domain>`, and `https://app.privacyai.example` for any other relay; `--app` or `PAI_APP_URL` sets it.

| Variable | Does |
|---|---|
| `PAI_PROXY_PORT` | The proxy's port, `47821` by default. Run `pai setup` again after changing it. |
| `PAI_PREFETCH=0` | Stops the proxy collecting tomorrow's credential ahead. |
| `PAI_OPEN_BROWSER=0` | Stops `pai` opening the DawonAI web app when this computer's device link is removed. |
| `PAI_AUTO_UPDATE=0` | Stops the daily background update. `pai update` still works. |

Each release carries the pins for the model API's key, plus a spare for when the box's key changes. Building or testing `pai` against a staging service, with a staging build, is covered in `docs/TESTING.md`.

Errors go to stderr. `pai token` exits with 2 when not paired, 3 when the plan isn't paid, 4 when the device was removed, and 5 when the relay is unreachable.

**When this computer's device link is removed.** DawonAI removes a `pai` device after 30 days without fetching a credential, and you can remove one in Account in the web app. The next time `pai token`, the proxy or `pai status` asks the relay, `pai` says so on stderr and opens the DawonAI web app in your default browser, at most once a UTC day. Pair again from **Account > Set up a coding tool**, after `pai unpair`.

## What's stored where

| What | Where |
|---|---|
| Device key, today's and tomorrow's credentials, the local proxy key | `pai/secrets.json` next to the config, created with mode 0600 on macOS and Linux. On Windows the user profile's own permissions protect it |
| Relay URL, API URL, web app URL, device ID, account ID | `pai/config.json` in the OS config directory (`~/Library/Application Support`, `%APPDATA%`, `~/.config`) |
| Lock that stops parallel `pai token` calls fetching twice | `pai/token.lock` next to the config |
| The UTC day `pai` last opened the web app for a removed device | `pai/web-app-opened` next to the config |
| When the next background update check is due | `pai/update.json` next to the config |

`PAI_CONFIG_DIR` moves all of these files to one directory.

**Why tomorrow's credential is collected ahead.** Collecting a credential is signed with this device's key, so the relay knows which account asked. If `pai` collected it just before your tool used it, someone watching the relay could match the two by time and tie that day's requests to your account. Collecting it at a random time the day before breaks that link. Set `PAI_PREFETCH=0` to turn this off.

The secrets are a file, not keychain items, on every platform. pai updates itself, and the macOS keychain ties each item to the code signature of the program that saved it, which changed with every release, so each update made the keychain ask permission again and failed over SSH, where nothing can show the prompt. A customer should never notice that pai collects a new credential every day. The file is readable by programs running as you, like any other tool's token file; what it holds can only collect day credentials for this one device, and **Devices** in the DawonAI web app revokes it. Whenever pai needs the device key and the file doesn't have it, it looks where pai before 0.1.6 kept it: `~/.config/pai/secrets.json` on macOS and Windows, then the OS store (the macOS keychain, Windows Credential Manager or the Secret Service on Linux). It moves every secret it finds into the file and deletes them there. On a Mac, reading the keychain can ask for permission one last time. If the old store can't be read, pai says why and tries again on the next command; `pai unpair` always works, and pairing again starts fresh.

## Updates

The model API names the latest release in a `pai-latest` header on every response. The first time a running proxy sees a version newer than its own, it starts `pai update` in the background, so an update begins on the first request after a release and the proxy is on the new version within a minute. A proxy that sends no requests still checks once a day, at a random time, as does the first `pai token` of the day. Checks and downloads go to `https://api.<domain>/pai/latest/` over the same pinned connection as your requests, through the relay like any request, and carry no credential. They name pai's version and platform in the user agent (`pai/0.1.8 (aarch64-apple-darwin)`); the model server counts them per UTC day, by version and platform, for the operator's dashboard, and keeps no time within the day. Requests to the model API carry your coding tool's own user agent, not pai's.

`pai` installs a release only if `SHA256SUMS` is signed by the release key built into it and the file matches its hash there, and never one older than itself. It writes the new binary next to the old one and renames it into place. A running proxy moves to the new binary once no request is in flight. On macOS and Linux it hands its open port to the new binary, so a request that arrives during the move waits a moment instead of being refused; on Windows the new proxy opens the port again. The proxy closes each connection after its response, so a coding tool never sends on a connection the old proxy is closing. An update from a pai that kept its secrets in the OS keychain (0.1.5 and older) keeps the local key coding tools already hold. The first `pai token` after an update or a reinstall rewrites the settings of the coding tools you set up with `--apply`, when the new release changes them (the context window, for example), backing up each file it changes.

A build from source never updates itself. `pai` 0.1.3 and earlier can't update: install again once with the command from the DawonAI web app.

## Coding tools

Run `pai setup <tool>` to see the exact config, with the path to `pai`, the proxy's address and your plan's context window filled in. `--apply` writes it. Every tool talks to the proxy, never to the model API directly.

`--apply` also installs the DawonAI skill, `skill/dawonai/SKILL.md`, as a personal skill named `dawonai`. It tells the tool's agent how `pai` works and how to fix problems with it. Running `pai setup` again replaces it, after a backup. `pai unpair` leaves it in place.

| Tool | Skill file |
|---|---|
| Claude Code | `~/.claude/skills/dawonai/SKILL.md`, or under `$CLAUDE_CONFIG_DIR` |
| Codex | `~/.agents/skills/dawonai/SKILL.md`. `CODEX_HOME` doesn't move it. OpenCode and OMP read this folder too. |
| OpenCode | `~/.config/opencode/skills/dawonai/SKILL.md`, or under `$XDG_CONFIG_HOME/opencode` |
| OMP | `~/.omp/agent/skills/dawonai/SKILL.md`, or under `$PI_CODING_AGENT_DIR` |
| Hermes Agent | `~/.hermes/skills/dawonai/SKILL.md`, or under `$HERMES_HOME` |

- **Claude Code**: `~/.claude/settings.json` gets `"apiKeyHelper": "<pai> token"`, `env.ANTHROPIC_BASE_URL`, the model `qwen3.8-27b` for every model alias, the context and output limits, and switches that keep Claude Code from sending traffic to Anthropic. Claude Code reruns the helper every hour and after a 401. An `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_BASE_URL` in the environment Claude Code starts in overrides these settings; `pai setup` warns about them.
- **Codex**: `~/.codex/config.toml` gets a `[model_providers.dawonai]` table using the Responses API, whose `[model_providers.dawonai.auth]` runs `pai token`. The model goes in a profile file, `~/.codex/dawonai.config.toml`, so start Codex with `codex --profile dawonai`.
- **OpenCode**: the plugin in `integrations/opencode/` goes to `~/.config/opencode/plugins/dawonai.ts`, and `opencode.json` gets `"model": "dawonai/qwen3.8-27b"`. The plugin registers the provider and runs `pai token`, again after a 401.
- **OMP**: the extension in `integrations/omp/` goes to `~/.omp/agent/extensions/dawonai.ts` (or under `$PI_CODING_AGENT_DIR`). It registers the provider with `pai token` as its key command. Pick `dawonai/qwen3.8-27b` with `/model`.
- **Hermes Agent**: `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`) gets a `dawonai` entry under `providers:`, whose `key_cmd` runs `pai token`, between a `# dawonai` and an `# end dawonai` comment. `pai` edits only those lines and leaves the rest of the file as it was. `key_cmd` needs a Hermes release from August 2026 or later. Your default model stays as it is: pick DawonAI with `/model custom:dawonai:qwen3.8-27b` (add `--global` to keep it), or with `hermes model`.

## Build

```sh
cargo build --release
cargo test
cargo test --features test-mode
```

`pai-proto` comes from `../pai-relay/crates/pai-proto`, so check out `pai-relay` next to this repo.

`release/build.sh` makes a release on a Mac with Docker running. It builds macOS natively, Linux as static musl binaries in `rust:1.95-alpine`, and Windows with `cargo-xwin` (static CRT, so no Visual C++ runtime needed). It writes to `dist/`, which is what gets hosted at `https://<domain>/pai/`:

| Path | Holds |
|---|---|
| `dist/<version>/` | `pai-{aarch64,x86_64}-apple-darwin`, `pai-{aarch64,x86_64}-unknown-linux-musl`, `pai-{x86_64,aarch64}-pc-windows-msvc.exe`, `dawonai-skill.zip`, `opencode-dawonai.ts`, `omp-dawonai.ts`, `SHA256SUMS`, and its signatures `SHA256SUMS.<key id>.sig` and `SHA256SUMS.<key id>.sshsig` (and, in the hand-over release 0.1.10, the old key's `SHA256SUMS.sig` and `.sshsig`) |
| `dist/latest/` | A copy of the newest version, which the installers download from |
| `dist/index.html`, `install.sh`, `install.ps1` | The release page and the installers |

`release/index.sh <version>` redoes only the page. `release/build.sh` needs `PAI_SIGNING_KEY`, the path to the release key, and signs `SHA256SUMS` with it; `release/SIGNING.md` covers the key, and what Apple and Windows code signing would take.

`cargo test` runs a fake relay with real issuer keys. To run against a real relay started with `PAI_DEV_PAYMENTS=1`:

```sh
PAI_TEST_RELAY=http://127.0.0.1:8787 cargo test --test real_relay -- --ignored
```

pai is part of DawonAI, a service of Dawon Science and Technology Laboratories LLC.
