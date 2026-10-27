import { test } from "node:test"
import assert from "node:assert/strict"
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import plugin, { DawonAISetup, createCredentialSource, createPaiFetch, defaultBaseURL } from "../dist/index.js"

function fakePai(t, body) {
  const dir = mkdtempSync(join(tmpdir(), "fake-pai-"))
  t.after(() => rmSync(dir, { recursive: true, force: true }))
  const bin = join(dir, "pai")
  writeFileSync(bin, `#!/bin/sh\n${body.replaceAll("$DIR", dir)}\n`)
  chmodSync(bin, 0o755)
  return bin
}

function withPaiBin(t, bin) {
  const previous = process.env.PAI_BIN
  process.env.PAI_BIN = bin
  t.after(() => {
    if (previous === undefined) delete process.env.PAI_BIN
    else process.env.PAI_BIN = previous
  })
}

test("adds the local key and asks pai again once on 401", async (t) => {
  withPaiBin(
    t,
    fakePai(t, 'n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$DIR/n"; echo "pai-local-tok$n"'),
  )
  const seen = []
  const baseFetch = async (_url, init) => {
    const auth = new Headers(init.headers).get("authorization")
    seen.push(auth)
    if (auth === "Bearer pai-local-tok1") {
      return new Response(JSON.stringify({ error: { type: "invalid_credential" } }), { status: 401 })
    }
    return new Response("{}", { status: 200 })
  }
  const paiFetch = createPaiFetch(createCredentialSource(), baseFetch)

  const res = await paiFetch("https://api.example/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: "{}",
  })

  assert.equal(res.status, 200)
  assert.deepEqual(seen, ["Bearer pai-local-tok1", "Bearer pai-local-tok2"])

  await paiFetch("https://api.example/v1/chat/completions", { method: "POST", body: "{}" })
  assert.equal(seen.at(-1), "Bearer pai-local-tok2", "the key is reused")
})

test("surfaces pai's stderr when it fails", async (t) => {
  withPaiBin(t, fakePai(t, 'echo "not paired" >&2; exit 1'))
  await assert.rejects(createCredentialSource().get(), /not paired/)
})

test("asks pai again once when the proxy can't be reached", async (t) => {
  withPaiBin(
    t,
    fakePai(t, 'n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$DIR/n"; echo "pai-local-k$n"'),
  )
  let calls = 0
  const baseFetch = async () => {
    calls++
    if (calls === 1) throw new TypeError("fetch failed")
    return new Response("{}", { status: 200 })
  }
  const res = await createPaiFetch(createCredentialSource(), baseFetch)("http://127.0.0.1:47821/v1/models", {})
  assert.equal(res.status, 200)
  assert.equal(calls, 2)
})

test("refuses output that isn't a local key", async (t) => {
  withPaiBin(t, fakePai(t, 'echo "pai1.a-day-credential"'))
  await assert.rejects(createCredentialSource().get(), /isn't pai's local key/)
})

test("base URL is pai's local proxy, or PAI_BASE_URL", () => {
  assert.equal(defaultBaseURL({}), "http://127.0.0.1:47821/v1")
  assert.equal(defaultBaseURL({ PAI_BASE_URL: "http://127.0.0.1:9999/" }), "http://127.0.0.1:9999/v1")
})

test("the default export loads in OpenCode 1 (server) and OpenCode 2 (setup)", () => {
  assert.equal(plugin.id, "dawonai")
  assert.equal(typeof plugin.server, "function")
  assert.equal(typeof plugin.setup, "function")
})

// A stand-in for OpenCode 2's plugin context, recording what setup registers. The provider editor follows
// packages/core/src/provider.ts at v2.1.4: update() and models.update() create what's missing.
function fakeV2(records = new Map()) {
  const transforms = []
  const hooks = []
  const ctx = {
    options: {},
    provider: {
      transform: async (cb) => {
        transforms.push(cb)
        return { dispose: async () => {} }
      },
    },
    session: {
      hook: async (name, cb, options) => {
        hooks.push({ name, cb, options })
        return { dispose: async () => {} }
      },
    },
  }
  const entry = (id) => {
    if (!records.has(id)) records.set(id, { provider: { id, name: id, activation: "auto", package: "" }, models: new Map() })
    return records.get(id)
  }
  const editor = {
    list: () => [...records.values()],
    get: (id) => records.get(id),
    update: (id, update) => update(entry(id).provider),
    remove: (id) => records.delete(id),
    models: {
      update: (providerID, modelID, update) => {
        const models = entry(providerID).models
        const model = models.get(modelID) ?? {
          id: modelID,
          modelID,
          providerID,
          name: modelID,
          capabilities: { tools: true, input: ["text", "image"], output: ["text"] },
          limit: { context: 200_000, output: 32_000 },
        }
        update(model)
        models.set(modelID, model)
      },
    },
  }
  const hook = (name) => hooks.find((h) => h.name === name)
  return { ctx, transforms, hooks, records, editor, hook }
}

