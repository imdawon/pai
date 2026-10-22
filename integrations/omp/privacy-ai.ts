import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

// `pai setup omp --apply` rewrites these three lines for the user's install and plan.
const DEFAULT_BASE_URL = "http://127.0.0.1:47821";
const DEFAULT_PAI_BIN = "pai";
const CONTEXT_WINDOW = 262144;

const PROVIDER_ID = "privacy-ai";

// pai's local proxy. It adds the day credential and checks the model API's certificate against
// pai's pins, so the key OMP holds only works on this machine.
function baseUrl(): string {
  return (process.env.PAI_BASE_URL?.trim() || DEFAULT_BASE_URL).replace(/\/+$/, "");
}

function paiBin(): string {
  return process.env.PAI_BIN?.trim() || DEFAULT_PAI_BIN;
}

// OMP runs `!command` keys through a POSIX-style shell (Brush on Windows), which both accept single quotes.
function shellQuote(s: string): string {
  return /^[A-Za-z0-9_@%+=:,./\\-]+$/.test(s) ? s : `'${s.replaceAll("'", `'\\''`)}'`;
}

export default function privacyAi(pi: ExtensionAPI): void {
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
        name: "Qwen 3.8 27B (Privacy AI)",
        reasoning: true,
        input: ["text"],
        // Billed by plan, not per token.
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: CONTEXT_WINDOW,
        maxTokens: Math.min(32768, CONTEXT_WINDOW / 4),
      },
    ],
  });
}
