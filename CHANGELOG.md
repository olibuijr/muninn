# Changelog

## Unreleased

- `muninn hook prompt` drops recalled memories that score below 0.6 of the best hit
  (`MUNINN_HOOK_REL_SCORE`), so one strong match no longer brings four weak ones into the prompt.
- `muninn-indexd`: `/webhook/push` rejects a repository name that is a path and `before`/`after`
  values that are not object ids, so they can no longer reach git as paths or options.
- Memory recall honours `project_id` (`muninn recall --project P`, memoryd `/recall`, the MCP
  `recall` tool): only memories stored with that project are returned.
- `muninn-mcp` refuses to start without a tokens file or `MUNINN_MCP_TOKEN` instead of serving
  the index tools to anyone; `MUNINN_MCP_ANONYMOUS=1` keeps the old open behaviour on purpose.
- `muninn-mcp` compares `MUNINN_MCP_TOKEN` in constant time.
- `muninn-indexd` refuses to start without `MUNINN_WEBHOOK_SECRET`, and `/webhook/push` rejects
  every push when no secret is configured, instead of accepting unsigned pushes.

## [0.1.0] - 2026-11-24

First public release.

- `muninn-indexd`: syntax-aware chunking, hybrid BM25 + embedding search fused by reciprocal rank
  with cross-encoder reranking, optional LLM query expansion, SCIP symbols with callers and
  callees, push-driven reindex of bare mirrors, a live overlay for uncommitted edits.
- `muninn-memoryd`: scoped agent memory with Postgres row-level security, deduplication, conflict
  detection, a review queue, supersession, secret rejection, an append-only audit log and weekly
  lifecycle jobs (consolidation, expiry, re-embedding).
- `muninn-mcp`: MCP server with code, symbol, file and memory tools, filtered by token scope.
- `muninn-indexweb`: browser viewer.
- `muninn` CLI: search, symbol, repos, recall, remember, forget, review, doctor, eval, expand,
  `--json` output, the bundled agent skill (`muninn skill`) and Claude Code / Codex hooks.
- Static x86_64 Linux binaries, systemd user units, `install.sh` and `client.sh`.
