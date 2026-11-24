use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Cfg {
    pub indexd: String,
    pub memoryd: String,
    pub token: Option<String>,
    pub tokens: Tokens,
}

use muninn_core::{auth::Tokens, memory::Ctx};

const MEM_TOOLS: [(&str, &str, &str); 3] = [
    ("recall", "memory:read", "recall"),
    ("remember", "memory:write", "remember"),
    ("forget", "memory:forget", "forget"),
];

pub fn all_tools() -> Vec<Value> {
    let mut v = vec![json!({
        "name": "code_search",
        "title": "Code search",
        "description": "Search indexed repositories by concept or exact text (BM25 + dense, RRF fused). Returns file, symbol, kind, line range and snippet per hit. Use it when you do not know the symbol name yet (\"where is retry backoff handled\") or need text that is not a definition (strings, config, comments); once you know a function/type name, explain_symbol gives its definition, usages and call graph in one call. Use mode=lexical for exact strings/identifiers; narrow with path (glob) and kind; set context to get the whole chunk plus surrounding lines instead of a 12-line preview. Follow up with read_file to see more of a file. Use list_repos to see which repos and branches exist.",
        "inputSchema": { "type": "object", "required": ["query"], "properties": {
            "query": {"type": "string"}, "repo": {"type": "string"}, "branch": {"type": "string"},
            "mode": {"type": "string", "enum": ["lexical", "semantic", "hybrid"], "default": "hybrid"},
            "k": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
            "path": {"type": "string", "description": "Glob on the file path: * and ? within a segment, ** across segments (crates/**/src/*.rs). A pattern without / also matches the file name (*.rs)."},
            "kind": {"type": "string", "description": "Exact chunk kind, e.g. function_item, impl_item, struct_item, function_declaration, class_declaration, window (plain text window)."},
            "context": {"type": "integer", "minimum": 0, "maximum": 50, "description": "Lines before and after the chunk; the snippet becomes the chunk's full lines plus context (snippet_start gives its first line). Not applied to live (uncommitted) hits."},
            "expand": {"type": "string", "enum": ["none", "heuristic", "llm", "both"], "description": "Query expansion. heuristic (default): identifier joins of the question words go to the keyword side. llm: a small local model adds likely identifiers and a pseudo-code line (about +0.7 s, better recall for natural-language questions). none: the query as typed."} } },
        "outputSchema": { "type": "object", "required": ["hits"], "properties": { "hits": { "type": "array", "items": { "type": "object",
            "required": ["repo", "branch", "path", "start_line", "end_line", "score", "snippet"],
            "properties": { "repo": {"type": "string"}, "branch": {"type": "string"}, "path": {"type": "string"},
                "symbol": {"type": ["string", "null"]}, "kind": {"type": "string"}, "start_line": {"type": "integer"},
                "end_line": {"type": "integer"}, "score": {"type": "number"}, "snippet": {"type": "string"},
                "snippet_start": {"type": "integer"} } } }, "busy_repos": {"type": "array", "items": {"type": "string"}} } },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    })];
    let ro = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false });
    v.push(json!({"name": "read_file", "title": "Read file",
        "description": "Read a file (or a line range) from an indexed repo at a committed branch, with line numbers. Use it to expand a code_search or symbol_lookup hit (pass its repo, path and a start_line/end_line around the hit); prefer this over leaving the index. At most 400 lines per call: for longer files page with start_line/end_line (total_lines tells you the length). branch defaults to main, falling back to master.",
        "inputSchema": {"type": "object", "required": ["repo", "path"], "properties": {"repo": {"type": "string"},
            "path": {"type": "string", "description": "Repo-relative path, as returned in hits."},
            "branch": {"type": "string", "default": "main"},
            "start_line": {"type": "integer", "minimum": 1, "description": "1-based, inclusive. Default 1."},
            "end_line": {"type": "integer", "minimum": 1, "description": "1-based, inclusive. Default end of file (capped at start_line + 399)."}}},
        "outputSchema": {"type": "object", "required": ["repo", "path", "branch", "start_line", "end_line", "total_lines"], "properties": {
            "repo": {"type": "string"}, "path": {"type": "string"}, "branch": {"type": "string"}, "start_line": {"type": "integer"},
            "end_line": {"type": "integer"}, "total_lines": {"type": "integer"}, "truncated": {"type": "boolean"}}},
        "annotations": ro}));
    v.push(json!({"name": "list_repos", "title": "List repositories",
        "description": "List indexed repositories: branches with file counts, chunk/embedded/symbol counts and whether each is loaded for search. Use it to find valid repo and branch names for code_search, read_file and symbol_lookup.",
        "inputSchema": {"type": "object", "properties": {}},
        "outputSchema": {"type": "object", "required": ["repos"], "properties": {"repos": {"type": "array", "items": {"type": "object",
            "required": ["name"], "properties": {"name": {"type": "string"}, "branches": {"type": "array", "items": {"type": "object",
            "properties": {"name": {"type": "string"}, "files": {"type": "integer"}}}}, "chunks": {"type": "integer"},
            "embedded": {"type": "integer"}, "symbols": {"type": "integer"}, "loaded": {"type": "boolean"}}}}}},
        "annotations": ro}));
    v.push(json!({"name": "recall", "title": "Recall memory",
        "description": "Hybrid (FTS + dense, RRF) + rerank recall over agent memory; score = rerank x (0.7 + 0.2 importance + 0.1 recency). Returns citations.",
        "inputSchema": {"type": "object", "required": ["query"], "properties": {"query": {"type": "string"},
            "scope": {"type": "string", "enum": ["global", "project", "agent"]}, "k": {"type": "integer", "minimum": 1, "maximum": 20, "default": 8}}},
        "outputSchema": {"type": "object", "required": ["hits"], "properties": {"hits": {"type": "array", "items": {"type": "object",
            "required": ["id", "text", "type", "scope", "score"], "properties": {"id": {"type": "string"}, "text": {"type": "string"},
            "type": {"type": "string"}, "scope": {"type": "string"}, "project_id": {"type": ["string", "null"]},
            "source": {"type": ["string", "null"]}, "confidence": {"type": "number"}, "valid_from": {"type": "string"}, "score": {"type": "number"}}}}}},
        "annotations": ro}));
    v.push(json!({"name": "remember", "title": "Remember a fact",
        "description": "Store a durable fact. Deduplicates (cosine>=0.97 + rerank; 0.85-0.97 goes to review as a possible duplicate or conflict), low confidence (<0.6) or conflicting facts go to a review queue; optional supersedes=<id> replaces an older fact. Secrets are rejected.",
        "inputSchema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string", "maxLength": 4000},
            "type": {"type": "string", "enum": ["episodic", "semantic", "procedural"], "default": "semantic"},
            "scope": {"type": "string", "enum": ["global", "project", "agent"], "default": "agent"}, "project_id": {"type": "string"},
            "source": {"type": "string"}, "confidence": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.8},
            "importance": {"type": "number", "minimum": 0, "maximum": 1, "default": 0.5}, "supersedes": {"type": "string"}}},
        "outputSchema": {"type": "object", "required": ["status", "id"], "properties": {"status": {"type": "string", "enum": ["active", "review", "duplicate"]},
            "id": {"type": "string"}, "superseded": {"type": ["string", "null"]}, "conflicts_with": {"type": ["string", "null"]}}},
        "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }}));
    v.push(json!({"name": "symbol_lookup", "title": "Symbol lookup",
        "description": "Raw symbol graph rows (SCIP: rust-analyzer, scip-typescript), for exhaustive lists after explain_symbol: kind=def finds definitions by exact name (or full SCIP symbol), refs lists every reference (set group_by_file=true for \"which files use this\": per-file counts and first lines), callers/callees list every call site. Start with explain_symbol, which summarises all of this in one call; come here when you need the complete list. Indexed per repo at its default branch (may lag behind it).",
        "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"},
            "kind": {"type": "string", "enum": ["def", "refs", "callers", "callees"], "default": "def"},
            "repo": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
            "group_by_file": {"type": "boolean", "default": false, "description": "kind=refs only: return files (path, count, first 5 lines; most references first) plus total_references/total_files instead of one row per reference."}}},
        "outputSchema": {"type": "object", "required": ["kind", "definitions"], "properties": {"kind": {"type": "string"},
            "definitions": {"type": "array"}, "references": {"type": "array"}, "files": {"type": "array"},
            "total_references": {"type": "integer"}, "total_files": {"type": "integer"},
            "callers": {"type": "array"}, "callees": {"type": "array"}}},
        "annotations": ro}));
    v.push(explain_tool(&ro));
    v.push(json!({"name": "search_all", "title": "Search everything",
        "description": "One query over code, symbols and memory; per-source results fused by RRF. Memory is included only with memory:read. Every item carries its source and a citation.",
        "inputSchema": {"type": "object", "required": ["query"], "properties": {"query": {"type": "string"},
            "sources": {"type": "array", "items": {"type": "string", "enum": ["code", "symbol", "memory"]}},
            "k": {"type": "integer", "minimum": 1, "maximum": 30, "default": 10}}},
        "outputSchema": {"type": "object", "required": ["results"], "properties": {"results": {"type": "array", "items": {"type": "object",
            "required": ["source", "title", "score"], "properties": {"source": {"type": "string"}, "title": {"type": "string"},
            "snippet": {"type": "string"}, "ref": {"type": "string"}, "score": {"type": "number"}}}}, "errors": {"type": "object"}}},
        "annotations": ro}));
    v.push(json!({"name": "forget", "title": "Forget a memory",
        "description": "Tombstone a memory you own (text and vector deleted, audit row kept). Requires memory:forget.",
        "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}},
        "outputSchema": {"type": "object", "required": ["forgotten"], "properties": {"forgotten": {"type": "string"}}},
        "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }}));
    v
}

fn required_scope(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "code_search"
            | "symbol_lookup"
            | "explain_symbol"
            | "search_all"
            | "read_file"
            | "list_repos"
    ) {
        return Some("index:read");
    }
    MEM_TOOLS.iter().find(|t| t.0 == name).map(|t| t.1)
}

pub fn tools(ctx: &Ctx) -> Value {
    Value::Array(
        all_tools()
            .into_iter()
            .filter(|t| {
                required_scope(t["name"].as_str().unwrap_or("")).is_some_and(|s| ctx.has(s))
            })
            .collect(),
    )
}

fn call_memory(cfg: &Cfg, ctx: &Ctx, auth: &str, route: &str, a: &Value) -> Result<Value, String> {
    let mut req = ureq::post(&format!("{}/{route}", cfg.memoryd)).header("Authorization", auth);
    if let Some(r) = &ctx.run_id {
        req = req.header("X-Muninn-Run-Id", r);
    }
    match req
        .config()
        .http_status_as_error(false)
        .build()
        .send_json(a)
    {
        Ok(mut r) => {
            let st = r.status().as_u16();
            let body: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
            if st >= 400 {
                Err(body["error"]
                    .as_str()
                    .unwrap_or("memoryd error")
                    .to_string())
            } else {
                Ok(body)
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

fn get_json(url: &str) -> Result<Value, String> {
    let mut req = ureq::get(url);
    if let Ok(token) = std::env::var("MUNINN_INDEXD_TOKEN") {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    req.call()
        .map_err(|e| e.to_string())?
        .into_body()
        .read_json()
        .map_err(|e| e.to_string())
}

/// GET returning (status, body text); non-2xx statuses are returned, not errors.
fn get_text(url: &str) -> Result<(u16, String), String> {
    let mut req = ureq::get(url);
    if let Ok(token) = std::env::var("MUNINN_INDEXD_TOKEN") {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    let mut r = req
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    let st = r.status().as_u16();
    let body = r.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok((st, body))
}

pub const READ_MAX_LINES: usize = 400;

/// Numbers lines `start..=end` (1-based, clamped to the file, at most READ_MAX_LINES) as
/// `{n:>5} | line`. Returns (text, start, end, total, truncated).
pub fn number_lines(
    text: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> Result<(String, usize, usize, usize, bool), String> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let s = start.unwrap_or(1).max(1);
    if total == 0 {
        return Ok((String::new(), 1, 0, 0, false));
    }
    if s > total {
        return Err(format!(
            "start_line {s} is past the end of the file ({total} lines)"
        ));
    }
    let want = end.unwrap_or(total).min(total);
    if want < s {
        return Err(format!("end_line {want} is before start_line {s}"));
    }
    let e = want.min(s + READ_MAX_LINES - 1);
    let mut out = String::new();
    for (i, l) in lines[s - 1..e].iter().enumerate() {
        out += &format!("{:>5} | {l}\n", s + i);
    }
    Ok((out, s, e, total, e < want))
}

fn call_read_file(cfg: &Cfg, a: &Value) -> Result<(Value, String), String> {
    let repo = a["repo"].as_str().ok_or("repo is required")?;
    let path = a["path"].as_str().ok_or("path is required")?;
    let given = a["branch"].as_str().filter(|b| !b.is_empty());
    let line = |k: &str| a[k].as_u64().map(|n| n as usize);
    let fetch = |b: &str| {
        get_text(&format!(
            "{}/file?repo={}&branch={}&path={}",
            cfg.indexd,
            urlencoding::encode(repo),
            urlencoding::encode(b),
            urlencoding::encode(path)
        ))
    };
    let mut branch = given.unwrap_or("main").to_string();
    let (mut st, mut body) = fetch(&branch)?;
    if st == 404 && given.is_none() {
        (st, body) = fetch("master")?;
        branch = "master".into();
    }
    match st {
        200 => {}
        404 => {
            return Err(format!(
                "not found: {repo}:{path} at {}",
                given.unwrap_or("main or master")
            ));
        }
        _ => return Err(format!("indexd /file {st}: {}", clip(body.trim(), 200))),
    }
    let (text, s, e, total, truncated) = number_lines(&body, line("start_line"), line("end_line"))?;
    let mut out = format!("{repo}:{path}@{branch} lines {s}-{e} of {total}\n{text}");
    if truncated {
        out += &format!(
            "[truncated at {READ_MAX_LINES} lines; continue with start_line={}]\n",
            e + 1
        );
    }
    let sc = json!({"repo": repo, "path": path, "branch": branch, "start_line": s, "end_line": e,
        "total_lines": total, "truncated": truncated});
    Ok((sc, out))
}

fn call_symbol(cfg: &Cfg, a: &Value) -> Result<Value, String> {
    let name = a["name"].as_str().ok_or("name is required")?;
    let mut url = format!(
        "{}/symbol?name={}&kind={}",
        cfg.indexd,
        urlencoding::encode(name),
        a["kind"].as_str().unwrap_or("def")
    );
    if let Some(r) = a["repo"].as_str() {
        url += &format!("&repo={}", urlencoding::encode(r));
    }
    if let Some(l) = a["limit"].as_u64() {
        url += &format!("&limit={}", l.clamp(1, 200));
    }
    if a["group_by_file"].as_bool() == Some(true) {
        url += "&group_by_file=true";
    }
    let v = get_json(&url)?;
    match v["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

fn explain_tool(ro: &Value) -> Value {
    let side = json!({"type": "object", "properties": {"distinct": {"type": "integer"}, "call_sites": {"type": "integer"},
        "top": {"type": "array", "items": {"type": "object", "properties": {"symbol": {"type": "string"}, "name": {"type": "string"},
            "kind": {"type": ["string", "null"]}, "path": {"type": ["string", "null"]}, "start_line": {"type": ["integer", "null"]},
            "external": {"type": "boolean"}, "calls": {"type": "integer"}, "first_call": {"type": "object"}}}}}});
    let def = json!({"type": "object", "properties": {
                "repo": {"type": "string"}, "symbol": {"type": "string"}, "name": {"type": "string"}, "kind": {"type": "string"},
                "path": {"type": "string"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"},
                "doc": {"type": ["string", "null"]}, "code": {"type": "string"}, "code_start": {"type": "integer"},
                "code_end": {"type": "integer"}, "code_truncated": {"type": "boolean"}, "commit": {"type": ["string", "null"]},
                "stale": {"type": "boolean"},
                "references": {"type": "object", "properties": {"count": {"type": "integer"}, "file_count": {"type": "integer"},
                    "files": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"},
                        "count": {"type": "integer"}, "first_line": {"type": "integer"}}}}}},
                "callers": side, "callees": side,
                "next": {"type": "array", "items": {"type": "object", "properties": {"tool": {"type": "string"},
                    "why": {"type": "string"}, "args": {"type": "object"}}}}}});
    json!({"name": "explain_symbol", "title": "Explain symbol",
        "description": "Everything about a function, method, type or module in ONE call: its definition(s) with doc and code (up to 80 lines), how many references and which files use it most, and its top callers and callees grouped by symbol with counts. Use it first whenever you know (or have just found) a symbol name and want to understand it, judge the impact of changing it, or find who calls it; it replaces a def + refs + callers + callees + read_file sequence. Each definition has `next`: exact arguments for read_file / symbol_lookup when you need more (rest of a long body, every referencing file, all callers). name is a plain name (case-insensitive) or a full SCIP symbol from a previous result; if several definitions share the name, the first 5 are expanded (functions and types first) and the rest listed in other_definitions, so pass repo or a full symbol to pick one. source=scip has the graph; source=tree-sitter (repo without a SCIP index, e.g. Swift) has definitions and code only, so use code_search mode=lexical for usages. stale=true means the graph was indexed at an older commit: line numbers are that commit's and next.read_file pins it.",
        "inputSchema": {"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string", "description": "Symbol name (e.g. symbol_lookup, PgStore) or full SCIP symbol."},
            "repo": {"type": "string", "description": "Restrict to one repo (see list_repos)."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "Top callers and callees per definition."}}},
        "outputSchema": {"type": "object", "required": ["query", "source", "definitions"], "properties": {
            "query": {"type": "string"}, "source": {"type": "string", "enum": ["scip", "tree-sitter", "none"]},
            "note": {"type": "string"}, "total_definitions": {"type": "integer"}, "other_definitions": {"type": "array"},
            "definitions": {"type": "array", "items": def}}},
        "annotations": ro})
}

fn call_explain(cfg: &Cfg, a: &Value) -> Result<Value, String> {
    let name = a["name"].as_str().ok_or("name is required")?;
    let mut url = format!(
        "{}/symbol/explain?name={}",
        cfg.indexd,
        urlencoding::encode(name)
    );
    if let Some(r) = a["repo"].as_str() {
        url += &format!("&repo={}", urlencoding::encode(r));
    }
    if let Some(l) = a["limit"].as_u64() {
        url += &format!("&limit={}", l.clamp(1, 50));
    }
    let v = get_json(&url)?;
    match v["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

/// Plain-text view of an explain_symbol result: one block per definition with numbered code,
/// reference files, grouped callers/callees and the next calls to make.
pub fn render_explain(v: &Value) -> String {
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    let n = |x: &Value| x.as_i64().unwrap_or(0);
    let defs = v["definitions"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "explain_symbol {} [source: {}] {} definition(s), {} expanded\n",
        s(&v["query"]),
        s(&v["source"]),
        n(&v["total_definitions"]),
        defs.len()
    );
    if let Some(note) = v["note"].as_str() {
        out += &format!("note: {note}\n");
    }
    for d in &defs {
        out += &format!(
            "\n## {} {}  {}:{}:{}-{}\nsymbol: {}\n",
            s(&d["kind"]),
            s(&d["name"]),
            s(&d["repo"]),
            s(&d["path"]),
            n(&d["start_line"]),
            n(&d["end_line"]),
            s(&d["symbol"])
        );
        if let Some(c) = d["commit"].as_str() {
            let short: String = c.chars().take(10).collect();
            out += &format!("indexed at {short}");
            if d["stale"].as_bool() == Some(true) {
                out += " (stale: the branch has moved on; lines are the indexed commit's)";
            }
            out += "\n";
        }
        if let Some(doc) = d["doc"].as_str().filter(|x| !x.is_empty()) {
            out += &format!("doc: {}\n", clip(doc, 600));
        }
        if let Some(code) = d["code"].as_str() {
            let first = d["code_start"].as_u64().unwrap_or(1) as usize;
            for (i, l) in code.lines().enumerate() {
                out += &format!("{:>5} | {l}\n", first + i);
            }
            if d["code_truncated"].as_bool() == Some(true) {
                out += &format!("[code truncated at line {}]\n", n(&d["code_end"]));
            }
        }
        let r = &d["references"];
        if r.is_object() {
            let files: Vec<String> = r["files"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    format!(
                        "{} ({}, first L{})",
                        s(&f["path"]),
                        n(&f["count"]),
                        n(&f["first_line"])
                    )
                })
                .collect();
            out += &format!(
                "references: {} in {} file(s){}{}\n",
                n(&r["count"]),
                n(&r["file_count"]),
                if files.is_empty() { "" } else { ": " },
                files.join(", ")
            );
        }
        for side in ["callers", "callees"] {
            let g = &d[side];
            if !g.is_object() {
                continue;
            }
            out += &format!(
                "{side}: {} distinct, {} call site(s)\n",
                n(&g["distinct"]),
                n(&g["call_sites"])
            );
            for t in g["top"].as_array().into_iter().flatten() {
                let at = if t["external"].as_bool() == Some(true) {
                    "external".to_string()
                } else {
                    format!(
                        "{} {}:{}",
                        s(&t["kind"]),
                        s(&t["path"]),
                        n(&t["start_line"])
                    )
                };
                out += &format!(
                    "  {} [{at}] x{} (first call {}:{})\n",
                    s(&t["name"]),
                    n(&t["calls"]),
                    s(&t["first_call"]["path"]),
                    n(&t["first_call"]["line"])
                );
            }
        }
        for h in d["next"].as_array().into_iter().flatten() {
            out += &format!("next: {} {} ({})\n", s(&h["tool"]), h["args"], s(&h["why"]));
        }
    }
    let others = v["other_definitions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !others.is_empty() {
        out += "\nother definitions:\n";
        for o in &others {
            out += &format!(
                "  {} {} {}:{}:{}  symbol: {}\n",
                s(&o["kind"]),
                s(&o["name"]),
                s(&o["repo"]),
                s(&o["path"]),
                n(&o["start_line"]),
                s(&o["symbol"])
            );
        }
    }
    out
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Normalised (title, snippet, ref) rows per source, rank order.
type Rows = Vec<(String, String, String)>;

pub fn fuse(per_source: Vec<(&str, Rows)>, k: usize) -> Vec<Value> {
    let mut out: Vec<(f64, Value)> = Vec::new();
    for (src, rows) in per_source {
        for (rank, (title, snippet, r)) in rows.into_iter().enumerate() {
            out.push((
                1.0 / (60.0 + rank as f64 + 1.0),
                json!({"source": src, "title": title, "snippet": snippet, "ref": r}),
            ));
        }
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter()
        .take(k)
        .map(|(s, mut v)| {
            v["score"] = json!(s);
            v
        })
        .collect()
}

fn call_search_all(cfg: &Cfg, ctx: &Ctx, auth: &str, a: &Value) -> Result<Value, String> {
    let q = a["query"].as_str().ok_or("query is required")?;
    let k = a["k"].as_u64().unwrap_or(10).clamp(1, 30) as usize;
    let want: Vec<&str> = match a["sources"].as_array() {
        Some(l) if !l.is_empty() => l.iter().filter_map(|x| x.as_str()).collect(),
        _ => vec!["code", "symbol", "memory"],
    };
    let mem = ctx.has("memory:read");
    let mut errors = serde_json::Map::new();
    let results: Vec<(&str, Result<Rows, String>)> = std::thread::scope(|sc| {
        let mut hs = Vec::new();
        for src in ["code", "symbol", "memory"] {
            if !want.contains(&src) || (src == "memory" && !mem) {
                continue;
            }
            hs.push((
                src,
                sc.spawn(move || -> Result<Rows, String> {
                    match src {
                        "code" => {
                            let v = call_search(cfg, &json!({"query": q, "k": k}))?;
                            Ok(v["hits"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|h| {
                                    (
                                        format!(
                                            "{} {}",
                                            h["path"].as_str().unwrap_or(""),
                                            h["symbol"].as_str().unwrap_or("")
                                        )
                                        .trim()
                                        .to_string(),
                                        clip(h["snippet"].as_str().unwrap_or(""), 300),
                                        format!(
                                            "{}:{}#L{}-{}",
                                            h["repo"].as_str().unwrap_or(""),
                                            h["path"].as_str().unwrap_or(""),
                                            h["start_line"],
                                            h["end_line"]
                                        ),
                                    )
                                })
                                .collect())
                        }
                        "symbol" => {
                            let v = get_json(&format!(
                                "{}/symbol?q={}&limit={k}",
                                cfg.indexd,
                                urlencoding::encode(q)
                            ))?;
                            Ok(v["hits"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|h| {
                                    (
                                        format!(
                                            "{} {}",
                                            h["kind"].as_str().unwrap_or(""),
                                            h["name"].as_str().unwrap_or("")
                                        ),
                                        String::new(),
                                        format!(
                                            "{}:{}#L{}-{}",
                                            h["repo"].as_str().unwrap_or(""),
                                            h["path"].as_str().unwrap_or(""),
                                            h["start_line"],
                                            h["end_line"]
                                        ),
                                    )
                                })
                                .collect())
                        }
                        "memory" => {
                            let v = call_memory(
                                cfg,
                                ctx,
                                auth,
                                "recall",
                                &json!({"query": q, "k": k}),
                            )?;
                            Ok(v["hits"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|h| {
                                    (
                                        format!(
                                            "{} ({})",
                                            h["type"].as_str().unwrap_or(""),
                                            h["scope"].as_str().unwrap_or("")
                                        ),
                                        clip(h["text"].as_str().unwrap_or(""), 300),
                                        format!("memory:{}", h["id"].as_str().unwrap_or("")),
                                    )
                                })
                                .collect())
                        }
                        _ => Ok(Vec::new()),
                    }
                }),
            ));
        }
        hs.into_iter()
            .map(|(s, h)| (s, h.join().unwrap_or_else(|_| Err("panic".into()))))
            .collect()
    });
    let mut per = Vec::new();
    for (s, r) in results {
        match r {
            Ok(rows) => per.push((s, rows)),
            Err(e) => {
                errors.insert(s.into(), json!(e));
            }
        }
    }
    Ok(json!({"results": fuse(per, k), "errors": errors}))
}

fn call_search(cfg: &Cfg, a: &Value) -> Result<Value, String> {
    let q = a["query"].as_str().ok_or("query is required")?;
    let mut url = format!("{}/search?q={}", cfg.indexd, urlencoding::encode(q));
    for (k, p) in [
        ("repo", "repo"),
        ("branch", "branch"),
        ("mode", "mode"),
        ("path", "path"),
        ("kind", "kind"),
        ("expand", "expand"),
    ] {
        if let Some(v) = a[k].as_str() {
            url += &format!("&{p}={}", urlencoding::encode(v));
        }
    }
    if let Some(k) = a["k"].as_u64() {
        url += &format!("&k={}", k.clamp(1, 50));
    }
    if let Some(c) = a["context"].as_u64() {
        url += &format!("&context={}", c.min(50));
    }
    get_json(&url)
}

pub fn handle(cfg: &Cfg, ctx: &Ctx, auth: &str, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let ok = |r: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": r}));
    let err = |c: i64, m: &str| {
        Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": m}}))
    };
    id.as_ref()?; // notifications get no response body
    match req["method"].as_str().unwrap_or("") {
        "initialize" => ok(
            json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "muninn-mcp", "version": env!("CARGO_PKG_VERSION")}}),
        ),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools(ctx)})),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("");
            let Some(need) = required_scope(name) else {
                return err(-32602, "unknown tool");
            };
            if !ctx.has(need) {
                return ok(
                    json!({"content": [{"type": "text", "text": format!("forbidden: {need} required")}], "isError": true}),
                );
            }
            let args = &req["params"]["arguments"];
            let res = match name {
                "read_file" => call_read_file(cfg, args).map(|(v, t)| (v, Some(t))),
                "list_repos" => get_json(&format!("{}/repos", cfg.indexd)).map(|v| (v, None)),
                "code_search" => call_search(cfg, args).map(|v| (v, None)),
                "symbol_lookup" => call_symbol(cfg, args).map(|v| (v, None)),
                "explain_symbol" => call_explain(cfg, args).map(|v| {
                    let t = render_explain(&v);
                    (v, Some(t))
                }),
                "search_all" => call_search_all(cfg, ctx, auth, args).map(|v| (v, None)),
                _ => {
                    let route = MEM_TOOLS.iter().find(|t| t.0 == name).unwrap().2;
                    call_memory(cfg, ctx, auth, route, args).map(|v| (v, None))
                }
            };
            match res {
                Ok((s, text)) => ok(
                    json!({"content": [{"type": "text", "text": text.unwrap_or_else(|| s.to_string())}], "structuredContent": s, "isError": false}),
                ),
                Err(e) => ok(json!({"content": [{"type": "text", "text": e}], "isError": true})),
            }
        }
        _ => err(-32601, "method not found"),
    }
}

async fn mcp(
    State(cfg): State<Arc<Cfg>>,
    h: HeaderMap,
    Json(req): Json<Value>,
) -> impl IntoResponse {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let run = h
        .get("x-muninn-run-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let ctx = if !cfg.tokens.0.is_empty() {
        cfg.tokens.resolve(Some(&auth), run)
    } else if let Some(t) = &cfg.token {
        (auth == format!("Bearer {t}")).then(|| Ctx {
            actor: "legacy".into(),
            scopes: vec!["index:read".into()],
            projects: vec![],
            run_id: run,
        })
    } else {
        Some(Ctx {
            actor: "anonymous".into(),
            scopes: vec!["index:read".into()],
            projects: vec![],
            run_id: run,
        })
    };
    let Some(ctx) = ctx else {
        return (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer")],
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    };
    let c = cfg.clone();
    let out = tokio::task::spawn_blocking(move || handle(&c, &ctx, &auth, &req))
        .await
        .unwrap();
    match out {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

pub fn router(cfg: Arc<Cfg>) -> Router {
    Router::new().route("/mcp", post(mcp)).with_state(cfg)
}

#[cfg(test)]
mod tests {
    use super::{READ_MAX_LINES, fuse, number_lines};

    #[test]
    fn number_lines_ranges_and_cap() {
        let (t, s, e, n, tr) = number_lines("a\nb\nc\n", Some(2), None).unwrap();
        assert_eq!(
            (t.as_str(), s, e, n, tr),
            ("    2 | b\n    3 | c\n", 2, 3, 3, false)
        );
        let big: String = (1..=1000).map(|i| format!("l{i}\n")).collect();
        let (t, s, e, n, tr) = number_lines(&big, None, None).unwrap();
        assert_eq!((s, e, n, tr), (1, READ_MAX_LINES, 1000, true));
        assert_eq!(t.lines().count(), READ_MAX_LINES);
        assert!(number_lines("a\n", Some(5), None).is_err());
        assert!(number_lines("a\nb\nc\n", Some(3), Some(2)).is_err());
        assert_eq!(number_lines("", None, None).unwrap().3, 0);
        assert_eq!(number_lines("a\nb\n", Some(1), Some(99)).unwrap().2, 2);
    }

    #[test]
    fn index_tools_need_index_read() {
        let names: Vec<String> = super::all_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for t in [
            "code_search",
            "read_file",
            "list_repos",
            "symbol_lookup",
            "explain_symbol",
            "search_all",
        ] {
            assert!(names.iter().any(|n| n == t), "{t}");
            assert_eq!(super::required_scope(t), Some("index:read"));
        }
    }

    #[test]
    fn explain_renders_code_graph_and_next_calls() {
        let v = serde_json::json!({"query": "helper", "source": "scip", "total_definitions": 2,
            "note": "2 definitions match",
            "definitions": [{"repo": "r", "path": "src/a.rs", "symbol": "t m helper().", "name": "helper",
                "kind": "function", "start_line": 12, "end_line": 14, "doc": "Helps.", "commit": "abcdef1234567",
                "stale": true, "code": "fn helper() {\n}", "code_start": 12, "code_end": 13, "code_truncated": true,
                "references": {"count": 4, "file_count": 1, "files": [{"path": "src/a.rs", "count": 4, "first_line": 3}]},
                "callers": {"distinct": 1, "call_sites": 2, "top": [{"name": "main", "kind": "function", "path": "src/a.rs",
                    "start_line": 1, "external": false, "calls": 2, "first_call": {"path": "src/a.rs", "line": 3}}]},
                "callees": {"distinct": 1, "call_sites": 1, "top": [{"name": "add", "external": true, "calls": 1,
                    "first_call": {"path": "src/a.rs", "line": 13}}]},
                "next": [{"tool": "read_file", "why": "rest of the definition", "args": {"repo": "r", "path": "src/a.rs", "start_line": 14, "end_line": 14}}]}],
            "other_definitions": [{"kind": "method", "name": "helper", "repo": "r", "path": "src/b.rs", "start_line": 7, "symbol": "t m B#helper()."}]});
        let t = super::render_explain(&v);
        for want in [
            "explain_symbol helper [source: scip] 2 definition(s), 1 expanded",
            "note: 2 definitions match",
            "## function helper  r:src/a.rs:12-14",
            "indexed at abcdef1234 (stale",
            "doc: Helps.",
            "   12 | fn helper() {\n   13 | }\n[code truncated at line 13]",
            "references: 4 in 1 file(s): src/a.rs (4, first L3)",
            "callers: 1 distinct, 2 call site(s)\n  main [function src/a.rs:1] x2 (first call src/a.rs:3)",
            "  add [external] x1",
            "next: read_file {\"end_line\":14,",
            "  method helper r:src/b.rs:7  symbol: t m B#helper().",
        ] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
        let tool = super::all_tools()
            .into_iter()
            .find(|t| t["name"] == "explain_symbol")
            .unwrap();
        let item = &tool["outputSchema"]["properties"]["definitions"]["items"]["properties"];
        assert!(item["callers"]["properties"]["top"].is_object() && item["next"].is_object());
        let ts = serde_json::json!({"query": "q", "source": "none", "total_definitions": 0, "definitions": [], "note": "no definition"});
        assert!(super::render_explain(&ts).contains("note: no definition"));
    }

    #[test]
    fn fuse_interleaves_sources_by_rank() {
        let r = fuse(
            vec![
                (
                    "code",
                    vec![
                        ("c1".into(), "".into(), "r".into()),
                        ("c2".into(), "".into(), "r".into()),
                    ],
                ),
                ("memory", vec![("m1".into(), "".into(), "r".into())]),
            ],
            3,
        );
        let t: Vec<_> = r.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(t, ["c1", "m1", "c2"]);
    }
}
