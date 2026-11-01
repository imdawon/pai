# opencode-dawonai

An [OpenCode](https://opencode.ai) plugin for DawonAI. It adds a `dawonai` provider with the `qwen3.8-27b` model and authenticates each request with a day credential from `pai token`, so you never put an API key in your config.

## Requirements

- The `pai` CLI, paired with this device (`pai token` must print a credential).
- OpenCode 1.18 or later, or OpenCode 2.0.4 or later.

## Enable it

The easy way: `pai setup opencode --apply`. It writes `src/index.ts` to `~/.config/opencode/plugins/dawonai.ts`, with the path to `pai`, the API URL and your plan's context window filled in. When it loads, the plugin asks pai's proxy for the current context window and uses the written number only if the proxy doesn't answer, and sets `"model": "dawonai/qwen3.8-27b"` in `~/.config/opencode/opencode.json`.

Or pick one of these.

**From npm** (once published), in `opencode.json` (project) or `~/.config/opencode/opencode.json` (global):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-dawonai"],
  "model": "dawonai/qwen3.8-27b"
}
```

**From a checkout**, point `plugin` at this directory after running `npm install && npm run build` in it. A relative path resolves against the config file that names it.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["/path/to/pai/integrations/opencode"],
  "model": "dawonai/qwen3.8-27b"
}
```

**As a single file**, copy the built `dist/index.js` to `~/.config/opencode/plugins/dawonai.js` (or `.opencode/plugins/` in a project). OpenCode loads files in those directories on startup, so only the `model` line is needed:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "dawonai/qwen3.8-27b"
}
```

You can also leave `model` out and pick **DawonAI › Qwen 3.8 27B** with `/models`.

## Configuration

| Setting | Default | Purpose |
| --- | --- | --- |
| `PAI_BIN` | `pai` on `PATH` | Path to the `pai` CLI. |
| `PAI_API_URL` | `https://api.privacyai.example` | API root. The plugin appends `/v1`. |
| `provider.dawonai.options.baseURL` in `opencode.json` | from `PAI_API_URL` | Full OpenAI-compatible base URL. Takes precedence over `PAI_API_URL`. |

Anything else you set under `provider.dawonai` in `opencode.json` (name, extra models, model limits) is kept; the plugin only fills in what is missing.

## How it works

- A `config` hook registers the `dawonai` provider using `@ai-sdk/openai-compatible`, so no `provider` block is needed in your config.
- The provider gets a custom `fetch` that runs `pai token` (no shell), keeps the credential in memory until the next UTC midnight, and sends it as `Authorization: Bearer …`. On a `401` it runs `pai token` again and retries the request once.
- A `chat.headers` hook sets the same header, for OpenCode's experimental native LLM runtime, which does not use the provider `fetch`.
- If `pai token` fails, its error message (for example `not paired`) is what OpenCode shows. The credential itself is never logged.

## Develop

```sh
npm install
npm run typecheck
npm test
```

## References

- Plugins: https://opencode.ai/docs/plugins/
- Providers and custom OpenAI-compatible providers: https://opencode.ai/docs/providers/
- Config: https://opencode.ai/docs/config/
- Plugin types: [`@opencode-ai/plugin`](https://www.npmjs.com/package/@opencode-ai/plugin) 1.19.0
