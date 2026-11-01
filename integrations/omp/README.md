# DawonAI for OMP

An [oh-my-pi](https://github.com/can1357/oh-my-pi) (OMP) extension that adds the `dawonai` provider with the model `qwen3.8-27b`. OMP gets its key by running `pai token`, so no API key is stored in OMP's config.

## Install

The easy way: `pai setup omp --apply`. It writes `dawonai.ts` to `~/.omp/agent/extensions/`, with the path to `pai`, the API URL and your plan's context window filled in. When it loads, the plugin asks pai's proxy for the current context window and uses the written number only if the proxy doesn't answer.

By hand:

1. Pair this machine: `pai pair <code>`. Check that `pai token` prints a `pai1.` credential.
2. Copy `dawonai.ts` into `~/.omp/agent/extensions/`. A named profile uses `~/.omp/profiles/<name>/agent/extensions/`, and a single project uses `<project>/.omp/extensions/`. To try it without installing: `omp -e /path/to/integrations/omp`.
3. Restart OMP and pick the model with `/model` (`dawonai/qwen3.8-27b`), or start it with `omp --model dawonai/qwen3.8-27b`.

No `/login` is needed.

## Settings

| Variable | Default | Use |
| --- | --- | --- |
| `PAI_BIN` | `pai` on `PATH` (or the path `pai setup` wrote) | Path to the `pai` binary |
| `PAI_API_URL` | `https://api.privacyai.example` | API root; OMP calls `<PAI_API_URL>/v1/chat/completions` |

## How the credential stays fresh

The extension registers the provider with a command-backed key, `apiKey: "!pai token"`. OMP runs the command when it first needs the key, keeps the output in memory for the life of the process, and runs it again after a `401`. A day credential expires at UTC midnight, so the first request after midnight gets a `401`, OMP runs `pai token` again, and retries.

This keeps the credential out of OMP's credential store (`~/.omp/agent/agent.db`), where an `oauth` login would persist it, and it needs no `/login` step.

If `pai token` fails, OMP doesn't show its message and retries after 30 seconds. Run `pai token` yourself to see why (`not paired`, `plan not paid`, `device removed, pair again`, `relay unreachable`).

## OMP and pi

OMP is a fork of pi, and this follows OMP:

- Extensions live in `~/.omp/agent/extensions`; pi uses `~/.pi/agent/extensions`, which OMP doesn't read.
- OMP re-runs a `!command` key after a `401`. pi caches the result until it exits, so in pi a session would stop working at UTC midnight.
- OMP's `registerProvider` has no top-level `name`; the provider shows as `dawonai`.

## Typecheck

```sh
npm install
npm run typecheck
```

## Sources

Checked against oh-my-pi at commit `3a2758b` (`@oh-my-pi/pi-coding-agent` 18.12.6; the typecheck uses 18.11.0 from npm), and run in OMP 18.12.5 against a fake API:

- [docs/extensions.md](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/docs/extensions.md) (provider registration)
- [docs/extension-loading.md](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/docs/extension-loading.md) (extension folders)
- [docs/models.md](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/docs/models.md) (command-resolved secrets)
- [extensibility/extensions/types.ts](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/packages/coding-agent/src/extensibility/extensions/types.ts) (`ProviderConfig`)
- [config/resolve-config-value.ts](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/packages/coding-agent/src/config/resolve-config-value.ts) and [config/api-key-resolver.ts](https://github.com/can1357/oh-my-pi/blob/3a2758b68f7c8e6bdf22c3a76b86c8675e5e0c9e/packages/coding-agent/src/config/api-key-resolver.ts) (`!command` keys, refresh after a 401)
- pi, for comparison: [custom-provider.md](https://github.com/badlogic/pi-mono/blob/58c0b02ce552adab76014e7ed712ab7981359aa8/packages/coding-agent/docs/custom-provider.md), [resolve-config-value.ts](https://github.com/badlogic/pi-mono/blob/58c0b02ce552adab76014e7ed712ab7981359aa8/packages/coding-agent/src/core/resolve-config-value.ts)
