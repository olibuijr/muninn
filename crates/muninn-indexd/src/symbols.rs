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
    process::{Command, Stdio},
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

/// Where an indexer binary comes from: an explicit setting, then (rust-analyzer only) the pinned
/// toolchain via `rustup which`, then PATH. None: the tool is not installed.
pub fn resolve_tool(
    setting: Option<String>,
    rustup: impl FnOnce() -> Option<PathBuf>,
    on_path: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    setting
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(rustup)
        .or_else(on_path)
}

/// First executable file called `name` in PATH.
fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn toolchain() -> String {
    std::env::var("MUNINN_SCIP_TOOLCHAIN").unwrap_or_else(|_| "1.98.1".into())
}

/// rust-analyzer: MUNINN_RUST_ANALYZER, else `rustup which --toolchain T rust-analyzer` (the
/// pinned toolchain's binary; rust-analyzer is often not on PATH), else PATH. Same idea as muninn-scip.
fn rust_analyzer() -> Option<PathBuf> {
    resolve_tool(
        std::env::var("MUNINN_RUST_ANALYZER").ok(),
        || {
            // A systemd user service usually lacks ~/.cargo/bin on PATH, where rustup lives.
            let cargo_home = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
            let rustup = which("rustup").or_else(|| {
                cargo_home
                    .map(|c| c.join("bin/rustup"))
                    .filter(|p| p.is_file())
            })?;
            let o = Command::new(rustup)
                .args(["which", "--toolchain", &toolchain(), "rust-analyzer"])
                .stderr(Stdio::null())
                .output()
                .ok()?;
            let p = PathBuf::from(String::from_utf8(o.stdout).ok()?.trim());
            (o.status.success() && p.is_file()).then_some(p)
        },
        || which("rust-analyzer"),
    )
}

/// scip-typescript: MUNINN_SCIP_TYPESCRIPT, else PATH.
fn scip_typescript() -> Option<PathBuf> {
    resolve_tool(
        std::env::var("MUNINN_SCIP_TYPESCRIPT").ok(),
        || None,
        || which("scip-typescript"),
    )
}

/// Repos whose symbols the muninn-scip timer builds (MUNINN_SCIP_REPOS, space or comma separated);
/// indexd leaves them alone because they are too big for its cgroup.
pub fn scip_timer_repos(list: &str) -> Vec<&str> {
    list.split([' ', ',', '\t', '\n'])
        .filter(|s| !s.is_empty())
        .collect()
}

/// Memory limits of the transient scope each indexer runs in.
pub struct Scope {
    pub high: String,
    pub max: String,
}

/// A transient systemd scope per indexer run, so an OOM kill hits that scope and never indexd
/// (whose own cgroup is 1 GB). Used when `systemd-run --user --scope` works here; MUNINN_SCIP_SCOPE=0
/// turns it off. Limits: MUNINN_INDEXD_SCIP_MEM_HIGH / MUNINN_INDEXD_SCIP_MEM_MAX (default 1536M / 2G).
fn scope() -> Option<&'static Scope> {
    static S: OnceLock<Option<Scope>> = OnceLock::new();
    S.get_or_init(|| {
        if std::env::var("MUNINN_SCIP_SCOPE").as_deref() == Ok("0") {
            return None;
        }
        let ok = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let env = |k: &str, d: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| d.into())
        };
        ok.then(|| Scope {
            high: env("MUNINN_INDEXD_SCIP_MEM_HIGH", "1536M"),
            max: env("MUNINN_INDEXD_SCIP_MEM_MAX", "2G"),
        })
    })
    .as_ref()
}

/// `nice -n 19 prog args`, wrapped in `systemd-run --user --scope` with memory limits when `scope`
/// is given. The unit name is made of `unit` with characters systemd rejects replaced.
pub fn tool_command(scope: Option<&Scope>, unit: &str, prog: &Path, args: &[&str]) -> Command {
    let mut c = match scope {
        Some(s) => {
            let unit: String = unit
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-_.:".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let mut c = Command::new("systemd-run");
            c.args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=background.slice",
            ])
            .arg(format!("--unit={unit}"))
            .arg("-p")
            .arg(format!("MemoryHigh={}", s.high))
            .arg("-p")
            .arg(format!("MemoryMax={}", s.max))
            .args(["-p", "MemorySwapMax=1G", "-p", "CPUWeight=20", "nice"]);
            c
        }
        None => Command::new("nice"),
    };
    c.args(["-n", "19"]).arg(prog).args(args);
    c
}

