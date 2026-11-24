# Deploy and operate

`install.sh` puts the binaries in `~/.local/bin`, the helper scripts in `~/.local/share/muninn`
and the units below in `~/.config/systemd/user`. Settings live in `~/.config/muninn/index.env`
(0600; start from `index.env.example`), plus `memoryd.env` and `indexweb.env` when used.

| Unit | Default bind | Notes |
|---|---|---|
| `muninn-indexd` | 127.0.0.1:7410 | needs `MUNINN_PG_URL`, `MUNINN_INDEXD_TOKEN`, `MUNINN_WEBHOOK_SECRET`, model URLs |
| `muninn-mcp` | 127.0.0.1:7411 | `/mcp` (streamable HTTP); tokens from `MUNINN_TOKENS_FILE` |
| `muninn-memoryd` | 127.0.0.1:7412 | same tokens file |
| `muninn-indexweb` | 0.0.0.0:7415 | `MUNINN_INDEXWEB_TOKEN` (16+ chars) in `indexweb.env`; LAN only |
| `muninn-watch` | - | `MUNINN_WATCH=repo=/checkout,...` |
| `muninn-enroll.timer` | - | every 15 min: hook and index new mirrors |
| `muninn-lifecycle.timer` | - | weekly memory consolidation, expiry, re-embedding |
| `muninn-scip.timer` | - | daily SCIP symbols for `MUNINN_SCIP_REPOS` |

To serve other machines, set the binds to a LAN address or put a TLS reverse proxy in front of
7411 and 7412 (and 7410 for the CLI). Every route except `/healthz`, `/readyz` and indexd's
`/webhook/push` (signed with `MUNINN_WEBHOOK_SECRET` instead) needs a bearer.

## Tokens and scopes

`MUNINN_TOKENS_FILE` is a JSON list of `{"sha256", "actor", "scopes", "projects"}`, hashes only
(`printf %s "$tok" | sha256sum`). Scopes:

- `index:read`: code tools.
- `memory:read`: recall, search_all over memory. Sees global facts, project facts for the listed
  `projects`, and its own agent-scoped facts (Postgres row-level security enforces this).
- `memory:write`: remember. `memory:forget`: forget your own facts and decide review items.

memoryd and mcp read the file at start: restart them after adding a token. Without the file, mcp falls back to a single
`MUNINN_MCP_TOKEN` with `index:read` only (no memory tools); with neither, mcp serves the index
tools to anyone who can reach it.

## Reindex

- **Pushes:** each bare mirror's `hooks/reference-transaction` (a symlink to
  `~/.local/share/muninn/hooks/reference-transaction`) signs the ref update with
  `MUNINN_WEBHOOK_SECRET` and posts `/webhook/push`. Ingest reads changed files through one
  `git cat-file --batch`, drops files over 512 KB and non-UTF-8 files, and embeds in batches.
  Search keeps answering during an ingest.
- **Keep mirrors current:** push to them as a second remote, or `git fetch` from a timer. Either
  fires the hook.
- **Uncommitted edits:** `muninn-watch` polls `git status` each second and posts a snapshot to
  `/live`. Unfiltered searches show the working-tree version (branch `live`); `branch=main`
  ignores the overlay.
- **By hand:** `~/.local/share/muninn/backfill.sh REPO...` reindexes the named mirrors.
- **Ignore files:** a `.muninn-ignore` at the repo root keeps paths out (`dir/`, `*.ext` or a path
  per line). `MUNINN_IGNORE_FILES=.muninn-ignore,.other-ignore` reads more files.

## Symbols (SCIP)

indexd can run `rust-analyzer scip` and `scip-typescript` itself after a push, inside its own 1 GB
cgroup. Large Rust workspaces need several GB, so run those with `muninn-scip` instead:

```sh
muninn-scip run --repo api                      # git archive, scip per project, load into Postgres
muninn-scip ingest --repo api --sha S a.scip    # load SCIP files built elsewhere
systemctl --user enable --now muninn-scip.timer # daily, for MUNINN_SCIP_REPOS
```

`scip-reindex.sh` runs each repo in a `systemd-run --scope` with `MemoryHigh`/`MemoryMax`
(`MUNINN_SCIP_MEM_HIGH`/`_MAX`, default 5G/6G), skips a repo when less than
`MUNINN_SCIP_MIN_AVAIL_GB` (7) is free, and holds `MUNINN_SCIP_LOCK` so it never overlaps a
heavy build that takes the same lock. Other knobs: `MUNINN_RUST_ANALYZER`, `MUNINN_SCIP_TOOLCHAIN`,
`MUNINN_SCIP_JOBS`, `MUNINN_SCIP_WORK`, `MUNINN_SCIP_TARGET`.

## Memory

- `POST /remember {text, type, scope, project_id?, source?, confidence?, importance?, supersedes?}`.
  Cosine >= 0.97 with rerank agreement is a duplicate; 0.85-0.97, confidence below 0.6 or a
  conflict with an active fact goes to the review queue.
- `POST /recall {query, k?, scope?, project_id?}`: hybrid recall, reranked over the top
  `clamp(k, 10, 20)` candidates. `project_id` keeps only memories stored with that project
  (row-level security still applies).
- `POST /forget {id}`: tombstone (text and vector removed, audit row kept).
- Review queue: `muninn review list | approve ID [--supersede] | reject ID | approve-all --older-than 7d`.
- `muninn-memoryd lifecycle all` (weekly timer): consolidates clusters of old episodic memories,
  expires TTLs and re-embeds rows from an older `MUNINN_EMBED_VERSION`.

## Backups

Everything lives in Postgres (the mirrors can be re-cloned): `pg_dump -Fc muninn` from a timer is
enough. `audit_log` is append-only by trigger; restore it with the trigger disabled.
