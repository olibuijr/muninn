---
name: muninn
description: Search code and keep shared memory with Muninn, the self-hosted code index and agent memory. Use when a repo is indexed by Muninn, before grepping a large codebase, to find a symbol, its callers or callees, to search code by concept, to recall past decisions, root causes and gotchas, or to remember a durable fact at the end of a task. Triggers - muninn, code index, code search, symbol lookup, callers, recall, remember, memory, past decision, why was this done.
license: MIT
compatibility: Needs a Muninn server (muninn-indexd, muninn-memoryd, muninn-mcp) and either the muninn MCP server or the muninn CLI with a token.
---

# Muninn

Muninn indexes git repos (hybrid BM25 + embeddings with a reranker, plus a SCIP symbol graph)
and keeps a memory that every agent session shares. Use it before grep in indexed repos, and
use the memory so the next session knows what this one learned.

## Tools

With the MCP server (tools `mcp__muninn__*`):

| Need | Tool |
|---|---|
| An identifier (function, type, constant) | `symbol_lookup` |
| Who calls it, what it calls | `explain_symbol` |
| Code by concept, or an exact string (`mode: lexical`) | `code_search` |
| Not sure where the answer lives | `search_all` (code, symbols and memory, fused) |
| A file at a branch | `read_file` |
| Which repos are indexed | `list_repos` |
| Past decisions, gotchas, how-tos | `recall` |
| Store a durable fact | `remember` |
| Retract one of your own facts | `forget` |

Without MCP, the CLI does the same (add `--json` for machine-readable output):

```sh
muninn search "where are webhook signatures checked" --mode hybrid -k 8
muninn symbol verify_signature
muninn recall "webhook secret rotation"
muninn remember "Webhook secrets rotate with deploy/rotate.sh; indexd needs a restart." \
  --type procedural --scope global --importance 0.6 --source session:2026-11-24-webhooks
muninn doctor            # daemons reachable, tokens valid
```

## Working rules

1. **Recall first.** For any non-trivial task, `recall` with the task plus two or three concrete
   nouns (repo, service, error string). Treat hits as leads: check them against the code.
2. **Index before grep** in indexed repos: `symbol_lookup` for names, `code_search` for
   concepts. Fall back to grep when the index returns nothing useful.
3. **Remember at the end**, at most three facts per task: decisions and why, root causes,
   gotchas, how-tos, user preferences. One self-contained sentence or short paragraph each.
   - `type`: `semantic` for facts, `procedural` for how-tos and rules.
   - `scope`: `global` (everyone), `project` (with `project_id`) or `agent` (only you).
   - `importance` 0.3 to 0.9; `source` such as `commit:repo@sha`, `issue:ID` or `session:date-topic`.
   - No secrets (they are rejected), no transient state, nothing the code already says.
   - Correct an old fact with `supersedes=<id>` rather than adding a contradicting one.
4. Near-duplicates and low-confidence facts go to a review queue (`muninn review list`) instead
   of the active set; that is expected.

## Hooks

`muninn hook prompt|pre|post` are Claude Code and Codex hooks: `prompt` injects recalled
memories into each prompt, `pre` denies grep in repos listed in `MUNINN_HOOK_REPOS` until a
Muninn tool has been used in the session, `post` records that use. When a search is denied,
query the index first, then grep is allowed.
