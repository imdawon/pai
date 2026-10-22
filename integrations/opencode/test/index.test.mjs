import { test } from "node:test"
import assert from "node:assert/strict"
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createCredentialSource, createPaiFetch, defaultBaseURL } from "../dist/index.js"

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

test("adds the bearer credential and retries once with a fresh one on 401", async (t) => {
  withPaiBin(
    t,
    fakePai(t, 'n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$DIR/n"; echo "pai1.tok$n"'),
  )
  const seen = []
  const baseFetch = async (_url, init) => {
    const auth = new Headers(init.headers).get("authorization")
    seen.push(auth)
    if (auth === "Bearer pai1.tok1") {
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
  assert.deepEqual(seen, ["Bearer pai1.tok1", "Bearer pai1.tok2"])

  await paiFetch("https://api.example/v1/chat/completions", { method: "POST", body: "{}" })
  assert.equal(seen.at(-1), "Bearer pai1.tok2", "cached credential is reused")
})

test("surfaces pai's stderr when it fails", async (t) => {
  withPaiBin(t, fakePai(t, 'echo "not paired" >&2; exit 1'))
  await assert.rejects(createCredentialSource().get(), /not paired/)
})

test("base URL honours PAI_API_URL", () => {
  assert.equal(defaultBaseURL({}), "https://api.privacyai.example/v1")
  assert.equal(defaultBaseURL({ PAI_API_URL: "http://localhost:8080/" }), "http://localhost:8080/v1")
})
