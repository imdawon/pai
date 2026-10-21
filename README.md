# pai

A small command-line program that lets coding tools use Privacy AI without a long-lived API key. It runs on Mac, Windows and Linux.

- `pai pair <code>`: links this machine using a 10-minute pairing code from the web app. It makes a device key and stores it in the OS keychain.
- `pai token`: prints a valid day credential. If the cached one has expired, it collects a new blind-signed one from the relay.
- `pai unpair`: removes the device key and the cached credential.

The account key never reaches this machine. A stolen device key can only fetch day credentials, and the user can revoke it from the web app.

`skill/` holds the skill users paste into their agent. It installs `pai`, pairs the machine, and points the tool at `pai token`:

| Tool | Setting |
| --- | --- |
| Claude Code | `apiKeyHelper` |
| Codex | the `[model_providers.<id>.auth]` command |
| OpenCode | auth plugin |
| OMP | `registerProvider` extension |

See `docs/V1.md` in the privacy-ai repo for the full design.
