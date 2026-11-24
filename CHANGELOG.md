# Changelog

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
