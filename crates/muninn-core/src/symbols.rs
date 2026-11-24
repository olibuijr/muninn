use crate::pg::PgStore;
use crate::scip::Graph;
use postgres::GenericClient;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub const SCHEMA: &str = "
create table if not exists ix_sym_defs(
  repo text not null, symbol text not null, path text not null, start_line int not null, end_line int not null,
  name text not null, kind text not null, doc text, primary key(repo, symbol, path, start_line));
create index if not exists ix_sym_defs_name on ix_sym_defs(repo, lower(name));
create table if not exists ix_sym_refs(
  repo text not null, symbol text not null, path text not null, line int not null, col int not null);
create index if not exists ix_sym_refs_sym on ix_sym_refs(repo, symbol);
create table if not exists ix_sym_calls(
  repo text not null, caller text not null, callee text not null, path text not null, line int not null);
create index if not exists ix_sym_calls_callee on ix_sym_calls(repo, callee);
create index if not exists ix_sym_calls_caller on ix_sym_calls(repo, caller);
create table if not exists ix_sym_meta(repo text primary key, indexed_at timestamptz not null default now(), commit_sha text, defs int, refs int, calls int);
create index if not exists ix_sym_defs_path on ix_sym_defs(repo, path, start_line);
create extension if not exists pg_trgm;
create index if not exists ix_sym_defs_name_trgm on ix_sym_defs using gin (lower(name) gin_trgm_ops);";

/// Kinds symbol_search returns (locals, fields and variables are noise in a name search).
const SEARCH_KINDS: &str =
    "('function','method','struct','enum','trait','interface','constructor','module')";
/// SQL rank of a definition kind (lower first); mirrors `kind_priority`.
const KIND_ORDER: &str = "case when d.kind in ('function','method','constructor','struct','enum','trait','interface') then 0 when d.kind in ('module','symbol') then 1 else 2 end";
/// Most definitions explain_symbol expands with code and call graph.
pub const EXPLAIN_MAX_DEFS: usize = 5;
/// Most code lines explain_symbol returns per definition.
pub const CODE_MAX_LINES: usize = 80;
/// Files listed per definition in explain_symbol's reference summary.
const EXPLAIN_REF_FILES: i64 = 10;

/// Rank of a definition kind for ordering results: types and callables first, then modules
/// and impl members, then variables and fields.
pub fn kind_priority(kind: &str) -> u8 {
    match kind {
        "function" | "method" | "constructor" | "struct" | "enum" | "trait" | "interface" => 0,
        "module" | "symbol" => 1,
        _ => 2,
    }
}

/// Lower-case identifier words: split on non-alphanumerics and camelCase
/// (`parseHTTPRequest_v2` -> parse, http, request, v2).
pub fn ident_parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        // rank::tokenize puts the whole lower-cased word first when it splits it.
        let t = crate::rank::tokenize(w);
        let skip = usize::from(t.len() > 1);
        out.extend(t.into_iter().skip(skip));
    }
    out
}

/// Match labels, indexed by `match_level`.
pub const MATCH_LABELS: [&str; 5] = ["exact", "prefix", "words", "substring", "fuzzy"];

/// How well a symbol name matches a query, best first: 0 exact (case-insensitive, or the same
/// words: `parse_http` = `parseHttp`), 1 prefix (string or word-sequence prefix), 2 every query
/// word starts a name word in any order, 3 substring, 4 every query word is a substring.
/// None when the name does not match at all.
pub fn match_level(name: &str, q: &str) -> Option<u8> {
    let (ln, lq) = (name.to_lowercase(), q.trim().to_lowercase());
    if lq.is_empty() {
        return None;
    }
    if ln == lq {
        return Some(0);
    }
    let (np, qp) = (ident_parts(name), ident_parts(q));
    if !qp.is_empty() && np == qp {
        return Some(0);
    }
    if ln.starts_with(&lq) {
        return Some(1);
    }
    if !qp.is_empty()
        && qp.len() <= np.len()
        && qp.iter().zip(&np).enumerate().all(|(i, (a, b))| {
            if i + 1 == qp.len() {
                b.starts_with(a.as_str())
            } else {
                a == b
            }
        })
    {
        return Some(1);
    }
    if !qp.is_empty()
        && qp
            .iter()
            .all(|a| np.iter().any(|b| b.starts_with(a.as_str())))
    {
        return Some(2);
    }
    if ln.contains(&lq) {
        return Some(3);
    }
    if !qp.is_empty() && qp.iter().all(|a| ln.contains(a.as_str())) {
        return Some(4);
    }
    None
}

