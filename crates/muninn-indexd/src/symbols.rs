//! SCIP symbol-graph indexing: check out a repo revision, run rust-analyzer / scip-typescript,
//! decode the SCIP output and replace the repo's defs/refs/calls rows.
use crate::App;
use axum::{
    Json,
    extract::{Query, State},
};
use muninn_core::scip;
use muninn_core::symbols::{CODE_MAX_LINES, HitSpan, clip_code, next_hints};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

fn running() -> &'static Mutex<HashSet<String>> {
    static R: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn work_dir(app: &App) -> PathBuf {
    std::env::var("MUNINN_SCIP_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| app.repos_dir.join("..").join("scip-work"))
}

fn run(dir: &Path, prog: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new("nice")
        .args(["-n", "19", prog])
        .args(args)
        .current_dir(dir)
        .env("CARGO_BUILD_JOBS", "4")
        .env(
            "RUSTUP_TOOLCHAIN",
            std::env::var("MUNINN_SCIP_TOOLCHAIN").unwrap_or_else(|_| "1.98.1".into()),
        )
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "{prog} failed: {}",
            err.lines()
                .find(|l| l.contains("error"))
                .or(err.lines().last())
                .unwrap_or("")
        ))
    }
}

/// Project roots (relative to checkout) for each indexer: the repo root plus direct subdirectories.
fn projects(root: &Path, marker: &str) -> Vec<String> {
    let mut v = Vec::new();
    if root.join(marker).exists() {
        v.push(String::new());
    }
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut subs: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
        subs.sort_by_key(|e| e.file_name());
        for e in subs {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.')
                && n != "node_modules"
                && n != "target"
                && e.path().join(marker).exists()
            {
                v.push(format!("{n}/"));
            }
        }
    }
    v
}

/// Sibling repos referenced by `path = "../Name/..."` dependencies in the checkout's Cargo manifests.
fn sibling_repos(root: &Path, repo: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = e.file_name().to_string_lossy().to_string();
                if depth < 3 && !n.starts_with('.') && n != "target" && n != "node_modules" {
                    stack.push((p, depth + 1));
                }
            } else if e.file_name() == "Cargo.toml"
                && let Ok(t) = std::fs::read_to_string(&p)
            {
                for part in t.split("path").skip(1) {
                    let Some(v) = part.trim_start().strip_prefix('=') else {
                        continue;
                    };
                    let Some(v) = v.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    let rel = v.split('"').next().unwrap_or("");
                    let mut base = d.clone();
                    let mut rest = rel;
                    while let Some(r) = rest.strip_prefix("../") {
                        base = base.parent().map(Path::to_path_buf).unwrap_or(base);
                        rest = r;
                    }
                    if rest.len() == rel.len() {
                        continue;
                    }
                    if base == root.parent().unwrap_or(root)
                        && let Some(name) = rest.split('/').next()
                        && !name.is_empty()
                        && name != repo
                        && !name.contains("..")
                        && !out.iter().any(|o| o == name)
                    {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}

pub fn index_repo(app: &App, repo: &str, branch: &str) -> Result<Value, String> {
    let store = app.store.as_ref().ok_or("no store")?;
    if !running().lock().unwrap().insert(repo.to_string()) {
        return Err("symbol indexing already running for this repo".into());
    }
    static SERIAL: Mutex<()> = Mutex::new(());
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let res = index_repo_inner(app, store, repo, branch);
    running().lock().unwrap().remove(repo);
    crate::release_free_memory(); // the decoded SCIP index can be hundreds of MB
    res
}

fn index_repo_inner(
    app: &App,
    store: &muninn_core::PgStore,
    repo: &str,
    branch: &str,
) -> Result<Value, String> {
    if repo.contains(['/', '\\']) || repo.starts_with('.') {
        return Err("bad repo name".into());
    }
    let src = app.repo_dir(repo);
    let sha = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["rev-parse", branch])
            .output()
            .map_err(|e| e.to_string())?
            .stdout,
    )
    .map_err(|e| e.to_string())?
    .trim()
    .to_string();
    if sha.len() < 7 {
        return Err(format!("unknown branch {branch}"));
    }
    let root = work_dir(app).join(repo);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let ar = Command::new("sh")
        .arg("-c")
        .arg("git -C \"$1\" archive \"$2\" | tar -x -C \"$3\"")
        .arg("sh")
        .arg(&src)
        .arg(&sha)
        .arg(&root)
        .status()
        .map_err(|e| e.to_string())?;
    if !ar.success() {
        return Err("git archive failed".into());
    }
    let sibs = sibling_repos(&root, repo);
    let mut extracted: Vec<PathBuf> = Vec::new();
    for sib in sibs {
        let dst = root.parent().unwrap().join(&sib);
        if dst.exists() {
            continue;
        }
        // Hosted mirror first, else a local checkout under MUNINN_SIBLING_SRC (default ~/Projects):
        // path dependencies often point at repos that are not hosted (an app pointing at a vendored library, ...).
        let mut sdir = app.repo_dir(&sib);
        if !sdir.exists() {
            let src = std::env::var("MUNINN_SIBLING_SRC")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Projects")
                });
            sdir = src.join(&sib);
            if !sdir.join(".git").exists() {
                continue;
            }
        }
        let _ = std::fs::create_dir_all(&dst);
        let _ = Command::new("sh")
            .arg("-c")
            .arg("git -C \"$1\" archive HEAD | tar -x -C \"$2\"")
            .arg("sh")
            .arg(&sdir)
            .arg(&dst)
            .status();
        extracted.push(dst);
    }
    let mut g = scip::Graph::default();
    let mut log: Vec<Value> = Vec::new();
    let mut jobs: Vec<(String, &str, String)> = Vec::new();
    for p in projects(&root, "Cargo.toml") {
        jobs.push((p, "rust-analyzer", "Cargo.toml".into()));
    }
    for p in projects(&root, "tsconfig.json") {
        jobs.push((p, "scip-typescript", "tsconfig.json".into()));
    }
    for (prefix, tool, _) in &jobs {
        let dir = root.join(prefix);
        let out = root.join(format!(
            ".scip-{}.scip",
            prefix.trim_end_matches('/').replace('/', "_")
        ));
        let out_s = out.to_string_lossy().to_string();
        let r = match *tool {
            "rust-analyzer" => run(&dir, "rust-analyzer", &["scip", ".", "--output", &out_s]),
            _ => run(&dir, "scip-typescript", &["index", "--output", &out_s]),
        };
        let entry = match r
            .and_then(|_| std::fs::read(&out).map_err(|e| e.to_string()))
            .and_then(|b| scip::decode(&b).map_err(|e| e.to_string()))
        {
            Ok(ix) => {
                let sg = scip::graph(&ix, prefix);
                let e = json!({"tool": tool, "project": prefix, "defs": sg.defs.len(), "refs": sg.refs.len(), "calls": sg.calls.len()});
                g.defs.extend(sg.defs);
                g.refs.extend(sg.refs);
                g.calls.extend(sg.calls);
                e
            }
            Err(e) => json!({"tool": tool, "project": prefix, "error": e}),
        };
        log.push(entry);
        let _ = std::fs::remove_file(&out);
    }
    let _ = std::fs::remove_dir_all(&root);
    for d in extracted {
        let _ = std::fs::remove_dir_all(d);
    }
    if g.defs.is_empty() && log.iter().all(|l| l.get("error").is_some()) && !log.is_empty() {
        return Err(format!("all indexers failed: {log:?}"));
    }
    store
        .replace_symbols(repo, &sha, &g)
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"repo": repo, "commit": sha, "defs": g.defs.len(), "refs": g.refs.len(), "calls": g.calls.len(), "projects": log}),
    )
}

