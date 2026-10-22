import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

// `pai setup omp --apply` rewrites these three lines for the user's install and plan.
const DEFAULT_API_URL = "https://api.privacyai.example";
const DEFAULT_PAI_BIN = "pai";
const CONTEXT_WINDOW = 262144;

const PROVIDER_ID = "privacy-ai";

function apiUrl(): string {
  return (process.env.PAI_API_URL?.trim() || DEFAULT_API_URL).replace(/\/+$/, "");
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
    baseUrl: `${apiUrl()}/v1`,
    api: "openai-completions",
    // A `!command` key keeps the day credential in OMP's memory only (an oauth login would
    // persist it in agent.db) and needs no /login. OMP caches the output for the process and
    // re-runs the command after a 401, which is how a session crosses UTC midnight.
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