/// Drops rows whose `name` does not match `q`, orders the rest by match level, kind priority,
/// name length, repo, path and line, and labels each with `match`.
pub fn rank_symbols(q: &str, rows: Vec<Value>) -> Vec<Value> {
    let mut v: Vec<(u8, Value)> = rows
        .into_iter()
        .filter_map(|r| match_level(r["name"].as_str().unwrap_or(""), q).map(|l| (l, r)))
        .collect();
    let key = |l: &u8, r: &Value| {
        (
            *l,
            kind_priority(r["kind"].as_str().unwrap_or("")),
            r["name"].as_str().unwrap_or("").len(),
            r["repo"].as_str().unwrap_or("").to_string(),
            r["path"].as_str().unwrap_or("").to_string(),
            r["start_line"].as_i64().unwrap_or(0),
        )
    };
    v.sort_by_cached_key(|(l, r)| key(l, r));
    v.into_iter()
        .map(|(l, mut r)| {
            r["match"] = json!(MATCH_LABELS[l as usize]);
            r
        })
        .collect()
}

/// Lines `start..=end` (1-based file lines) of `text`, whose first line is file line
/// `text_first`, capped at `max` lines. Returns (code, first, last, truncated); None when the
/// range misses the text.
pub fn clip_code(
    text: &str,
    text_first: usize,
    start: usize,
    end: usize,
    max: usize,
) -> Option<(String, usize, usize, bool)> {
    let lines: Vec<&str> = text.lines().collect();
    let first = start.max(text_first);
    let last_avail = text_first + lines.len().saturating_sub(1);
    if lines.is_empty() || first > last_avail || first > end {
        return None;
    }
    let want = end.min(last_avail);
    let last = want.min(first + max.max(1) - 1);
    let code = lines[first - text_first..=last - text_first].join("\n");
    Some((code, first, last, last < want))
}

/// Follow-up calls for one explain_symbol definition, with exact arguments, so an agent can
/// continue without guessing: read the rest of a truncated definition, list every referencing
/// file, page through callers/callees, or open the definition with context.
pub fn next_hints(d: &Value) -> Vec<Value> {
    let (repo, path, sym) = (&d["repo"], &d["path"], &d["symbol"]);
    let s = d["start_line"].as_i64().unwrap_or(1);
    let e = d["end_line"].as_i64().unwrap_or(s);
    let mut read = json!({"repo": repo, "path": path});
    if let Some(b) = d["branch"].as_str() {
        read["branch"] = json!(b);
    }
    let mut out = Vec::new();
    if d["code_truncated"].as_bool() == Some(true) {
        let from = d["code_end"].as_i64().unwrap_or(s) + 1;
        let mut a = read.clone();
        a["start_line"] = json!(from);
        a["end_line"] = json!(e);
        out.push(json!({"tool": "read_file", "why": "rest of the definition", "args": a}));
    } else {
        let mut a = read.clone();
        a["start_line"] = json!((s - 10).max(1));
        a["end_line"] = json!(e + 10);
        out.push(
            json!({"tool": "read_file", "why": "definition with surrounding lines", "args": a}),
        );
    }
    if d["source"] == "scip" {
        let shown = d["references"]["files"].as_array().map_or(0, |f| f.len()) as i64;
        if d["references"]["file_count"].as_i64().unwrap_or(0) > shown {
            out.push(json!({"tool": "symbol_lookup", "why": "every file that references it",
                "args": {"name": sym, "kind": "refs", "repo": repo, "group_by_file": true, "limit": 200}}));
        }
        for side in ["callers", "callees"] {
            let shown = d[side]["top"].as_array().map_or(0, |t| t.len()) as i64;
            if d[side]["distinct"].as_i64().unwrap_or(0) > shown {
                out.push(
                    json!({"tool": "symbol_lookup", "why": format!("all {side} with call sites"),
                    "args": {"name": sym, "kind": side, "repo": repo, "limit": 200}}),
                );
            }
        }
    }
    out
}

/// A search hit's line span, for `symbols_for_hits`.
#[derive(Debug, Clone, Deserialize)]
pub struct HitSpan {
    pub repo: String,
    pub path: String,
    pub start_line: i32,
    pub end_line: i32,
}

type Row = postgres::Row;

/// Tree-sitter definitions from `ix_chunks`, one per (repo, path, start line). Columns: repo,
/// path, start_line, end_line, symbol, kind, hash, text.
fn chunk_def_rows<C: GenericClient>(
    c: &mut C,
    name: &str,
    repo: Option<&str>,
    limit: i64,
) -> Result<Vec<Row>, postgres::Error> {
    c.query(
        "select distinct on (repo, path, start_line) repo, path, start_line, end_line, symbol, kind, hash, text
         from ix_chunks
         where ($2::text is null or repo=$2) and lower(symbol)=lower($1) and kind <> 'window'
         order by repo, path, start_line limit $3",
        &[&name, &repo, &limit],
    )
}

fn chunk_def_json(r: &Row) -> Value {
    let (repo, hash): (String, String) = (r.get(0), r.get(6));
    json!({"repo": repo, "symbol": format!("chunk:{repo}:{hash}"), "path": r.get::<_,String>(1),
           "start_line": r.get::<_,i32>(2), "end_line": r.get::<_,i32>(3), "name": r.get::<_,String>(4),
           "kind": r.get::<_,String>(5), "doc": Value::Null, "source": "tree-sitter"})
}

