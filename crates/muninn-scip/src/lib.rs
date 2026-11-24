//! Out-of-daemon SCIP symbol indexing. indexd runs rust-analyzer inside its own 1 GB cgroup, where a
//! run over a large Rust workspace is OOM-killed; this crate runs the same pipeline as a separate process (a systemd
//! scope with its own limits) and writes the same rows through `PgStore::replace_symbols`.
//!
//! The checkout, project discovery, sibling-repo extraction and path prefixes mirror
//! `muninn-indexd/src/symbols.rs` (`index_repo`) so the stored graph has the same shape.
use muninn_core::scip::{self, Graph};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

pub const DEFAULT_RUST_ANALYZER: &str =
    ".rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin/rust-analyzer";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn env_path(key: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default)
}

/// Settings for `run`, all from the environment.
pub struct Config {
    /// Hosted mirrors (`MUNINN_REPOS_DIR`, default `~/.local/share/muninn/repos`).
    pub repos_dir: PathBuf,
    /// Scratch checkouts (`MUNINN_SCIP_WORK`, default `~/.cache/muninn-scip`).
    pub work_dir: PathBuf,
    /// `MUNINN_RUST_ANALYZER`, default the pinned toolchain's binary (rust-analyzer is not on PATH on purpose).
    pub rust_analyzer: PathBuf,
    /// `MUNINN_SCIP_TYPESCRIPT`, default `scip-typescript` from PATH (only used for tsconfig projects).
    pub scip_typescript: String,
    /// `MUNINN_SCIP_TOOLCHAIN`, default 1.98.1 (same as indexd).
    pub toolchain: String,
    /// `MUNINN_SCIP_JOBS`, default 4: cargo jobs for build scripts and proc macros.
    pub jobs: String,
    /// `MUNINN_SCIP_TARGET`: persistent cargo target dir to reuse between runs; default a
    /// throwaway dir inside the checkout.
    pub target_dir: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            repos_dir: env_path("MUNINN_REPOS_DIR", || {
                home().join(".local/share/muninn/repos")
            }),
            work_dir: env_path("MUNINN_SCIP_WORK", || home().join(".cache/muninn-scip")),
            rust_analyzer: env_path("MUNINN_RUST_ANALYZER", || {
                home().join(DEFAULT_RUST_ANALYZER)
            }),
            scip_typescript: std::env::var("MUNINN_SCIP_TYPESCRIPT")
                .unwrap_or_else(|_| "scip-typescript".into()),
            toolchain: std::env::var("MUNINN_SCIP_TOOLCHAIN").unwrap_or_else(|_| "1.98.1".into()),
            jobs: std::env::var("MUNINN_SCIP_JOBS").unwrap_or_else(|_| "4".into()),
            target_dir: std::env::var("MUNINN_SCIP_TARGET")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Same lookup as indexd's `App::repo_dir`: the bare mirror `<name>.git`, else `<name>`.
    pub fn repo_dir(&self, name: &str) -> PathBuf {
        let bare = self.repos_dir.join(format!("{name}.git"));
        if bare.exists() {
            bare
        } else {
            self.repos_dir.join(name)
        }
    }
}

