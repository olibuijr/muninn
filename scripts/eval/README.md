# Retrieval eval

`muninn eval` replays query → expected-hit cases against a running indexd and reports, per mode,
recall@k (an expected hit in the top k), MRR (1/rank of the first expected hit, 0 past k),
path@k (the expected *file* in the top k, symbol ignored) and client-side p50/p95 latency.

## Run

```sh
scripts/eval/run.sh                                  # all cases-*.tsv, --mode all -k 10
scripts/eval/run.sh -v --json /tmp/eval.json         # list misses with the top 3 hits, write JSON
scripts/eval/run.sh --mode hybrid --min-recall 0.75  # exits 1 when recall@k is below 0.75
```

`run.sh` takes `MUNINN_INDEXD_TOKEN` from `~/.config/muninn/index.env` when unset,
`MUNINN_INDEXD_URL` (default `http://127.0.0.1:7410`) and `MUNINN_BIN` (default `cargo run
--release -p muninn-cli`). The CLI directly:

```
muninn eval <cases.tsv>... [--mode lexical|semantic|hybrid|all] [-k N] [--repo R] [--branch B]
            [--json out.json] [--min-recall X] [--expand none|heuristic|llm|both]
            [--param key=value]... [-v]
muninn eval --compare a.json b.json [-v]
```

`--repo` keeps only that repo's cases; `--min-recall` must hold for every mode that ran.
A request that fails or returns no hits counts in `err` (no hits usually means the repo is not
loaded yet, e.g. right after an indexd restart: check `/readyz` before trusting a run).
Requests are sent one at a time, so latency is per query.

## Case files

TSV, `#` comment lines: `query<TAB>repo<TAB>expected[<TAB>tag]`. `expected` is `path` or
`path#symbol`, alternatives separated by `|`. A hit matches when its path equals and, for
`path#symbol`, its symbol equals too. The tag groups the report (`doc`, `name`, `path`).

Generate cases from the index (read-only psql; deterministic for a given seed and index state):

```sh
scripts/eval/gen-cases.sh myrepo 60 > scripts/eval/cases-myrepo.tsv
```

- `doc`: first sentence of the item's doc comment (`ix_sym_defs.doc`, else the comment block
  above the item), expected `path#symbol`.
- `name`: `fn parse http request` from `parse_http_request`; names in more than 3 files and
  short one-word names are skipped, names in 2-3 files list every file.
- `path` (repos with no symbol chunks, e.g. Swift): file name words, expected `path`.

Hand-written cases can go in any extra `cases-*.tsv`.

Case files are about your own code, so they are not committed here (`scripts/eval/cases-*.tsv`
is ignored); keep them in your fork or next to your index.

## What the numbers said so far

On hand-written task-style questions ("where does the compositor decide which window gets
keyboard focus") over two private codebases (126 cases, k=10): semantic search alone reached
recall@10 0.76, hybrid 0.73, lexical 0.37; LLM query expansion lifted hybrid to 0.79 at about
0.7 s per query, so it is opt-in (`expand=llm`). Generated name and doc queries reach path@10
around 0.98.