test("OpenCode 2: setup adds the dawonai provider and model", async (t) => {
  withPaiBin(t, fakePai(t, 'echo "pai-local-k1"'))
  const v2 = fakeV2()
  await DawonAISetup(v2.ctx)

  assert.equal(v2.transforms.length, 1)
  v2.transforms[0](v2.editor)
  const record = v2.records.get("dawonai")
  assert.deepEqual(record.provider, {
    id: "dawonai",
    name: "DawonAI",
    activation: "enabled",
    package: "@opencode/ai/providers/openai-compatible",
    settings: { baseURL: defaultBaseURL(), provider: "dawonai" },
  })
  const model = record.models.get("qwen3.8-27b")
  assert.equal(model.name, "Qwen 3.8 27B")
  assert.deepEqual(model.capabilities, { tools: true, input: ["text"], output: ["text"] })
  assert.deepEqual(model.limit, { context: 262144, output: 32768 })
})

test("OpenCode 2: setup keeps a baseURL and name the user configured", async (t) => {
  withPaiBin(t, fakePai(t, 'echo "pai-local-k1"'))
  const records = new Map([
    [
      "dawonai",
      {
        provider: { id: "dawonai", name: "Mine", activation: "auto", package: "", settings: { baseURL: "http://127.0.0.1:9/v1" } },
        models: new Map(),
      },
    ],
  ])
  const v2 = fakeV2(records)
  await DawonAISetup(v2.ctx)
  v2.transforms[0](v2.editor)
  assert.equal(records.get("dawonai").provider.name, "Mine")
  assert.equal(records.get("dawonai").provider.settings.baseURL, "http://127.0.0.1:9/v1")
})

test("OpenCode 2: each dawonai request carries the local key, and a 401 asks pai again once", async (t) => {
  withPaiBin(
    t,
    fakePai(t, 'n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$DIR/n"; echo "pai-local-k$n"'),
  )
  const v2 = fakeV2()
  await DawonAISetup(v2.ctx)

  const request = v2.hook("model.request")
  const retry = v2.hook("retry")
  assert.deepEqual(request.options, { providerID: "dawonai" })
  assert.deepEqual(retry.options, { providerID: "dawonai" })

  const first = { headers: { "x-opencode-session": "s" } }
  await request.cb(first)
  assert.deepEqual(first.headers, { "x-opencode-session": "s", authorization: "Bearer pai-local-k1" })

  const again = { headers: {} }
  await request.cb(again)
  assert.equal(again.headers.authorization, "Bearer pai-local-k1", "the key is reused")

  const unauthorized = { error: { type: "provider.auth", message: "", status: 401 }, attempt: 2, decision: { retry: false } }
  await retry.cb(unauthorized)
  assert.deepEqual(unauthorized.decision, { retry: true, delay: 0 })

  const retried = { headers: {} }
  await request.cb(retried)
  assert.equal(retried.headers.authorization, "Bearer pai-local-k2")

  const secondFailure = { error: { type: "provider.auth", message: "", status: 401 }, attempt: 3, decision: { retry: false } }
  await retry.cb(secondFailure)
  assert.deepEqual(secondFailure.decision, { retry: false }, "only one forced retry")
})

test("OpenCode 2: when the proxy can't be reached, the next attempt asks pai again", async (t) => {
  withPaiBin(
    t,
    fakePai(t, 'n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$DIR/n"; echo "pai-local-k$n"'),
  )
  const v2 = fakeV2()
  await DawonAISetup(v2.ctx)
  await v2.hook("model.request").cb({ headers: {} })

  const failed = { error: { type: "provider.transport", message: "Unable to connect" }, attempt: 2, decision: { retry: true, delay: 2000 } }
  await v2.hook("retry").cb(failed)
  assert.deepEqual(failed.decision, { retry: true, delay: 2000 }, "OpenCode's own retry timing is kept")

  const next = { headers: {} }
  await v2.hook("model.request").cb(next)
  assert.equal(next.headers.authorization, "Bearer pai-local-k2")

  const other = { error: { type: "provider.rate-limit", message: "", status: 429 }, attempt: 2, decision: { retry: true, delay: 5 } }
  await v2.hook("retry").cb(other)
  const after = { headers: {} }
  await v2.hook("model.request").cb(after)
  assert.equal(after.headers.authorization, "Bearer pai-local-k2", "other errors keep the key")
})

test("setup does nothing in OpenCode 1's experimental core, which has no provider or session domain", async () => {
  const calls = []
  const ctx = {
    options: {},
    catalog: { transform: async () => calls.push("catalog") },
    aisdk: { sdk: async () => calls.push("aisdk") },
  }
  await DawonAISetup(ctx)
  assert.deepEqual(calls, [])
})