/// SCIP definitions named `name` (case-insensitive) or with full SCIP symbol `name` (it has
/// spaces), joined to their repo's index metadata. Columns: repo, symbol, path, start_line,
/// end_line, name, kind, doc, commit_sha, indexed_at. `by_refs` orders definitions of the same
/// kind rank by reference count (most used first; ~40 ms for the 1.7k `new` defs in a large Rust workspace).
fn def_rows<C: GenericClient>(
    c: &mut C,
    name: &str,
    repo: Option<&str>,
    limit: i64,
    by_refs: bool,
) -> Result<Vec<Row>, postgres::Error> {
    let refs = if by_refs {
        "(select count(*) from ix_sym_refs r where r.repo=d.repo and r.symbol=d.symbol) desc,"
    } else {
        ""
    };
    let cond = if name.contains(' ') {
        "d.symbol=$1"
    } else {
        "lower(d.name)=lower($1)"
    };
    c.query(
        &format!(
            "select d.repo,d.symbol,d.path,d.start_line,d.end_line,d.name,d.kind,d.doc,m.commit_sha,m.indexed_at::text
             from ix_sym_meta m cross join lateral (
               select * from ix_sym_defs d where d.repo=m.repo and {cond} offset 0) d
             where ($2::text is null or m.repo=$2)
             order by (d.name=$1) desc, {KIND_ORDER}, {refs} d.repo, d.path, d.start_line limit $3"
        ),
        &[&name, &repo, &limit],
    )
}

fn def_json(r: &Row) -> Value {
    json!({"repo": r.get::<_,String>(0), "symbol": r.get::<_,String>(1), "path": r.get::<_,String>(2),
           "start_line": r.get::<_,i32>(3), "end_line": r.get::<_,i32>(4), "name": r.get::<_,String>(5),
           "kind": r.get::<_,String>(6), "doc": r.get::<_,Option<String>>(7)})
}

/// One side of a symbol's call graph, grouped by the other end: distinct count, call-site
/// count and the `top` most frequent partners with their definition and first call site.
fn call_side<C: GenericClient>(
    c: &mut C,
    repo: &str,
    sym: &str,
    callers: bool,
    top: i64,
) -> Result<Value, postgres::Error> {
    let (this, other) = if callers {
        ("callee", "caller")
    } else {
        ("caller", "callee")
    };
    let rows = c.query(
        &format!(
            "with g as (select {other} sym, count(*) n, (array_agg(path order by path,line))[1] p,
                               (array_agg(line order by path,line))[1] l
                        from ix_sym_calls where repo=$1 and {this}=$2 group by {other}),
                  t as (select count(*) dc, coalesce(sum(n),0)::int8 sc from g),
                  top as (select * from g order by n desc, sym limit $3)
             select top.sym, top.n, top.p, top.l, d.name, d.kind, d.path, d.start_line, d.end_line, t.dc, t.sc
             from t left join top on true
             left join lateral (select name,kind,path,start_line,end_line from ix_sym_defs d
                                where d.repo=$1 and d.symbol=top.sym limit 1) d on true
             order by top.n desc nulls last, top.sym"
        ),
        &[&repo, &sym, &top],
    )?;
    let (mut distinct, mut sites) = (0i64, 0i64);
    let mut list = Vec::new();
    for r in &rows {
        distinct = r.get(9);
        sites = r.get(10);
        let Some(s) = r.get::<_, Option<String>>(0) else {
            continue;
        };
        let name = r
            .get::<_, Option<String>>(4)
            .unwrap_or_else(|| crate::scip::symbol_name(&s));
        let def_path: Option<String> = r.get(6);
        list.push(json!({"symbol": s, "name": name, "kind": r.get::<_,Option<String>>(5),
            "path": def_path, "start_line": r.get::<_,Option<i32>>(7), "end_line": r.get::<_,Option<i32>>(8),
            "external": def_path.is_none(), "calls": r.get::<_,i64>(1),
            "first_call": {"path": r.get::<_,String>(2), "line": r.get::<_,i32>(3)}}));
    }
    Ok(json!({"distinct": distinct, "call_sites": sites, "top": list}))
}