/// `""` stays the repo root; anything else ends in exactly one `/`, as `projects()` returns it.
pub fn normalize_prefix(p: &str) -> String {
    let p = p.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

/// Project roots (relative to checkout) for each indexer: the repo root plus direct subdirectories.
/// Copied from indexd so discovery stays identical.
pub fn projects(root: &Path, marker: &str) -> Vec<String> {
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
/// Copied from indexd.
pub fn sibling_repos(root: &Path, repo: &str) -> Vec<String> {
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

/// Decode one SCIP file and turn it into graph rows with `prefix` before every path.
/// The raw bytes and the decoded index are dropped before returning; only the rows stay.
pub fn graph_of_file(file: &Path, prefix: &str) -> Result<Graph, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let ix = scip::decode(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    drop(bytes);
    Ok(scip::graph(&ix, &normalize_prefix(prefix)))
}

/// Append one project's rows, as `index_repo` does.
pub fn merge(g: &mut Graph, sg: Graph) {
    g.defs.extend(sg.defs);
    g.refs.extend(sg.refs);
    g.calls.extend(sg.calls);
}

pub fn counts(g: &Graph) -> Value {
    json!({"defs": g.defs.len(), "refs": g.refs.len(), "calls": g.calls.len()})
}

/// Peak memory of this process's cgroup (the systemd scope it runs in), from cgroup v2 `memory.peak`.
/// Covers rust-analyzer too, since children share the scope.
pub fn cgroup_peak_bytes() -> Option<u64> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::"))?;
    let p = Path::new("/sys/fs/cgroup")
        .join(rel.trim_start_matches('/'))
        .join("memory.peak");
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn sh(script: &str, args: &[&Path]) -> Result<(), String> {
    let st = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("`{script}` failed: {st}"))
    }
}

fn log(v: Value) {
    eprintln!("{v}");
}

/// Run one indexer at `nice -n 19`; stdout is discarded, stderr kept for the error message.
fn index_one(
    cfg: &Config,
    dir: &Path,
    tool: &str,
    out: &Path,
    target: &Path,
) -> Result<(), String> {
    let out_s = out.to_string_lossy().to_string();
    let mut c = Command::new("nice");
    c.arg("-n").arg("19");
    if tool == "rust-analyzer" {
        c.arg(&cfg.rust_analyzer)
            .args(["scip", ".", "--output", &out_s]);
        // The toolchain's own cargo/rustc first, so this works without ~/.cargo/bin on PATH.
        if let Some(bin) = cfg.rust_analyzer.parent() {
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{}:{path}", bin.display()));
        }
    } else {
        c.arg(&cfg.scip_typescript)
            .args(["index", "--output", &out_s]);
    }
    let o = c
        .current_dir(dir)
        .env("CARGO_BUILD_JOBS", &cfg.jobs)
        .env("RUSTUP_TOOLCHAIN", &cfg.toolchain)
        // Never build into a caller's shared target dir.
        .env("CARGO_TARGET_DIR", target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{tool}: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    Err(format!(
        "{tool} failed ({}): {}",
        o.status,
        err.lines()
            .find(|l| l.contains("error"))
            .or(err.lines().last())
            .unwrap_or("")
    ))
}

pub struct RunResult {
    pub sha: String,
    pub graph: Graph,
    pub projects: Vec<Value>,
}

/// Check out `branch` of the mirror, index every Cargo (and tsconfig) project and return the merged graph.
pub fn run_repo(cfg: &Config, repo: &str, branch: &str) -> Result<RunResult, String> {
    if repo.is_empty() || repo.contains(['/', '\\']) || repo.starts_with('.') {
        return Err("bad repo name".into());
    }
    let src = cfg.repo_dir(repo);
    if !src.exists() {
        return Err(format!("no mirror {}", src.display()));
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(&src)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{branch}^{{commit}}"))
        .output()
        .map_err(|e| e.to_string())?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.len() < 7 {
        return Err(format!("unknown branch {branch}"));
    }

    // <work>/muninn-scip-<repo>/<repo>: sibling repos land next to the checkout (as `../Name`
    // dependencies expect) and the whole run dir goes away at the end. A lock file keeps two
    // runs of the same repo from deleting each other's checkout.
    std::fs::create_dir_all(&cfg.work_dir).map_err(|e| e.to_string())?;
    let lock = std::fs::File::create(cfg.work_dir.join(format!("muninn-scip-{repo}.lock")))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| format!("another muninn-scip run for {repo} is active"))?;
    let run_dir = cfg.work_dir.join(format!("muninn-scip-{repo}"));
    let _ = std::fs::remove_dir_all(&run_dir);
    let root = run_dir.join(repo);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let res = index_checkout(cfg, repo, &src, &sha, &run_dir, &root);
    let _ = std::fs::remove_dir_all(&run_dir);
    res.map(|(graph, projects)| RunResult {
        sha,
        graph,
        projects,
    })
}

fn index_checkout(
    cfg: &Config,
    repo: &str,
    src: &Path,
    sha: &str,
    run_dir: &Path,
    root: &Path,
) -> Result<(Graph, Vec<Value>), String> {
    let t = Instant::now();
    sh(
        "git -C \"$1\" archive \"$2\" | tar -x -C \"$3\"",
        &[src, Path::new(sha), root],
    )?;
    for sib in sibling_repos(root, repo) {
        let dst = run_dir.join(&sib);
        if dst.exists() {
            continue;
        }
        // Hosted mirror first, else a local checkout under MUNINN_SIBLING_SRC (default ~/Projects).
        let mut sdir = cfg.repo_dir(&sib);
        if !sdir.exists() {
            sdir = env_path("MUNINN_SIBLING_SRC", || home().join("Projects")).join(&sib);
            if !sdir.join(".git").exists() {
                continue;
            }
        }
        let _ = std::fs::create_dir_all(&dst);
        let r = sh(
            "git -C \"$1\" archive HEAD | tar -x -C \"$2\"",
            &[&sdir, &dst],
        );
        log(json!({"event": "scip_sibling", "repo": repo, "sibling": sib, "ok": r.is_ok()}));
    }
    log(
        json!({"event": "scip_checkout", "repo": repo, "commit": sha, "secs": t.elapsed().as_secs()}),
    );

    let target = cfg
        .target_dir
        .clone()
        .unwrap_or_else(|| run_dir.join("target"));
    let mut jobs: Vec<(String, &str)> = Vec::new();
    for p in projects(root, "Cargo.toml") {
        jobs.push((p, "rust-analyzer"));
    }
    for p in projects(root, "tsconfig.json") {
        jobs.push((p, "scip-typescript"));
    }
    let mut g = Graph::default();
    let mut log_v: Vec<Value> = Vec::new();
    for (prefix, tool) in &jobs {
        let t = Instant::now();
        let dir = root.join(prefix);
        let out = run_dir.join(format!(
            ".scip-{}.scip",
            prefix.trim_end_matches('/').replace('/', "_")
        ));
        let r = index_one(cfg, &dir, tool, &out, &target);
        let index_secs = t.elapsed().as_secs();
        let entry = match r.and_then(|_| graph_of_file(&out, prefix)) {
            Ok(sg) => {
                let mut e = json!({"tool": tool, "project": prefix, "index_secs": index_secs});
                e.as_object_mut()
                    .unwrap()
                    .extend(counts(&sg).as_object().unwrap().clone());
                merge(&mut g, sg);
                e
            }
            Err(e) => {
                json!({"tool": tool, "project": prefix, "index_secs": index_secs, "error": e})
            }
        };
        log(json!({"event": "scip_project", "repo": repo, "result": entry}));
        log_v.push(entry);
        let _ = std::fs::remove_file(&out);
    }
    if jobs.is_empty() {
        return Err("no Cargo.toml or tsconfig.json projects".into());
    }
    if g.defs.is_empty() && log_v.iter().all(|l| l.get("error").is_some()) {
        return Err(format!("all indexers failed: {}", Value::from(log_v)));
    }
    Ok((g, log_v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_match_projects_shape() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("springboard"), "springboard/");
        assert_eq!(normalize_prefix("springboard/"), "springboard/");
        assert_eq!(normalize_prefix("a/b//"), "a/b/");
    }

    #[test]
    fn projects_lists_root_and_direct_subdirs() {
        let d = std::env::temp_dir().join(format!("muninn-scip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for sub in ["", "b", "a", "target", ".hidden", "c/deep"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
            std::fs::write(d.join(sub).join("Cargo.toml"), "").unwrap();
        }
        assert_eq!(projects(&d, "Cargo.toml"), vec!["", "a/", "b/"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn merge_appends_rows() {
        let mut g = Graph::default();
        let mut sg = Graph::default();
        sg.refs.push(scip::Ref {
            symbol: "s".into(),
            path: "p".into(),
            line: 1,
            col: 2,
        });
        merge(&mut g, sg);
        assert_eq!(counts(&g), json!({"defs": 0, "refs": 1, "calls": 0}));
    }
}
