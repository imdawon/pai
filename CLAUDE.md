# pai: rules for changes

- **Every pai update answers two questions, in the PR:** is it backward compatible (the new pai
  works against today's relay and gateway, and with what older versions left on disk), and is it
  forward compatible (every pai already installed keeps working once the new relay and gateway
  ship)? Installed copies update only after they see `pai-latest`, so they will meet the new
  servers first. See `../privacy-ai/CLAUDE.md`, "Every pai update answers two questions".
- Releases are signed with both release keys until every install is on 0.1.10 or later; keep
  `release/handover-key.pub`.
- Read `../privacy-ai/CLAUDE.md` before writing user-facing copy, and check design changes against
  `../privacy-ai/docs/THREAT-MODEL.md`.
