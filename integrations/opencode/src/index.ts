import { execFile } from "node:child_process"
import type { Plugin, PluginModule } from "@opencode-ai/plugin"

export const PROVIDER_ID = "privacy-ai"
export const MODEL_ID = "qwen3.8-27b"
// `pai setup opencode --apply` rewrites these three lines for the user's install and plan.
const DEFAULT_BASE_URL = "http://127.0.0.1:47821"
const DEFAULT_PAI_BIN = "pai"
const CONTEXT_WINDOW = 262144

type Fetch = typeof globalThis.fetch

// pai's local proxy. It adds the day credential and checks the model API's certificate against
// pai's pins, so the key this plugin holds only works on this machine.
export function defaultBaseURL(env: NodeJS.ProcessEnv = process.env): string {
  const base = (env.PAI_BASE_URL?.trim() || DEFAULT_BASE_URL).replace(/\/+$/, "")
  return base.endsWith("/v1") ? base : `${base}/v1`
}

function runPaiToken(bin: string): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(bin, ["token"], { timeout: 30_000, windowsHide: true }, (error, stdout, stderr) => {
      if (error) {
        const code = (error as NodeJS.ErrnoException).code
        if (code === "ENOENT") {
          reject(new Error(`Privacy AI: \`${bin}\` not found. Install the pai CLI or set PAI_BIN.`))
          return
        }
        const detail = String(stderr).trim() || error.message
        reject(new Error(`Privacy AI: \`pai token\` failed: ${detail}`))
        return
      }
      const key = String(stdout).trim()
      // Never echo stdout here: if it is a secret in an unexpected shape it must not reach logs.
      if (!key.startsWith("pai-local-") || /\s/.test(key)) {
        reject(new Error("Privacy AI: `pai token` printed something that isn't pai's local key. Update pai."))
        return
      }
      resolve(key)
    })
  })
}

export type CredentialSource = {
  get(): Promise<string>
  invalidate(stale: string): void
}

// `pai token` starts pai's local proxy if it isn't running and prints this machine's key for it.
// The key doesn't expire; it's fetched again after a 401, or when the proxy can't be reached.
export function createCredentialSource(options: { bin?: string } = {}): CredentialSource {
  let cached: { value: string } | undefined
  let pending: Promise<string> | undefined

  return {
    get() {
      if (cached) return Promise.resolve(cached.value)
      if (pending) return pending
      const bin = options.bin ?? (process.env.PAI_BIN?.trim() || DEFAULT_PAI_BIN)
      // Single-flight: a burst of requests (title generation, subagents) should spawn pai once.
      pending = runPaiToken(bin)
        .then((value) => {
          cached = { value }
          return value
        })
        .finally(() => {
          pending = undefined
        })
      return pending
    },
    invalidate(stale) {
      // Only drop the cache if no concurrent request has already refreshed it.
      if (cached?.value === stale) cached = undefined
    },
  }
}

export function createPaiFetch(credentials: CredentialSource, baseFetch: Fetch = globalThis.fetch): Fetch {
  return async (input, init) => {
    const send = async (credential: string) => {
      const headers = new Headers(input instanceof Request ? input.headers : undefined)
      new Headers(init?.headers).forEach((value, key) => headers.set(key, value))
      headers.set("authorization", `Bearer ${credential}`)
      return baseFetch(input, { ...init, headers })
    }

    const credential = await credentials.get()
    // A Request body is a one-shot stream; the AI SDK sends string bodies via init, which can be replayed.
    const replayable = !(input instanceof Request)
    let response: Response
    try {
      response = await send(credential)
    } catch (e) {
      // The proxy isn't running (the machine restarted, or pai was updated): `pai token` starts it.
      if (!replayable) throw e
      credentials.invalidate(credential)
      return send(await credentials.get())
    }
    if (response.status !== 401 || !replayable) return response

    await response.body?.cancel().catch(() => {})
    credentials.invalidate(credential)
    return send(await credentials.get())
  }
}

export const PrivacyAIPlugin: Plugin = async () => {
  const credentials = createCredentialSource()
  const paiFetch = createPaiFetch(credentials)

  return {
    config: async (cfg) => {
      cfg.provider ??= {}
      const existing = cfg.provider[PROVIDER_ID] ?? {}
      cfg.provider[PROVIDER_ID] = {
        ...existing,
        npm: existing.npm ?? "@ai-sdk/openai-compatible",
        name: existing.name ?? "Privacy AI",
        options: {
          ...existing.options,
          baseURL: existing.options?.baseURL ?? defaultBaseURL(),
          // OpenCode hands provider options straight to createOpenAICompatible, so a function survives here.
          fetch: paiFetch,
        },
        models: {
          [MODEL_ID]: {
            name: "Qwen 3.8 27B",
            tool_call: true,
            limit: { context: CONTEXT_WINDOW, output: Math.min(32768, CONTEXT_WINDOW / 4) },
          },
          ...existing.models,
        },
      }
    },
    // The custom fetch only runs on the AI SDK runtime; this keeps the experimental native runtime authenticated too.
    "chat.headers": async (input, output) => {
      if (input.model.providerID !== PROVIDER_ID) return
      output.headers.authorization = `Bearer ${await credentials.get()}`
    },
  }
}

const plugin: PluginModule = {
  id: PROVIDER_ID,
  server: PrivacyAIPlugin,
}

export default plugin
