//! muninn-watch: keeps the muninn live overlay in step with working trees, so uncommitted edits
//! are searchable within about a second. It polls `git status` (about 10 ms on a mid-size repo, honours
//! .gitignore) and posts a full snapshot of the edited files to indexd `/live` whenever the set or a
//! file's mtime/size changes, and every minute anyway so an indexd restart heals itself.
//! Env: MUNINN_WATCH="repo=/path/to/checkout,..." MUNINN_INDEXD_URL, MUNINN_INDEXD_TOKEN.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    process::Command,
    time::{Duration, Instant, SystemTime},
};

const MAX_BYTES: u64 = 1 << 20;
const POLL: Duration = Duration::from_secs(1);
const RESYNC: Duration = Duration::from_secs(60);

type Fingerprint = BTreeMap<String, Option<(SystemTime, u64)>>;

/// Paths git reports changed against HEAD: modified, added, untracked or deleted (renames split).
fn dirty(dir: &str) -> Option<Vec<String>> {
    let out = Command::new("git")
        .args([
            "-C",
            dir,
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout
            .split(|b| *b == 0)
            .filter(|r| r.len() > 3)
            .map(|r| String::from_utf8_lossy(&r[3..]).into_owned())
            .collect()
    })
}

fn fingerprint(dir: &str, paths: &[String]) -> Fingerprint {
    paths
        .iter()
        .map(|p| {
            let m = std::fs::metadata(format!("{dir}/{p}"))
                .ok()
                .filter(|m| m.is_file());
            (
                p.clone(),
                m.map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())),
            )
        })
        .collect()
}

/// path -> text, or null when deleted. Large and non-UTF-8 files are left out (and so reverted).
fn snapshot(dir: &str, fp: &Fingerprint) -> serde_json::Map<String, Value> {
    let mut files = serde_json::Map::new();
    for (p, meta) in fp {
        match meta {
            None => {
                files.insert(p.clone(), Value::Null);
            }
            Some((_, len)) if *len <= MAX_BYTES => {
                if let Ok(t) = std::fs::read_to_string(format!("{dir}/{p}")) {
                    files.insert(p.clone(), Value::String(t));
                }
            }
            Some(_) => {}
        }
    }
    files
}

fn post(url: &str, token: &str, body: &Value) -> Result<String, String> {
    ureq::post(&format!("{url}/live"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn main() {
    let url = std::env::var("MUNINN_INDEXD_URL").unwrap_or_else(|_| "http://127.0.0.1:7410".into());
    let token = std::env::var("MUNINN_INDEXD_TOKEN").expect("MUNINN_INDEXD_TOKEN");
    let repos: Vec<(String, String)> = std::env::var("MUNINN_WATCH")
        .expect("MUNINN_WATCH=repo=/path,...")
        .split(',')
        .filter_map(|s| {
            s.split_once('=')
                .map(|(r, d)| (r.trim().to_string(), d.trim().to_string()))
        })
        .collect();
    eprintln!("{}", json!({"event": "watching", "repos": repos}));
    let mut last: Vec<(Fingerprint, Option<Instant>)> =
        vec![(Fingerprint::new(), None); repos.len()];
    loop {
        for ((repo, dir), (prev, sent)) in repos.iter().zip(last.iter_mut()) {
            let Some(paths) = dirty(dir) else { continue };
            let fp = fingerprint(dir, &paths);
            if fp == *prev && sent.is_some_and(|t| t.elapsed() < RESYNC) {
                continue;
            }
            let body = json!({"repo": repo, "files": snapshot(dir, &fp), "full": true});
            match post(&url, &token, &body) {
                Ok(r) => {
                    if fp != *prev {
                        eprintln!(
                            "{}",
                            json!({"event": "live", "repo": repo, "dirty": fp.len(), "result": r})
                        );
                    }
                    *prev = fp;
                    *sent = Some(Instant::now());
                }
                Err(e) => eprintln!("{}", json!({"event": "error", "repo": repo, "error": e})),
            }
        }
        std::thread::sleep(POLL);
    }
}