#[derive(Deserialize)]
pub struct ReQ {
    repo: String,
    branch: Option<String>,
}

pub async fn reindex(State(app): State<Arc<App>>, Query(q): Query<ReQ>) -> Json<Value> {
    let repo = q.repo.clone();
    std::thread::spawn(move || {
        let r = index_repo(&app, &q.repo, q.branch.as_deref().unwrap_or("main"));
        eprintln!(
            "{{\"event\":\"symbols_reindex\",\"repo\":\"{}\",\"result\":{}}}",
            q.repo,
            match r {
                Ok(v) => v.to_string(),
                Err(e) => json!({"error": e}).to_string(),
            }
        );
    });
    Json(json!({"accepted": repo}))
}

#[derive(Deserialize)]
pub struct SQ {
    name: Option<String>,
    q: Option<String>,
    kind: Option<String>,
    repo: Option<String>,
    limit: Option<i64>,
    group_by_file: Option<bool>,
}

pub async fn lookup(State(app): State<Arc<App>>, Query(q): Query<SQ>) -> Json<Value> {
    tokio::task::spawn_blocking(move || {
        let Some(s) = &app.store else {
            return Json(json!({"error": "no store"}));
        };
        let limit = q.limit.unwrap_or(50).clamp(1, 200);
        if let Some(t) = &q.q {
            return Json(json!({"hits": s.symbol_search(t, limit)}));
        }
        let Some(name) = &q.name else {
            return Json(json!({"error": "name or q required"}));
        };
        match s.symbol_lookup_opts(
            name,
            q.kind.as_deref().unwrap_or("def"),
            q.repo.as_deref(),
            limit,
            q.group_by_file.unwrap_or(false),
        ) {
            Ok(v) => Json(v),
            Err(e) => Json(json!({"error": e})),
        }
    })
    .await
    .unwrap()
}

