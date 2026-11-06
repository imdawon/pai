# pai: rules for changes

- **Every pai update answers two questions, in the PR:** is it backward compatible (the new pai
  works against today's relay and gateway, and with what older versions left on disk), and is it
  forward compatible (every pai already installed keeps working once the new relay and gateway
  ship)? Installed copies update only after they see `pai-latest`, so they will meet the new
  servers first. See `../privacy-ai/CLAUDE.md`, "Every pai update answers two questions".
- Releases are signed with both release keys until every install is on 0.1.10 or later; keep
  `release/handover-key.pub`.
- **Stay compatible with every major harness and with the OpenAI and Anthropic APIs.** What pai's local proxy sends a harness, its own errors included, must look like what that API sends. Act on the gateway's `dawonai_error`, never on OpenAI's or Anthropic's labels. See `../privacy-ai/CLAUDE.md`, "Stay compatible with every major harness and with both APIs".
- Read `../privacy-ai/CLAUDE.md` before writing user-facing copy, and check design changes against
  `../privacy-ai/docs/THREAT-MODEL.md`.
- **Write down every tuned number in `../privacy-ai/docs/SETTINGS.md`.** A limit, threshold, timer,
  weight, cap, retry count or size someone chose gets a row there with its value, why, the evidence
  and who decided; a change to one updates its row in the same change, keeping the old value. The
  code comment points there. See `../privacy-ai/CLAUDE.md`, "Write down every tuned number".
- **Rerun the outside day pass tests** (RFC 9474 vector, Wycheproof, Cloudflare circl) on any change to
  how pai collects, holds or checks passes, or to pai-proto: run `PAI_REQUIRE_CIRCL=1 cargo test -p pai-proto`
  in `../pai-relay` (needs Go 1.25+) and paste the result into the PR. This repo has no CI for it. A
  failure blocks the merge. See `../privacy-ai/CLAUDE.md`, "Rerun the outside day pass tests".
- When a change alters what `../privacy-ai/docs/HOW-IT-WORKS.html` shows (sign-up, day passes, pai, a
  message's path, limits, what a machine sees or keeps), update that page in the same change and
  republish it. When a change alters what `../privacy-ai/docs/DAY-PASS-CRYPTO.html` shows (a key, a
  derivation, a signed format, issuing or checking day passes, when passes are collected, a privacy
  claim), update and republish that page too. See `../privacy-ai/CLAUDE.md`, "Keep the two explainer
  pages up to date".