/// Reference count for a symbol, with the files that use it most.
fn ref_summary<C: GenericClient>(
    c: &mut C,
    repo: &str,
    sym: &str,
    files: i64,
) -> Result<Value, postgres::Error> {
    let rows = c.query(
        "with f as (select path, count(*) n, min(line) l from ix_sym_refs where repo=$1 and symbol=$2 group by path),
              t as (select count(*) fc, coalesce(sum(n),0)::int8 rc from f),
              top as (select * from f order by n desc, path limit $3)
         select top.path, top.n, top.l, t.fc, t.rc from t left join top on true order by top.n desc nulls last, top.path",
        &[&repo, &sym, &files],
    )?;
    let (mut fc, mut rc) = (0i64, 0i64);
    let mut list = Vec::new();
    for r in &rows {
        fc = r.get(3);
        rc = r.get(4);
        if let Some(p) = r.get::<_, Option<String>>(0) {
            list.push(
                json!({"path": p, "count": r.get::<_,i64>(1), "first_line": r.get::<_,i32>(2)}),
            );
        }
    }
    Ok(json!({"count": rc, "file_count": fc, "files": list}))
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn lit(s: &str) -> String {
    format!("%{}%", esc(s))
}

impl PgStore {
    pub fn ensure_symbols(&self) -> Result<(), postgres::Error> {
        self.db.lock().batch_execute(SCHEMA)
    }

    /// Replace the repo's whole symbol graph atomically.
    pub fn replace_symbols(&self, repo: &str, sha: &str, g: &Graph) -> Result<(), postgres::Error> {
        let mut db = self.db.lock();
        let mut t = db.transaction()?;
        for tbl in ["ix_sym_defs", "ix_sym_refs", "ix_sym_calls"] {
            t.execute(&format!("delete from {tbl} where repo=$1"), &[&repo])?;
        }
        for d in &g.defs {
            t.execute(
                "insert into ix_sym_defs values($1,$2,$3,$4,$5,$6,$7,$8) on conflict do nothing",
                &[
                    &repo,
                    &d.symbol,
                    &d.path,
                    &d.start_line,
                    &d.end_line,
                    &d.name,
                    &d.kind,
                    &d.doc,
                ],
            )?;
        }
        for r in &g.refs {
            t.execute(
                "insert into ix_sym_refs values($1,$2,$3,$4,$5)",
                &[&repo, &r.symbol, &r.path, &r.line, &r.col],
            )?;
        }
        for c in &g.calls {
            t.execute(
                "insert into ix_sym_calls values($1,$2,$3,$4,$5)",
                &[&repo, &c.caller, &c.callee, &c.path, &c.line],
            )?;
        }
        let (d, r, c) = (
            g.defs.len() as i32,
            g.refs.len() as i32,
            g.calls.len() as i32,
        );
        t.execute(
            "insert into ix_sym_meta(repo,commit_sha,defs,refs,calls) values($1,$2,$3,$4,$5)
             on conflict(repo) do update set indexed_at=now(),commit_sha=$2,defs=$3,refs=$4,calls=$5",
            &[&repo, &sha, &d, &r, &c],
        )?;
        t.commit()
    }

    /// Drop a repo's whole symbol graph and its metadata (repo removed from hosting).
    pub fn delete_symbols(&self, repo: &str) -> Result<(), postgres::Error> {
        let mut db = self.db.lock();
        let mut t = db.transaction()?;
        for tbl in ["ix_sym_defs", "ix_sym_refs", "ix_sym_calls", "ix_sym_meta"] {
            t.execute(&format!("delete from {tbl} where repo=$1"), &[&repo])?;
        }
        t.commit()
    }

    /// kind: def | refs | callers | callees. `name` matches the symbol name (exact, case-insensitive)
    /// or a full SCIP symbol; `repo` optionally restricts.
    pub fn symbol_lookup(
        &self,
        name: &str,
        kind: &str,
        repo: Option<&str>,
        limit: i64,
    ) -> Result<Value, String> {
        self.symbol_lookup_opts(name, kind, repo, limit, false)
    }

    /// `symbol_lookup` with options: `group_by_file` turns kind=refs into per-file counts
    /// (`files`: path, count, first lines; most-referencing files first) plus totals.
    pub fn symbol_lookup_opts(
        &self,
        name: &str,
        kind: &str,
        repo: Option<&str>,
        limit: i64,
        group_by_file: bool,
    ) -> Result<Value, String> {
        let e = |x: postgres::Error| x.to_string();
        let mut db = self.db.lock();
        let defs = def_rows(&mut *db, name, repo, 50, false).map_err(e)?;
        if kind == "def" {
            let mut v: Vec<Value> = defs.iter().take(limit as usize).map(def_json).collect();
            if v.is_empty() {
                // No SCIP graph for this name (Swift, or a Rust repo whose rust-analyzer run is
                // stale): fall back to the chunker's tree-sitter definitions.
                v = self.chunk_defs(&mut db, name, repo, limit).map_err(e)?;
                let src = if v.is_empty() { "scip" } else { "tree-sitter" };
                return Ok(json!({"kind": "def", "definitions": v, "source": src}));
            }
            return Ok(json!({"kind": "def", "definitions": v, "source": "scip"}));
        }
        let syms: Vec<(String, String)> = defs.iter().map(|r| (r.get(0), r.get(1))).collect();
        let (mut refs, mut callers, mut callees, mut files) = (vec![], vec![], vec![], vec![]);
        let (mut total_refs, mut total_files) = (0i64, 0i64);
        let mut seen = HashSet::new();
        for (rp, sym) in &syms {
            if !seen.insert((rp, sym)) {
                continue;
            }
            match kind {
                "refs" if group_by_file => {
                    let rows = db
                        .query(
                            "select path, count(*) n, (array_agg(line order by line))[1:5],
                                    (sum(count(*)) over ())::int8, count(*) over ()
                             from ix_sym_refs where repo=$1 and symbol=$2
                             group by path order by n desc, path limit $3",
                            &[rp, sym, &limit],
                        )
                        .map_err(e)?;
                    if let Some(r) = rows.first() {
                        total_refs += r.get::<_, i64>(3);
                        total_files += r.get::<_, i64>(4);
                    }
                    for r in &rows {
                        files.push(json!({"repo": rp, "symbol": sym, "path": r.get::<_,String>(0),
                            "count": r.get::<_,i64>(1), "lines": r.get::<_,Vec<i32>>(2)}));
                    }
                }
                "refs" => {
                    for r in db.query("select path,line,col from ix_sym_refs where repo=$1 and symbol=$2 order by path,line limit $3",
                        &[rp, sym, &limit]).map_err(e)? {
                        refs.push(json!({"repo": rp, "symbol": sym, "path": r.get::<_,String>(0), "line": r.get::<_,i32>(1), "col": r.get::<_,i32>(2)}));
                    }
                }
                "callers" => {
                    for r in db.query(
                        "select c.caller, c.path, c.line, d.name, d.kind, d.start_line from ix_sym_calls c
                         left join lateral (select name, kind, start_line from ix_sym_defs d
                                            where d.repo=c.repo and d.symbol=c.caller limit 1) d on true
                         where c.repo=$1 and c.callee=$2 order by c.path,c.line limit $3",
                        &[rp, sym, &limit]).map_err(e)? {
                        callers.push(json!({"repo": rp, "callee": sym, "caller": r.get::<_,String>(0), "caller_name": r.get::<_,Option<String>>(3).unwrap_or_else(|| crate::scip::symbol_name(&r.get::<_,String>(0))),
                            "caller_kind": r.get::<_,Option<String>>(4), "path": r.get::<_,String>(1), "line": r.get::<_,i32>(2)}));
                    }
                }
                "callees" => {
                    for r in db.query(
                        "select c.callee, c.path, c.line, d.name from ix_sym_calls c
                         left join lateral (select name from ix_sym_defs d
                                            where d.repo=c.repo and d.symbol=c.callee limit 1) d on true
                         where c.repo=$1 and c.caller=$2 order by c.path,c.line limit $3",
                        &[rp, sym, &limit]).map_err(e)? {
                        callees.push(json!({"repo": rp, "caller": sym, "callee": r.get::<_,String>(0), "callee_name": r.get::<_,Option<String>>(3).unwrap_or_else(|| crate::scip::symbol_name(&r.get::<_,String>(0))),
                            "path": r.get::<_,String>(1), "line": r.get::<_,i32>(2)}));
                    }
                }
                _ => return Err("kind must be def|refs|callers|callees".into()),
            }
        }
        let definitions: Vec<Value> = defs.iter().take(10).map(def_json).collect();
        Ok(match kind {
            "refs" if group_by_file => {
                json!({"kind": "refs", "definitions": definitions, "files": files,
                       "total_references": total_refs, "total_files": total_files})
            }
            "refs" => json!({"kind": "refs", "definitions": definitions, "references": refs}),
            "callers" => json!({"kind": "callers", "definitions": definitions, "callers": callers}),
            _ => json!({"kind": "callees", "definitions": definitions, "callees": callees}),
        })
    }

    /// Definitions by name from the tree-sitter chunks (`ix_chunks.symbol`), one per
    /// (repo, path, start line). Used when the SCIP graph has nothing.
    fn chunk_defs(
        &self,
        db: &mut postgres::Client,
        name: &str,
        repo: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, postgres::Error> {
        Ok(chunk_def_rows(db, name, repo, limit)?
            .iter()
            .map(chunk_def_json)
            .collect())
    }

    /// Symbol-name search for all-sources search. Word-aware: the query is split into
    /// identifier words (non-alphanumerics and camelCase), Postgres prefilters names that
    /// contain every word (pg_trgm GIN on lower(name)), and `rank_symbols` keeps real matches
    /// ranked exact > prefix > all words > substring, then by kind and name length. Repos
    /// without a SCIP graph contribute their tree-sitter chunk symbols instead.
    pub fn symbol_search(&self, q: &str, limit: i64) -> Vec<Value> {
        let lq = q.trim().to_lowercase();
        if lq.is_empty() {
            return vec![];
        }
        let words = ident_parts(q);
        let pats: Vec<String> = if words.is_empty() {
            vec![lit(&lq)]
        } else {
            words.iter().take(6).map(|w| lit(w)).collect()
        };
        let prefix = format!("{}%", esc(&lq));
        let pool = (limit * 4).max(400);
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&lq, &prefix, &pool];
        for p in &pats {
            params.push(p);
        }
        let conds = |col: &str| {
            (0..pats.len())
                .map(|i| format!("lower({col}) like ${} escape '\\'", i + 4))
                .collect::<Vec<_>>()
                .join(" and ")
        };
        let mut db = self.db.lock();
        let scip = db
            .query(
                &format!(
                    "select repo,path,start_line,end_line,name,kind,symbol from ix_sym_defs
                     where kind in {SEARCH_KINDS} and {}
                     order by (lower(name)=$1) desc, (lower(name) like $2 escape '\\') desc, length(name) limit $3",
                    conds("name")
                ),
                &params,
            )
            .unwrap_or_default()
            .iter()
            .map(|r| json!({"source": "symbol", "repo": r.get::<_,String>(0), "path": r.get::<_,String>(1),
                "start_line": r.get::<_,i32>(2), "end_line": r.get::<_,i32>(3), "name": r.get::<_,String>(4),
                "kind": r.get::<_,String>(5), "symbol": r.get::<_,String>(6)}))
            .collect::<Vec<Value>>();
        let mut out = rank_symbols(q, scip);
        out.truncate(limit as usize);
        if (out.len() as i64) < limit {
            let chunks = db
                .query(
                    &format!(
                        "select * from (select distinct on (repo, path, start_line) repo, path, start_line, end_line, symbol, kind, hash
                         from ix_chunks
                         where {} and kind <> 'window'
                           and repo not in (select repo from ix_sym_meta where defs > 0)
                         order by repo, path, start_line) c
                         order by (lower(symbol)=$1) desc, (lower(symbol) like $2 escape '\\') desc, length(symbol) limit $3",
                        conds("symbol")
                    ),
                    &params,
                )
                .unwrap_or_default()
                .iter()
                .map(|r| {
                    let (repo, hash): (String, String) = (r.get(0), r.get(6));
                    json!({"source": "symbol", "repo": repo, "path": r.get::<_,String>(1),
                        "start_line": r.get::<_,i32>(2), "end_line": r.get::<_,i32>(3), "name": r.get::<_,String>(4),
                        "kind": r.get::<_,String>(5), "symbol": format!("chunk:{repo}:{hash}")})
                })
                .collect();
            let rest = (limit as usize).saturating_sub(out.len());
            out.extend(rank_symbols(q, chunks).into_iter().take(rest));
        }
        out
    }

    /// Everything an agent needs about a symbol in one call. For up to EXPLAIN_MAX_DEFS
    /// definitions (distinct repo+symbol, most referenced first) named `name` (or full SCIP
    /// symbol `name`): the definition, reference count with top files, and callers/callees grouped by partner
    /// symbol (`top` each). SCIP definitions carry `commit` (the indexed revision) and no code:
    /// the caller reads it from git. Repos without a SCIP definition fall back to tree-sitter
    /// chunk definitions with their code and no graph. `repo_ok` filters out repos that are no
    /// longer hosted. All queries share one transaction with a 5 s statement timeout.
    pub fn symbol_explain(
        &self,
        name: &str,
        repo: Option<&str>,
        top: i64,
        repo_ok: &dyn Fn(&str) -> bool,
    ) -> Result<Value, String> {
        let e = |x: postgres::Error| x.to_string();
        let mut db = self.db.lock();
        let mut t = db.transaction().map_err(e)?;
        t.batch_execute("set local statement_timeout = '5s'")
            .map_err(e)?;
        let rows = def_rows(&mut t, name, repo, 200, true).map_err(e)?;
        let mut skipped: Vec<String> = Vec::new();
        let rows: Vec<&Row> = rows
            .iter()
            .filter(|r| {
                let rp: String = r.get(0);
                let ok = repo_ok(&rp);
                if !ok && !skipped.contains(&rp) {
                    skipped.push(rp);
                }
                ok
            })
            .collect();
        let mut notes: Vec<String> = Vec::new();
        if !skipped.is_empty() {
            notes.push(format!(
                "skipped definitions in repos that are no longer hosted: {}",
                skipped.join(", ")
            ));
        }
        if rows.is_empty() {
            let cs = chunk_def_rows(&mut t, name, repo, 50).map_err(e)?;
            let cs: Vec<&Row> = cs
                .iter()
                .filter(|r| repo_ok(&r.get::<_, String>(0)))
                .collect();
            if cs.is_empty() {
                notes.push(format!("no definition named {name:?}; try code_search with mode=lexical, or symbol_lookup for partial names via search_all"));
                return Ok(
                    json!({"query": name, "source": "none", "definitions": [], "total_definitions": 0, "note": notes.join("; ")}),
                );
            }
            let defs: Vec<Value> = cs
                .iter()
                .take(EXPLAIN_MAX_DEFS)
                .map(|r| {
                    let mut d = chunk_def_json(r);
                    let (s, en): (i32, i32) = (r.get(2), r.get(3));
                    if let Some((code, a, b, tr)) = clip_code(
                        &r.get::<_, String>(7),
                        s as usize,
                        s as usize,
                        en as usize,
                        CODE_MAX_LINES,
                    ) {
                        d["code"] = json!(code);
                        d["code_start"] = json!(a);
                        d["code_end"] = json!(b);
                        d["code_truncated"] = json!(tr);
                    }
                    d
                })
                .collect();
            notes.push("no SCIP graph for this name (repo not SCIP-indexed or index stale): definitions come from tree-sitter chunks; references and callers are not available, use code_search mode=lexical for usages".into());
            return Ok(
                json!({"query": name, "source": "tree-sitter", "definitions": defs,
                "total_definitions": cs.len(), "note": notes.join("; ")}),
            );
        }
        let mut seen = HashSet::new();
        let (mut detailed, mut others) = (Vec::new(), Vec::new());
        for r in &rows {
            let key: (String, String) = (r.get(0), r.get(1));
            if detailed.len() < EXPLAIN_MAX_DEFS && seen.insert(key) {
                detailed.push(*r);
            } else if others.len() < 20 {
                let mut o = def_json(r);
                o.as_object_mut().unwrap().remove("doc");
                others.push(o);
            }
        }
        let mut defs = Vec::new();
        for r in detailed {
            let (rp, sym): (String, String) = (r.get(0), r.get(1));
            let mut d = def_json(r);
            d["source"] = json!("scip");
            d["commit"] = json!(r.get::<_, Option<String>>(8));
            d["indexed_at"] = json!(r.get::<_, String>(9));
            d["references"] = ref_summary(&mut t, &rp, &sym, EXPLAIN_REF_FILES).map_err(e)?;
            d["callers"] = call_side(&mut t, &rp, &sym, true, top).map_err(e)?;
            d["callees"] = call_side(&mut t, &rp, &sym, false, top).map_err(e)?;
            defs.push(d);
        }
        let mut out = json!({"query": name, "source": "scip", "definitions": defs,
            "total_definitions": rows.len(), "other_definitions": others});
        if rows.len() > EXPLAIN_MAX_DEFS {
            notes.push(format!("{} definitions match; the first {EXPLAIN_MAX_DEFS} distinct symbols are expanded (functions and types first). Pass repo= or a full SCIP symbol from other_definitions to pick one", rows.len()));
        }
        if !notes.is_empty() {
            out["note"] = json!(notes.join("; "));
        }
        Ok(out)
    }

    /// For each hit span, the SCIP definition that best covers it (highest line-range overlap,
    /// i.e. intersection over union; variables and fields are skipped) with its distinct caller
    /// and callee counts, or null when the repo has no SCIP graph there. One query per repo.
    pub fn symbols_for_hits(&self, hits: &[HitSpan]) -> Result<Vec<Value>, String> {
        let mut out = vec![Value::Null; hits.len()];
        let mut by_repo: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, h) in hits.iter().enumerate() {
            by_repo.entry(&h.repo).or_default().push(i);
        }
        let mut db = self.db.lock();
        for (repo, idx) in by_repo {
            let paths: Vec<&str> = idx.iter().map(|&i| hits[i].path.as_str()).collect();
            let starts: Vec<i32> = idx.iter().map(|&i| hits[i].start_line).collect();
            let ends: Vec<i32> = idx
                .iter()
                .map(|&i| hits[i].end_line.max(hits[i].start_line))
                .collect();
            let rows = db
                .query(
                    "select h.i, d.symbol, d.name, d.kind, d.start_line, d.end_line, d.iou,
                       (select count(distinct c.caller) from ix_sym_calls c where c.repo=$1 and c.callee=d.symbol),
                       (select count(distinct c.callee) from ix_sym_calls c where c.repo=$1 and c.caller=d.symbol)
                     from unnest($2::text[], $3::int4[], $4::int4[]) with ordinality h(path, s, e, i)
                     join lateral (
                       select symbol, name, kind, start_line, end_line,
                         (least(end_line, h.e) - greatest(start_line, h.s) + 1)::float8
                           / (greatest(end_line, h.e) - least(start_line, h.s) + 1) iou
                       from ix_sym_defs
                       where repo=$1 and path=h.path and start_line <= h.e and end_line >= h.s
                         and kind not in ('variable', 'field')
                       order by iou desc, end_line - start_line, start_line limit 1) d on true",
                    &[&repo, &paths, &starts, &ends],
                )
                .map_err(|e| e.to_string())?;
            for r in rows {
                let k = r.get::<_, i64>(0) as usize - 1;
                let iou: f64 = r.get(6);
                out[idx[k]] = json!({"symbol": r.get::<_,String>(1), "name": r.get::<_,String>(2),
                    "kind": r.get::<_,String>(3), "start_line": r.get::<_,i32>(4), "end_line": r.get::<_,i32>(5),
                    "overlap": (iou * 100.0).round() / 100.0, "callers": r.get::<_,i64>(7), "callees": r.get::<_,i64>(8)});
            }
        }
        Ok(out)
    }

    pub fn symbol_stats(&self) -> Value {
        let mut db = self.db.lock();
        let rows = db.query("select repo,indexed_at::text,commit_sha,defs,refs,calls from ix_sym_meta order by repo", &[]).unwrap_or_default();
        json!(rows.iter().map(|r| json!({"repo": r.get::<_,String>(0), "indexed_at": r.get::<_,String>(1),
            "commit": r.get::<_,Option<String>>(2), "defs": r.get::<_,i32>(3), "refs": r.get::<_,i32>(4), "calls": r.get::<_,i32>(5)})).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_parts_split_case_and_separators() {
        assert_eq!(
            ident_parts("parseHTTPRequest_v2"),
            ["parse", "http", "request", "v2"]
        );
        assert_eq!(ident_parts("symbol lookup"), ["symbol", "lookup"]);
        assert_eq!(ident_parts("Foo::bar"), ["foo", "bar"]);
        assert!(ident_parts("::").is_empty());
    }

    #[test]
    fn match_levels_order_exact_prefix_words_substring() {
        assert_eq!(match_level("symbol_lookup", "SYMBOL_LOOKUP"), Some(0));
        assert_eq!(match_level("symbolLookup", "symbol_lookup"), Some(0));
        assert_eq!(match_level("symbol_lookup_opts", "symbol_lookup"), Some(1));
        assert_eq!(match_level("symbolLookupOpts", "symbol look"), Some(1));
        assert_eq!(match_level("lookup_symbol", "symbol lookup"), Some(2));
        assert_eq!(match_level("my_symbol_lookup", "symbol_lookup"), Some(2));
        assert_eq!(match_level("resymbolize", "symbol"), Some(3));
        assert_eq!(match_level("reparse_requests", "parse request"), Some(4));
        assert_eq!(match_level("parse", "request"), None);
        assert_eq!(match_level("x", "  "), None);
    }

    #[test]
    fn rank_symbols_orders_by_match_then_kind_then_length() {
        let row = |name: &str, kind: &str| json!({"name": name, "kind": kind, "repo": "r", "path": "p", "start_line": 1});
        let r = rank_symbols(
            "explain",
            vec![
                row("unexplained", "function"),
                row("explain_symbol", "variable"),
                row("explain_symbol_long", "function"),
                row("explain", "module"),
                row("explain", "function"),
                row("other", "function"),
            ],
        );
        let got: Vec<(&str, &str, &str)> = r
            .iter()
            .map(|v| {
                (
                    v["name"].as_str().unwrap(),
                    v["kind"].as_str().unwrap(),
                    v["match"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("explain", "function", "exact"),
                ("explain", "module", "exact"),
                ("explain_symbol_long", "function", "prefix"),
                ("explain_symbol", "variable", "prefix"),
                ("unexplained", "function", "substring"),
            ]
        );
    }

    #[test]
    fn clip_code_ranges() {
        let text = "a\nb\nc\nd\ne";
        // text starts at file line 10
        assert_eq!(
            clip_code(text, 10, 11, 13, 80),
            Some(("b\nc\nd".into(), 11, 13, false))
        );
        assert_eq!(
            clip_code(text, 10, 1, 99, 2),
            Some(("a\nb".into(), 10, 11, true))
        );
        assert_eq!(clip_code(text, 10, 20, 30, 80), None);
        assert_eq!(clip_code("", 1, 1, 5, 80), None);
    }

    #[test]
    fn next_hints_point_at_the_rest() {
        let d = json!({"repo": "r", "path": "src/a.rs", "symbol": "s f().", "start_line": 10, "end_line": 200,
            "source": "scip", "branch": "abc123", "code_truncated": true, "code_end": 89,
            "references": {"count": 40, "file_count": 12, "files": [{}, {}]},
            "callers": {"distinct": 3, "top": [{}, {}, {}]}, "callees": {"distinct": 9, "top": [{}]}});
        let h = next_hints(&d);
        assert_eq!(h[0]["tool"], "read_file");
        assert_eq!(
            h[0]["args"],
            json!({"repo": "r", "path": "src/a.rs", "branch": "abc123", "start_line": 90, "end_line": 200})
        );
        assert_eq!(h[1]["args"]["group_by_file"], true);
        assert_eq!(h[1]["args"]["name"], "s f().");
        assert_eq!(h.len(), 3, "callers are all shown, callees are not");
        assert_eq!(h[2]["args"]["kind"], "callees");
        let ts = json!({"repo": "r", "path": "a.swift", "start_line": 3, "end_line": 5, "source": "tree-sitter"});
        let h = next_hints(&ts);
        assert_eq!(h.len(), 1);
        assert_eq!(
            h[0]["args"],
            json!({"repo": "r", "path": "a.swift", "start_line": 1, "end_line": 15})
        );
    }
}