pub async fn stats(State(app): State<Arc<App>>) -> Json<Value> {
    tokio::task::spawn_blocking(move || {
        Json(
            app.store
                .as_ref()
                .map(|s| s.symbol_stats())
                .unwrap_or(json!([])),
        )
    })
    .await
    .unwrap()
}

#[derive(Deserialize)]
pub struct EQ {
    name: String,
    repo: Option<String>,
    limit: Option<i64>,
}

/// GET /symbol/explain?name=&repo=&limit=: definitions with code, reference summary and grouped
/// callers/callees in one response (see `PgStore::symbol_explain`). `limit` caps callers and
/// callees per definition (default 10, max 50).
pub async fn explain(State(app): State<Arc<App>>, Query(q): Query<EQ>) -> Json<Value> {
    tokio::task::spawn_blocking(move || {
        let Some(s) = &app.store else {
            return Json(json!({"error": "no store"}));
        };
        if q.name.trim().is_empty() {
            return Json(json!({"error": "name required"}));
        }
        let top = q.limit.unwrap_or(10).clamp(1, 50);
        let hosted = |r: &str| app.repo_dir(r).exists();
        match s.symbol_explain(q.name.trim(), q.repo.as_deref(), top, &hosted) {
            Ok(mut v) => {
                fill_code(&app, &mut v);
                Json(v)
            }
            Err(e) => Json(json!({"error": e})),
        }
    })
    .await
    .unwrap()
}

/// Adds each SCIP definition's code (read from git at the indexed commit, else the default
/// branch), a `stale` flag (indexed commit is not the branch head) and `next` hints.
fn fill_code(app: &App, v: &mut Value) {
    let mut heads: HashMap<String, Option<String>> = HashMap::new();
    let Some(defs) = v["definitions"].as_array_mut() else {
        return;
    };
    for d in defs.iter_mut() {
        if d["source"] == "scip" {
            let repo = d["repo"].as_str().unwrap_or("").to_string();
            let path = d["path"].as_str().unwrap_or("").to_string();
            let dir = app.repo_dir(&repo);
            let head = heads
                .entry(repo.clone())
                .or_insert_with(|| {
                    crate::git(&dir, &["rev-parse", "HEAD"])
                        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
                })
                .clone();
            let commit = d["commit"].as_str().map(String::from);
            let stale = commit.is_some() && head.is_some() && head != commit;
            d["stale"] = json!(stale);
            let (s, e) = (
                d["start_line"].as_u64().unwrap_or(1) as usize,
                d["end_line"].as_u64().unwrap_or(1) as usize,
            );
            let revs: Vec<&str> = commit.iter().map(String::as_str).chain(["HEAD"]).collect();
            let found = crate::safe_ref(&path)
                .then(|| {
                    revs.iter().find_map(|rev| {
                        crate::read_blobs(&dir, rev, std::slice::from_ref(&path))
                            .pop()
                            .flatten()
                            .map(|t| (rev.to_string(), t))
                    })
                })
                .flatten();
            if let Some((rev, text)) = found {
                if let Some((code, a, b, tr)) =
                    clip_code(&text, 1, s.max(1), e.max(s), CODE_MAX_LINES)
                {
                    d["code"] = json!(code);
                    d["code_start"] = json!(a);
                    d["code_end"] = json!(b);
                    d["code_truncated"] = json!(tr);
                }
                let at_commit = commit.as_deref() == Some(rev.as_str());
                d["code_rev"] = json!(if at_commit {
                    rev.clone()
                } else {
                    "HEAD".into()
                });
                if stale && at_commit {
                    // Line numbers are the indexed commit's: read_file there to match them.
                    d["branch"] = json!(rev);
                } else if stale {
                    d["code_note"] = json!(
                        "indexed commit not in the mirror; code is from the default branch and lines may have drifted"
                    );
                }
            }
        }
        d["next"] = json!(next_hints(d));
    }
}

#[derive(Deserialize)]
pub struct FH {
    hits: Vec<HitSpan>,
}

/// POST /symbols/for_hits {"hits": [{repo, path, start_line, end_line}]}: per hit (same
/// order) the best-covering SCIP definition with caller/callee counts, or null.
pub async fn for_hits(State(app): State<Arc<App>>, Json(b): Json<FH>) -> Json<Value> {
    if b.hits.len() > 200 {
        return Json(json!({"error": "at most 200 hits"}));
    }
    tokio::task::spawn_blocking(move || {
        let Some(s) = &app.store else {
            return Json(json!({"error": "no store"}));
        };
        match s.symbols_for_hits(&b.hits) {
            Ok(v) => Json(json!({"hits": v})),
            Err(e) => Json(json!({"error": e})),
        }
    })
    .await
    .unwrap()
}
