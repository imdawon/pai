import { execFile } from "node:child_process";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

// `pai setup omp --apply` rewrites these three lines for the user's install and plan, and pai
// rewrites CONTEXT_WINDOW again when the model API names a new limit.
const DEFAULT_BASE_URL = "http://127.0.0.1:47821";
const DEFAULT_PAI_BIN = "pai";
const CONTEXT_WINDOW = 131072;
// The range pai accepts from the model API's `pai-context` header.
const MIN_CONTEXT = 8192;
const MAX_CONTEXT = 1048576;
// How long loading waits for pai before using CONTEXT_WINDOW.
const CONTEXT_WAIT_MS = 5000;

const PROVIDER_ID = "dawonai";

// pai's local proxy. It adds the day credential and checks the model API's certificate against
// pai's pins, so the key OMP holds only works on this machine.
function baseUrl(): string {
  return (process.env.PAI_BASE_URL?.trim() || DEFAULT_BASE_URL).replace(/\/+$/, "");
}

function paiBin(): string {
  return process.env.PAI_BIN?.trim() || DEFAULT_PAI_BIN;
}

// OMP runs `!command` keys through a shell. Double quotes are the one form a POSIX shell and
// cmd.exe both accept, and they keep the backslashes of a Windows path, which an unquoted POSIX
// command line would strip.
function shellQuote(s: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(s) ? s : `"${s.replace(/["$`]/g, "\\$&")}"`;
}

function paiToken(): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(paiBin(), ["token"], { timeout: CONTEXT_WAIT_MS, windowsHide: true }, (error, stdout) => {
      const key = String(stdout).trim();
      if (error || !key.startsWith("pai-local-") || /\s/.test(key)) reject(new Error("no local key"));
      else resolve(key);
    });
  });
}

// Asks pai's local proxy for the plan's context limit, which pai keeps in step with the model API's
// `pai-context` header. Uses CONTEXT_WINDOW when pai or its proxy doesn't answer in time, or answers
// with a number outside 8K to 1M.
async function contextWindow(): Promise<number> {
  const ask = async (): Promise<number> => {
    const key = await paiToken();
    const response = await fetch(`${baseUrl()}/pai/context`, {
      headers: { authorization: `Bearer ${key}` },
      signal: AbortSignal.timeout(CONTEXT_WAIT_MS),
    });
    if (!response.ok) return CONTEXT_WINDOW;
    const n = ((await response.json()) as { context?: unknown })?.context;
    return typeof n === "number" && Number.isInteger(n) && n >= MIN_CONTEXT && n <= MAX_CONTEXT ? n : CONTEXT_WINDOW;
  };
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<number>((resolve) => {
    timer = setTimeout(() => resolve(CONTEXT_WINDOW), CONTEXT_WAIT_MS);
  });
  try {
    return await Promise.race([ask().catch(() => CONTEXT_WINDOW), late]);
  } finally {
    clearTimeout(timer);
  }
}

export default async function dawonAi(pi: ExtensionAPI): Promise<void> {
  const context = await contextWindow();
  pi.registerProvider(PROVIDER_ID, {
    baseUrl: `${baseUrl()}/v1`,
    api: "openai-completions",
    // `pai token` starts pai's local proxy if needed and prints this machine's key for it. A
    // `!command` key keeps it in OMP's memory only (an oauth login would persist it in agent.db)
    // and needs no /login. OMP caches the output for the process and re-runs it after a 401.
    apiKey: `!${shellQuote(paiBin())} token`,
    models: [
      {
        id: "qwen3.8-27b",
        name: "Qwen 3.8 27B (DawonAI)",
        reasoning: true,
        input: ["text"],
        // Billed by plan, not per token.
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: context,
        maxTokens: Math.min(32768, Math.floor(context / 4)),
      },
    ],
  });
}