fn run(dir: &Path, unit: &str, prog: &Path, args: &[&str]) -> Result<(), String> {
    let mut c = tool_command(scope(), unit, prog, args);
    // The toolchain's own cargo/rustc first, so rust-analyzer works without ~/.cargo/bin on PATH.
    if let Some(bin) = prog.parent().filter(|b| !b.as_os_str().is_empty()) {
        let path = std::env::var("PATH").unwrap_or_default();
        c.env("PATH", format!("{}:{path}", bin.display()));
    }
    let name = prog.file_name().unwrap_or_default().to_string_lossy();
    let out = c
        .current_dir(dir)
        .env("CARGO_BUILD_JOBS", "4")
        .env("RUSTUP_TOOLCHAIN", toolchain())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "{name} failed ({}): {}",
            out.status,
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

/// Logs `symbols_skipped` (info) the first time a repo is skipped for a given reason in this
/// process, so a missing tool does not log on every push.
pub fn log_skip_once(repo: &str, reason: &str) {
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert((repo.to_string(), reason.to_string()));
    if first {
        eprintln!(
            "{}",
            json!({"event": "symbols_skipped", "level": "info", "repo": repo, "reason": reason})
        );
    }
}

/// Builds the repo's symbol graph in-daemon. Returns `{"skipped": reason}` without touching the
/// stored symbols when the repo belongs to the muninn-scip timer or no indexer it needs is installed.
pub fn index_repo(app: &App, repo: &str, branch: &str) -> Result<Value, String> {
    let store = app.store.as_ref().ok_or("no store")?;
    let timer = std::env::var("MUNINN_SCIP_REPOS").unwrap_or_default();
    if scip_timer_repos(&timer).contains(&repo) {
        return Ok(
            json!({"repo": repo, "skipped": "built by the muninn-scip timer (MUNINN_SCIP_REPOS)"}),
        );
    }
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
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (mut ra, mut ts) = (None, None);
    for (prefix, tool, _) in &jobs {
        let prog = match *tool {
            "rust-analyzer" => ra.get_or_insert_with(rust_analyzer).clone(),
            _ => ts.get_or_insert_with(scip_typescript).clone(),
        };
        let Some(prog) = prog else {
            let hint = if *tool == "rust-analyzer" {
                "MUNINN_RUST_ANALYZER"
            } else {
                "MUNINN_SCIP_TYPESCRIPT"
            };
            log.push(json!({"tool": tool, "project": prefix,
                "skipped": format!("{tool} not found (set {hint} or put it on PATH)")}));
            continue;
        };
        let unit = format!(
            "muninn-indexd-scip-{repo}-{}-{}",
            std::process::id(),
            RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let dir = root.join(prefix);
        let out = root.join(format!(
            ".scip-{}.scip",
            prefix.trim_end_matches('/').replace('/', "_")
        ));
        let out_s = out.to_string_lossy().to_string();
        let r = match *tool {
            "rust-analyzer" => run(&dir, &unit, &prog, &["scip", ".", "--output", &out_s]),
            _ => run(&dir, &unit, &prog, &["index", "--output", &out_s]),
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
    let ran = |l: &&Value| l.get("error").is_none() && l.get("skipped").is_none();
    if !log.is_empty() && !log.iter().any(|l| ran(&l)) {
        if log.iter().any(|l| l.get("error").is_some()) {
            return Err(format!("all indexers failed: {log:?}"));
        }
        let reasons: Vec<&str> = log.iter().filter_map(|l| l["skipped"].as_str()).collect();
        return Ok(json!({"repo": repo, "skipped": reasons.join("; ")}));
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

#[cfg(test)]
mod tests {
    use super::{Scope, resolve_tool, scip_timer_repos, tool_command};
    use std::path::{Path, PathBuf};

    #[test]
    fn tool_resolution_order() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            resolve_tool(Some("/set".into()), || p("/rustup"), || p("/path")),
            p("/set")
        );
        assert_eq!(
            resolve_tool(Some(String::new()), || p("/rustup"), || p("/path")),
            p("/rustup")
        );
        assert_eq!(resolve_tool(None, || None, || p("/path")), p("/path"));
        assert_eq!(resolve_tool(None, || None, || None), None);
    }

    #[test]
    fn timer_repos_are_space_or_comma_separated() {
        assert_eq!(
            scip_timer_repos(" big, other  third\n"),
            ["big", "other", "third"]
        );
        assert!(scip_timer_repos("").is_empty());
        assert!(!scip_timer_repos("bigger").contains(&"big"));
    }

    #[test]
    fn indexer_runs_in_its_own_scope_when_available() {
        let args = |c: &std::process::Command| -> Vec<String> {
            std::iter::once(c.get_program())
                .chain(c.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let ra = Path::new("/tc/bin/rust-analyzer");
        let plain = tool_command(None, "u", ra, &["scip", "."]);
        assert_eq!(
            args(&plain),
            ["nice", "-n", "19", "/tc/bin/rust-analyzer", "scip", "."]
        );
        let s = Scope {
            high: "1536M".into(),
            max: "2G".into(),
        };
        let scoped = args(&tool_command(
            Some(&s),
            "muninn-indexd-scip-a b/c",
            ra,
            &["scip"],
        ));
        assert_eq!(scoped[0], "systemd-run");
        for want in [
            "--scope",
            "--unit=muninn-indexd-scip-a_b_c",
            "MemoryHigh=1536M",
            "MemoryMax=2G",
        ] {
            assert!(scoped.iter().any(|a| a == want), "{want} in {scoped:?}");
        }
        assert_eq!(
            scoped[scoped.len() - 5..],
            ["nice", "-n", "19", "/tc/bin/rust-analyzer", "scip"]
        );
    }
}
