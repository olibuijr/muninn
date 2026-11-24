//! muninn-scip: SCIP symbol indexing outside indexd's memory-limited cgroup.
//!
//!   muninn-scip ingest --repo R --sha S [--prefix P] FILE.scip [--prefix P2 FILE2.scip ...]
//!   muninn-scip run --repo R [--branch main]
//!
//! Both write through `PgStore::replace_symbols` on `$MUNINN_PG_URL` and print counts as JSON.
use muninn_core::{PgStore, scip::Graph};
use muninn_scip::{Config, cgroup_peak_bytes, counts, graph_of_file, merge, run_repo};
use serde_json::{Value, json};
use std::{path::PathBuf, process::ExitCode, time::Instant};

const USAGE: &str = "usage:
  muninn-scip ingest --repo R --sha S [--prefix P] FILE.scip... (a --prefix applies to the files after it)
  muninn-scip run --repo R [--branch main]
env: MUNINN_PG_URL (required), MUNINN_REPOS_DIR, MUNINN_SCIP_WORK, MUNINN_RUST_ANALYZER,
     MUNINN_SCIP_TOOLCHAIN, MUNINN_SCIP_JOBS, MUNINN_SCIP_TARGET, MUNINN_SCIP_TYPESCRIPT";

fn store(g: &Graph, repo: &str, sha: &str) -> Result<(), String> {
    let url = std::env::var("MUNINN_PG_URL").map_err(|_| "MUNINN_PG_URL is not set")?;
    let s = PgStore::connect(&url).map_err(|e| format!("postgres: {e}"))?;
    s.replace_symbols(repo, sha, g)
        .map_err(|e| format!("replace_symbols: {e}"))
}

fn finish(mut v: Value, t: Instant) -> Value {
    let o = v.as_object_mut().unwrap();
    o.insert("secs".into(), json!(t.elapsed().as_secs()));
    if let Some(p) = cgroup_peak_bytes() {
        o.insert("cgroup_peak_mb".into(), json!(p / (1 << 20)));
    }
    v
}

fn ingest(args: &[String]) -> Result<Value, String> {
    let t = Instant::now();
    let (mut repo, mut sha, mut prefix) = (None, None, String::new());
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--repo" => repo = it.next().cloned(),
            "--sha" => sha = it.next().cloned(),
            "--prefix" => prefix = it.next().cloned().ok_or("--prefix needs a value")?,
            f if !f.starts_with("--") => files.push((prefix.clone(), PathBuf::from(f))),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let repo = repo.ok_or("--repo is required")?;
    let sha = sha.ok_or("--sha is required")?;
    if files.is_empty() {
        return Err("no SCIP files given".into());
    }
    let mut g = Graph::default();
    let mut projects = Vec::new();
    for (p, f) in &files {
        let sg = graph_of_file(f, p)?;
        let mut e = json!({"file": f, "project": muninn_scip::normalize_prefix(p)});
        e.as_object_mut()
            .unwrap()
            .extend(counts(&sg).as_object().unwrap().clone());
        projects.push(e);
        merge(&mut g, sg);
    }
    store(&g, &repo, &sha)?;
    let mut v = json!({"repo": repo, "commit": sha, "projects": projects});
    v.as_object_mut()
        .unwrap()
        .extend(counts(&g).as_object().unwrap().clone());
    Ok(finish(v, t))
}

fn run(args: &[String]) -> Result<Value, String> {
    let t = Instant::now();
    let (mut repo, mut branch) = (None, "main".to_string());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--repo" => repo = it.next().cloned(),
            "--branch" => branch = it.next().cloned().ok_or("--branch needs a value")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let repo = repo.ok_or("--repo is required")?;
    if std::env::var("MUNINN_PG_URL").is_err() {
        return Err("MUNINN_PG_URL is not set".into());
    }
    let cfg = Config::from_env();
    if !cfg.rust_analyzer.exists() {
        return Err(format!(
            "no rust-analyzer at {}",
            cfg.rust_analyzer.display()
        ));
    }
    let r = run_repo(&cfg, &repo, &branch)?;
    store(&r.graph, &repo, &r.sha)?;
    let mut v = json!({"repo": repo, "branch": branch, "commit": r.sha, "projects": r.projects});
    v.as_object_mut()
        .unwrap()
        .extend(counts(&r.graph).as_object().unwrap().clone());
    Ok(finish(v, t))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("ingest") => ingest(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match res {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "{}",
                json!({"event": "scip_failed", "args": args, "error": e})
            );
            ExitCode::FAILURE
        }
    }
}
