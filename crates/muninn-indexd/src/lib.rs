use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use hmac::{Hmac, KeyInit, Mac};
use muninn_core::{Change, Embedder, Index, Mode, PgStore, Reranker};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_FILE: usize = 512 * 1024;
/// Ingest collects prepared files until this many chunk texts are pending, then embeds them in one
/// `embed()` call (the embedder still splits it into EMBED_BATCH requests).
const INGEST_EMBED_TEXTS: usize = muninn_core::EMBED_BATCH * 4;
const EXCLUDED: &[&str] = &[
    ".env",
    ".pem",
    ".key",
    "id_rsa",
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    ".lock",
];

pub mod graph;
pub mod symbols;

type GraphEntry = (String, Arc<Vec<u8>>);

pub struct App {
    pub graph_cache: Mutex<HashMap<String, GraphEntry>>,
    pub repos_dir: PathBuf,
    pub secret: Option<String>,
    pub indexes: Mutex<HashMap<String, Arc<Mutex<Index>>>>,
    pub gate: Arc<tokio::sync::Semaphore>,
    pub queued: AtomicU64,
    pub active: Mutex<Option<(String, u64)>>,
    pub load_done: AtomicU64,
    pub embed_done: AtomicU64,
    pub load_total: AtomicU64,
    pub files_done: AtomicU64,
    pub files_total: AtomicU64,
    pub lag_ms: AtomicU64,
    pub pushes: AtomicU64,
    pub store: Option<Arc<PgStore>>,
    pub emb: Option<Box<dyn Embedder>>,
    pub rr: Option<Box<dyn Reranker>>,
    /// Normalized /search query -> response; cleared on every index mutation.
    pub qcache: Mutex<QueryCache>,
}

const QC_CAP: usize = 256;
const QC_TTL: Duration = Duration::from_secs(600);

/// Small TTL cache of /search responses. `generation` bumps on every clear, so a search that
/// started before a mutation cannot store its (stale) result after the clear.
#[derive(Default)]
pub struct QueryCache {
    map: HashMap<String, (Instant, Value)>,
    generation: u64,
    pub hits: u64,
}

impl QueryCache {
    pub fn get(&mut self, key: &str, now: Instant) -> Option<Value> {
        match self.map.get(key) {
            Some((t, v)) if now.saturating_duration_since(*t) < QC_TTL => {
                self.hits += 1;
                Some(v.clone())
            }
            Some(_) => {
                self.map.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Stores `v` unless the cache was cleared since `generation` was read. When full, expired
    /// entries go first, then the oldest one.
    pub fn put(&mut self, key: String, v: Value, now: Instant, generation: u64) {
        if generation != self.generation {
            return;
        }
        if self.map.len() >= QC_CAP && !self.map.contains_key(&key) {
            self.map
                .retain(|_, (t, _)| now.saturating_duration_since(*t) < QC_TTL);
            if self.map.len() >= QC_CAP
                && let Some(old) = self
                    .map
                    .iter()
                    .min_by_key(|(_, (t, _))| *t)
                    .map(|(k, _)| k.clone())
            {
                self.map.remove(&old);
            }
        }
        self.map.insert(key, (now, v));
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.generation += 1;
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Glob over a repo path: `*` and `?` stay within a segment, `**` crosses segments (`a/**/b` also
/// matches `a/b`). A pattern without `/` also matches the file name alone, so `*.rs` finds every
/// Rust file (gitignore-style).
pub fn glob_match(pat: &str, path: &str) -> bool {
    fn m(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') if p.get(1) == Some(&'*') => {
                let rest = &p[2..];
                if rest.first() == Some(&'/') && m(&rest[1..], s) {
                    return true;
                }
                (0..=s.len()).any(|i| m(rest, &s[i..]))
            }
            Some('*') => {
                for i in 0..=s.len() {
                    if m(&p[1..], &s[i..]) {
                        return true;
                    }
                    if i < s.len() && s[i] == '/' {
                        break;
                    }
                }
                false
            }
            Some('?') => s.first().is_some_and(|c| *c != '/') && m(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && m(&p[1..], &s[1..]),
        }
    }
    if pat.len() > 512 {
        return false;
    }
    let p: Vec<char> = pat.chars().collect();
    let full: Vec<char> = path.chars().collect();
    if m(&p, &full) {
        return true;
    }
    !pat.contains('/') && {
        let base: Vec<char> = path.rsplit('/').next().unwrap_or(path).chars().collect();
        m(&p, &base)
    }
}

pub fn excluded(path: &str) -> bool {
    EXCLUDED.iter().any(|e| path.ends_with(e))
        || path
            .split('/')
            .any(|p| p == ".git" || p == "node_modules" || p == "target")
}

fn git(dir: &PathBuf, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    o.status.success().then_some(o.stdout)
}

/// Reads `paths` at `rev` through one `git cat-file --batch` process (one process per push instead
/// of one `git show` per file). None for a path that is missing, not a blob, larger than MAX_FILE or
/// not UTF-8.
fn read_blobs(dir: &PathBuf, rev: &str, paths: &[String]) -> Vec<Option<String>> {
    use std::io::{BufReader, BufWriter, Write};
    use std::process::Stdio;
    // A newline would split the request line; such paths cannot be asked for.
    let ask: Vec<usize> = (0..paths.len())
        .filter(|&i| !paths[i].contains('\n'))
        .collect();
    let mut out: Vec<Option<String>> = vec![None; paths.len()];
    if ask.is_empty() {
        return out;
    }
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return out;
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return out;
    };
    std::thread::scope(|s| {
        // Write requests on their own thread: git answers while it reads, so writing everything
        // first would deadlock once both pipe buffers are full.
        s.spawn(|| {
            let mut w = BufWriter::new(stdin);
            for &i in &ask {
                if writeln!(w, "{rev}:{}", paths[i]).is_err() {
                    return;
                }
            }
            let _ = w.flush();
        });
        let mut r = BufReader::new(stdout);
        for &i in &ask {
            match read_batch_entry(&mut r) {
                Some(b) => out[i] = b,
                None => {
                    // Broken stream: stop git so the writer cannot block; the rest stays None.
                    let _ = child.kill();
                    break;
                }
            }
        }
    });
    let _ = child.wait();
    out
}

/// One `git cat-file --batch` response: `<sha> <type> <size>\n<bytes>\n`, or `<name> missing\n`
/// (also `ambiguous`). Outer None when the stream ended or broke.
fn read_batch_entry(r: &mut impl std::io::BufRead) -> Option<Option<String>> {
    use std::io::Read;
    let mut header = String::new();
    if r.read_line(&mut header).ok()? == 0 {
        return None;
    }
    let mut f = header.trim_end_matches('\n').rsplitn(3, ' ');
    let (size, typ) = (f.next(), f.next());
    // `<name> missing`: the last field is a word, never a size.
    let Some(size) = size.and_then(|s| s.parse::<usize>().ok()) else {
        return Some(None);
    };
    if typ != Some("blob") || size > MAX_FILE {
        std::io::copy(&mut r.take(size as u64 + 1), &mut std::io::sink()).ok()?;
        return Some(None);
    }
    let mut buf = vec![0u8; size + 1];
    r.read_exact(&mut buf).ok()?;
    buf.pop(); // trailing newline
    Some(String::from_utf8(buf).ok())
}

/// Changes between two revs; `before` None or all-zero means full tree at `after`.
pub fn git_changes(dir: &PathBuf, before: Option<&str>, after: &str) -> Vec<Change> {
    let zero = |s: &str| s.chars().all(|c| c == '0');
    let mut out = Vec::new();
    let mut upserts: Vec<String> = Vec::new();
    match before.filter(|b| !zero(b)) {
        None => {
            let ls = git(dir, &["ls-tree", "-r", "--name-only", after]).unwrap_or_default();
            upserts.extend(String::from_utf8_lossy(&ls).lines().map(String::from));
        }
        Some(b) => {
            let d =
                git(dir, &["diff", "--name-status", "--no-renames", b, after]).unwrap_or_default();
            for l in String::from_utf8_lossy(&d).lines() {
                if let Some((st, p)) = l.split_once('\t') {
                    if st == "D" {
                        out.push(Change::Delete { path: p.into() });
                    } else {
                        upserts.push(p.into());
                    }
                }
            }
        }
    }
    let ignore = read_ignore(dir, after);
    let paths: Vec<String> = upserts
        .into_iter()
        .filter(|p| !excluded(p) && !ignore_match(&ignore, p))
        .collect();
    let blobs = read_blobs(dir, after, &paths);
    for (p, blob) in paths.into_iter().zip(blobs) {
        match blob {
            Some(text) => out.push(Change::Upsert { path: p, text }),
            None => out.push(Change::Delete { path: p }), // binary/oversize: ensure not indexed
        }
    }
    out
}

/// Per-repo `.muninn-ignore` (gitignore-style) read from the committed tree: one glob per line,
/// `#` comments and blank lines skipped. Lets a repo keep generated/data files out of the index.
/// `MUNINN_IGNORE_FILES` (comma-separated) names other files to read as well, e.g. an ignore file
/// your repos already carry for another tool.
fn read_ignore(dir: &PathBuf, rev: &str) -> Vec<String> {
    let names = std::env::var("MUNINN_IGNORE_FILES").unwrap_or_else(|_| ".muninn-ignore".into());
    names
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .filter_map(|n| git(dir, &["show", &format!("{rev}:{n}")]))
        .flat_map(|b| {
            String::from_utf8_lossy(&b)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Minimal matcher: `dir/` matches that dir and everything under it, `*.ext` matches a suffix,
/// anything else matches the exact path or that path as a directory prefix.
fn ignore_match(pats: &[String], path: &str) -> bool {
    pats.iter().any(|pat| {
        if let Some(d) = pat.strip_suffix('/') {
            path == d || path.starts_with(&format!("{d}/"))
        } else if let Some(ext) = pat.strip_prefix('*') {
            path.ends_with(ext)
        } else {
            path == pat || path.starts_with(&format!("{pat}/"))
        }
    })
}

impl App {
    pub fn new(repos_dir: PathBuf, secret: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            repos_dir,
            secret,
            graph_cache: Default::default(),
            indexes: Default::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
            queued: AtomicU64::new(0),
            active: Mutex::new(None),
            load_done: AtomicU64::new(0),
            embed_done: AtomicU64::new(0),
            load_total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            lag_ms: AtomicU64::new(0),
            pushes: AtomicU64::new(0),
            store: None,
            emb: None,
            rr: None,
            qcache: Default::default(),
        })
    }

    /// Attach Postgres storage and load previously indexed repos from it.
    pub fn with_backends(
        repos_dir: PathBuf,
        secret: Option<String>,
        store: Option<Arc<PgStore>>,
        emb: Option<Box<dyn Embedder>>,
        rr: Option<Box<dyn Reranker>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            repos_dir,
            secret,
            graph_cache: Default::default(),
            indexes: Default::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
            queued: AtomicU64::new(0),
            active: Mutex::new(None),
            load_done: AtomicU64::new(0),
            embed_done: AtomicU64::new(0),
            load_total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            lag_ms: AtomicU64::new(0),
            pushes: AtomicU64::new(0),
            store,
            emb,
            rr,
            qcache: Default::default(),
        })
    }

    /// Load previously indexed repos from Postgres one at a time, off the request path.
    pub fn load_repos(&self) {
        let Some(s) = &self.store else { return };
        // The store only holds enrolled repos (webhook-indexed), so load all of them; a prefix filter
        // here silently dropped enrolled repos from search after every restart.
        let repos: Vec<String> = s.repos();
        self.load_total.store(repos.len() as u64, Ordering::Relaxed);
        for r in repos {
            if !self.indexes.lock().unwrap().contains_key(&r) {
                let ix = Arc::new(Mutex::new(Index::with_store(&r, s.clone())));
                self.indexes.lock().unwrap().entry(r).or_insert(ix);
                self.clear_query_cache();
            }
            self.load_done.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Enrolled mirrors (webhook hook installed) that hold no indexed files: the store was reset or a
    /// push was lost. Sorted.
    pub fn missing_repos(&self) -> Vec<String> {
        let loaded: std::collections::HashSet<String> = self
            .store
            .as_ref()
            .map(|s| s.repos())
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut v: Vec<String> = std::fs::read_dir(&self.repos_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("hooks/reference-transaction").exists())
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".git")
                    .map(String::from)
            })
            .filter(|r| !loaded.contains(r) && index_allowed(r))
            .collect();
        v.sort();
        v
    }

    /// Re-indexes every missing enrolled repo at its HEAD branch, one at a time (startup self-heal).
    pub fn heal_missing(&self) {
        for repo in self.missing_repos() {
            let dir = self.repo_dir(&repo);
            let git = |args: &[&str]| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            };
            let (Some(head), Some(sha)) = (
                git(&["symbolic-ref", "--short", "-q", "HEAD"]),
                git(&["rev-parse", "-q", "--verify", "HEAD"]),
            ) else {
                continue;
            };
            let n = self.index_push(&repo, &head, None, &sha);
            eprintln!(
                "{}",
                json!({"event": "healed", "repo": repo, "branch": head, "files": n})
            );
        }
    }

    /// Low-priority fill of chunks that were stored without an embedding.
    pub fn embed_missing(&self) {
        let (Some(s), Some(e)) = (&self.store, &self.emb) else {
            return;
        };
        loop {
            let rows = s.missing_emb(64);
            if rows.is_empty() {
                break;
            }
            let mut ok = 0;
            for b in rows.chunks(muninn_core::EMBED_BATCH) {
                let texts: Vec<String> = b.iter().map(|r| r.2.clone()).collect();
                let mut vs = e.embed(&texts);
                if vs.iter().any(|v| v.len() != muninn_core::pg::DIM) {
                    vs = texts
                        .iter()
                        .map(|t| e.embed(std::slice::from_ref(t)).pop().unwrap_or_default())
                        .collect();
                }
                for (r, v) in b.iter().zip(vs) {
                    if v.len() == muninn_core::pg::DIM {
                        s.set_emb(&r.0, &r.1, &v);
                        ok += 1;
                    }
                }
            }
            self.embed_done.fetch_add(ok, Ordering::Relaxed);
            if ok == 0 {
                break;
            }
            self.clear_query_cache();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Chunks still waiting for a current-version embedding (0 once the backfill is done).
    pub fn embed_pending(&self) -> i64 {
        self.store.as_ref().map_or(0, |s| s.missing_emb_count())
    }

    /// Drops every cached /search response; call after anything that changes search results.
    pub fn clear_query_cache(&self) {
        self.qcache.lock().unwrap().clear();
    }

    pub fn repo_dir(&self, name: &str) -> PathBuf {
        let bare = self.repos_dir.join(format!("{name}.git"));
        if bare.exists() {
            bare
        } else {
            self.repos_dir.join(name)
        }
    }

    pub fn index_push(&self, repo: &str, branch: &str, before: Option<&str>, after: &str) -> usize {
        if !index_allowed(repo) {
            eprintln!(
                "{}",
                json!({"event": "skipped", "repo": repo, "reason": "not in MUNINN_INDEX_ONLY"})
            );
            return 0;
        }
        let changes = git_changes(&self.repo_dir(repo), before, after);
        let n = changes.len();
        self.files_total.store(n as u64, Ordering::Relaxed);
        self.files_done.store(0, Ordering::Relaxed);
        *self.active.lock().unwrap() = Some((repo.into(), now_ms()));
        let ix = self
            .indexes
            .lock()
            .unwrap()
            .entry(repo.into())
            .or_insert_with(|| {
                Arc::new(Mutex::new(match &self.store {
                    Some(s) => Index::with_store(repo, s.clone()),
                    None => Index::new(repo),
                }))
            })
            .clone();
        // Lock per file and embed outside the lock, so searches and live edits interleave with ingest.
        // Prepared files wait until INGEST_EMBED_TEXTS texts are pending, then embed in one call.
        let mut pending: Vec<(muninn_core::Prepared, Vec<String>)> = Vec::new();
        let mut pending_texts = 0;
        for ch in changes {
            match ch {
                Change::Delete { path } => {
                    ix.lock().unwrap().delete(branch, &path);
                    self.files_done.fetch_add(1, Ordering::Relaxed);
                }
                Change::Upsert { path, text } => {
                    let prepared = ix.lock().unwrap().prepare(branch, &path, &text);
                    match prepared {
                        Some(p) => {
                            let texts = p.texts();
                            pending_texts += texts.len();
                            pending.push((p, texts));
                            if pending_texts >= INGEST_EMBED_TEXTS {
                                self.embed_commit(&ix, &mut pending);
                                pending_texts = 0;
                            }
                        }
                        None => {
                            self.files_done.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        self.embed_commit(&ix, &mut pending);
        ix.lock().unwrap().finish_memory();
        if let Some(s) = &self.store {
            s.gc(repo);
        }
        *self.active.lock().unwrap() = None;
        self.clear_query_cache();
        release_free_memory();
        n
    }

    /// Embeds the texts of all `pending` files in one call (outside the index lock), then commits
    /// each file under its own lock and counts it done.
    fn embed_commit(
        &self,
        ix: &Mutex<Index>,
        pending: &mut Vec<(muninn_core::Prepared, Vec<String>)>,
    ) {
        if pending.is_empty() {
            return;
        }
        let counts: Vec<usize> = pending.iter().map(|(_, t)| t.len()).collect();
        let all: Vec<String> = pending
            .iter_mut()
            .flat_map(|(_, t)| std::mem::take(t))
            .collect();
        let mut vecs = self.emb.as_deref().map(|e| {
            if all.is_empty() {
                Vec::new()
            } else {
                e.embed(&all)
            }
            .into_iter()
        });
        for ((p, _), n) in pending.drain(..).zip(counts) {
            let v = vecs.as_mut().map(|it| it.by_ref().take(n).collect());
            ix.lock().unwrap().commit(p, v);
            self.files_done.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Applies a working-tree snapshot to the repo's live overlay. `files` maps path -> text, or null
    /// for a deleted file. A file equal to its committed version leaves the overlay; with `full`,
    /// overlay paths missing from `files` are reverted too. Returns (upserted, deleted, reverted).
    pub fn live_update(
        &self,
        repo: &str,
        files: Vec<(String, Option<String>)>,
        full: bool,
    ) -> (usize, usize, usize) {
        let Some(ix) = self.indexes.lock().unwrap().get(repo).cloned() else {
            return (0, 0, 0);
        };
        let (mut up, mut del, mut rev) = (0, 0, 0);
        let sent: std::collections::HashSet<String> = files.iter().map(|f| f.0.clone()).collect();
        for (path, text) in files {
            match text {
                None => {
                    let mut i = ix.lock().unwrap();
                    if i.base_has(&path, None) {
                        i.live_delete(&path);
                        del += 1;
                    } else {
                        i.live_revert(&path);
                    }
                }
                Some(text) => {
                    let sha = muninn_core::chunk::sha_hex(text.as_bytes());
                    let prepared = {
                        let mut i = ix.lock().unwrap();
                        if i.base_has(&path, Some(&sha)) {
                            i.live_revert(&path);
                            rev += 1;
                            continue;
                        }
                        i.prepare(muninn_core::LIVE, &path, &text)
                    };
                    if let Some(p) = prepared {
                        let vecs = self.emb.as_deref().map(|e| e.embed(&p.texts()));
                        ix.lock().unwrap().commit(p, vecs);
                        up += 1;
                    }
                }
            }
        }
        if full {
            let mut i = ix.lock().unwrap();
            for (path, _) in i.live_paths() {
                if !sent.contains(&path) {
                    i.live_revert(&path);
                    rev += 1;
                }
            }
        }
        self.clear_query_cache();
        (up, del, rev)
    }
}

/// MUNINN_INDEX_ONLY (comma-separated repo names) limits indexing to those repos; unset = all enrolled.
/// Returns free heap pages to the kernel. glibc keeps memory freed after a repo load or an ingest
/// (Postgres result rows, chunk text, embedding batches) resident until a trim; that was about
/// 80 MB after startup alone with 20 repos.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only walks glibc's own heaps; it takes no pointers from us.
        unsafe {
            malloc_trim(0);
        }
    }
}

fn index_allowed(repo: &str) -> bool {
    std::env::var("MUNINN_INDEX_ONLY").map_or(true, |v| v.split(',').any(|r| r.trim() == repo))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn verify(secret: &str, body: &[u8], sig: &str) -> bool {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(body);
    hex::decode(sig.trim_start_matches("sha256=")).is_ok_and(|s| m.verify_slice(&s).is_ok())
}

#[derive(Deserialize)]
struct LiveReq {
    repo: String,
    files: std::collections::BTreeMap<String, Option<String>>,
    #[serde(default)]
    full: bool,
}

/// POST /live: working-tree snapshot from muninn-watch (see App::live_update).
async fn live(State(app): State<Arc<App>>, Json(r): Json<LiveReq>) -> Json<Value> {
    let res = tokio::task::spawn_blocking(move || {
        app.live_update(&r.repo, r.files.into_iter().collect(), r.full)
    })
    .await;
    match res {
        Ok((up, del, rev)) => Json(json!({"upserted": up, "deleted": del, "reverted": rev})),
        Err(_) => Json(json!({"error": "live update failed"})),
    }
}

async fn webhook(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let t0 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    if let Some(sec) = &app.secret {
        let sig = h
            .get("x-gitea-signature")
            .or_else(|| h.get("x-hub-signature-256"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify(sec, &body, sig) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "bad signature"})),
            );
        }
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad json"}))),
    };
    let (Some(r), Some(after), Some(repo)) = (
        v["ref"].as_str(),
        v["after"].as_str(),
        v["repository"]["name"].as_str(),
    ) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing ref/after/repository.name"})),
        );
    };
    let branch = r.trim_start_matches("refs/heads/").to_string();
    let before = v["before"]
        .as_str()
        .filter(|b| !b.is_empty() && !b.chars().all(|c| c == '0'))
        .map(String::from);
    if !push_args_ok(repo, &branch, before.as_deref(), after) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "bad repository.name, ref, before or after"})),
        );
    }
    let (repo, a2) = (repo.to_string(), app.clone());
    let after = after.to_string();
    let Ok(permit) = app.gate.clone().acquire_owned().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "closed"})),
        );
    };
    let n = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let n = muninn_core::with_lane(muninn_core::Lane::Ingest, || {
            a2.index_push(&repo, &branch, before.as_deref(), &after)
        });
        let done = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        a2.lag_ms.store(done - t0, Ordering::Relaxed);
        a2.pushes.fetch_add(1, Ordering::Relaxed);
        if (branch == "main" || branch == "master")
            && std::env::var("MUNINN_SCIP").as_deref() != Ok("0")
        {
            let (a3, r3, b3) = (a2.clone(), repo.clone(), branch.clone());
            std::thread::spawn(move || {
                if let Err(e) = symbols::index_repo(&a3, &r3, &b3) {
                    eprintln!(
                        "{{\"event\":\"symbols_reindex_failed\",\"repo\":\"{r3}\",\"error\":{}}}",
                        json!(e)
                    );
                }
            });
        }
        n
    })
    .await
    .unwrap_or(0);
    (StatusCode::OK, Json(json!({"changes": n})))
}

#[derive(Deserialize)]
struct Q {
    q: String,
    repo: Option<String>,
    branch: Option<String>,
    mode: Option<Mode>,
    k: Option<usize>,
    /// Glob on the hit path (see `glob_match`).
    path: Option<String>,
    /// Exact chunk kind, e.g. `function_item` or `window`.
    kind: Option<String>,
    /// 0..=50 lines around the chunk; replaces `snippet` with the chunk's full lines from git.
    context: Option<usize>,
    /// Query expansion: `none`, `heuristic` (default, or `MUNINN_EXPAND`), `llm`, `both`.
    /// Expansion terms go to BM25 only; an LLM HyDE line is added to the embedded text.
    expand: Option<String>,
}

/// The LLM expander, built once from the environment on first use (`MUNINN_CHAT_URL`,
/// `MUNINN_CHAT_MODEL`, `MUNINN_EXPAND_TIMEOUT_MS`).
fn llm_expander() -> &'static muninn_core::expand::HttpChatExpander {
    static E: std::sync::OnceLock<muninn_core::expand::HttpChatExpander> =
        std::sync::OnceLock::new();
    E.get_or_init(muninn_core::expand::HttpChatExpander::from_env)
}

/// Expansion mode for a request: the `expand` parameter, else `MUNINN_EXPAND`, else heuristic
/// for lexical searches and none otherwise. On 126 real task-style queries (2026-10-31) the
/// heuristic identifier joins lifted lexical recall@10 by 0.055 but cost hybrid 0.015, because
/// the joined terms pull BM25 towards near-miss identifiers that then displace dense hits.
fn expand_mode(q: &Q, search_mode: Mode) -> muninn_core::ExpandMode {
    q.expand
        .as_deref()
        .or(std::env::var("MUNINN_EXPAND").ok().as_deref())
        .and_then(|m| m.parse().ok())
        .unwrap_or(if search_mode == Mode::Lexical {
            muninn_core::ExpandMode::Heuristic
        } else {
            muninn_core::ExpandMode::None
        })
}

/// Expands `q` for `mode`. Returns the expansion, the BM25 query and the text to embed. With an
/// LLM expansion the identifiers go to every stage (BM25, embedder, and the caller reranks with
/// the same expanded query): that is what measured +0.06 recall@10 on real queries, while
/// identifiers on the BM25 side alone did nothing. Heuristic terms go to BM25 only.
fn expand_query(
    q: &str,
    mode: muninn_core::ExpandMode,
    search_mode: Mode,
) -> (muninn_core::Expansion, String, String) {
    use muninn_core::{ExpandMode, Expander};
    let e = match mode {
        ExpandMode::None => muninn_core::Expansion::default(),
        ExpandMode::Heuristic => {
            if search_mode == Mode::Semantic {
                muninn_core::Expansion::default()
            } else {
                muninn_core::heuristic_expand(q)
            }
        }
        ExpandMode::Llm => llm_expander().expand(q),
        ExpandMode::Both => llm_expander()
            .expand(q)
            .merge(muninn_core::heuristic_expand(q)),
    };
    let lexical = if e.is_empty() {
        q.to_string()
    } else {
        muninn_core::lexical_query(q, &e)
    };
    let dense = match mode {
        ExpandMode::Llm | ExpandMode::Both => muninn_core::dense_text(&lexical, &e),
        _ => q.to_string(),
    };
    (e, lexical, dense)
}

async fn search(State(app): State<Arc<App>>, Query(q): Query<Q>) -> Json<Value> {
    let h = tokio::task::spawn_blocking(move || {
        muninn_core::with_lane(muninn_core::Lane::Query, || search_blocking(&app, q))
    });
    match tokio::time::timeout(std::time::Duration::from_secs(15), h).await {
        Ok(Ok(j)) => j,
        Ok(Err(_)) => Json(json!({"hits": [], "error": "search failed"})),
        Err(_) => Json(json!({"hits": [], "error": "timeout"})),
    }
}

/// Characters of chunk text after the `path symbol` header line in a rerank document.
const RERANK_DOC_CHARS: usize = 1500;

/// Characters of chunk text per rerank document: `MUNINN_RERANK_CHARS`, default
/// `RERANK_DOC_CHARS`. Measured on 126 real queries (2026-10-31): 1500 chars gives hybrid
/// recall@10 0.730, 600 chars 0.698; 20 vs 30 documents makes no difference, so the window
/// (`MUNINN_RERANK_N`) defaults to 20. Rerank is ~60% of a hybrid search's latency.
fn rerank_doc_chars() -> usize {
    std::env::var("MUNINN_RERANK_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RERANK_DOC_CHARS)
}

/// Reranker input for a hit: a `path symbol` header line, then the start of the chunk text
/// (the snippet when the text is unavailable). Shorter than the reranker's 1500-char clip, which
/// cost up to 1.5 s for 30 documents; the header carries the context the clip used to.
fn rerank_doc(h: &muninn_core::Hit) -> String {
    let body = if h.text.is_empty() {
        &h.snippet
    } else {
        &h.text
    };
    let head = match h.symbol.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{} {s}", h.path),
        None => h.path.clone(),
    };
    let body: String = body.chars().take(rerank_doc_chars()).collect();
    format!("{head}\n{body}")
}

/// Whether reranking can change the answer. Not for a single candidate, and not when the top
/// hit is one whose repo's lexical and dense rankings both put it first (`rank::top_agree`) and
/// it strictly leads the fused list: `agreed` holds (repo, path, start_line) of those hits.
fn should_rerank(hits: &[muninn_core::Hit], agreed: &[(String, String, usize)]) -> bool {
    let [top, second, ..] = hits else {
        return false;
    };
    let top_agrees = agreed
        .iter()
        .any(|(r, p, l)| *r == top.repo && *p == top.path && *l == top.start_line);
    !(top_agrees && second.score < top.score)
}

/// Keeps at most `max_per_file` hits per (repo, path) in front; the rest follow in their
/// original order, so `k` is still reached when the diverse set is short. A no-op when there
/// are no more than `k` hits (nothing would be gained) or `max_per_file` is 0.
fn diversify(hits: Vec<muninn_core::Hit>, k: usize, max_per_file: usize) -> Vec<muninn_core::Hit> {
    if hits.len() <= k || max_per_file == 0 {
        return hits;
    }
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let (mut keep, mut demoted) = (Vec::with_capacity(hits.len()), Vec::new());
    for h in hits {
        let n = seen.entry((h.repo.clone(), h.path.clone())).or_default();
        *n += 1;
        if *n <= max_per_file {
            keep.push(h);
        } else {
            demoted.push(h);
        }
    }
    keep.extend(demoted);
    keep
}

fn ms_since(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1e5).round() / 100.0
}

/// One JSON line per search on stderr (the daemon's event log): what agents ask and how long
/// it took. `v` is the /search response.
fn log_search(q: &Q, mode: Mode, k: usize, v: &Value) {
    let hits = v["hits"].as_array().map(Vec::len).unwrap_or(0);
    let top = v["hits"].get(0).map(|h| {
        let base = format!(
            "{}:{}",
            h["repo"].as_str().unwrap_or(""),
            h["path"].as_str().unwrap_or("")
        );
        match h["symbol"].as_str() {
            Some(s) => format!("{base}#{s}"),
            None => base,
        }
    });
    eprintln!(
        "{}",
        json!({
            "event": "search",
            "q": q.q.chars().take(200).collect::<String>(),
            "repo": q.repo,
            "mode": mode,
            "k": k,
            "ms": v["timings_ms"]["total"],
            "hits": hits,
            "top": top,
            "cached": v["timings_ms"]["cached"],
        })
    );
}

/// /search. Per request: embed the query once (non-lexical modes), search every repo in
/// parallel (`muninn_core::index::search_shared`, which holds a repo's lock only for its
/// in-memory stages), merge by score (ties keep repo-name order), filter by path/kind,
/// rerank the top `MUNINN_RERANK_N` (default 20) unless `should_rerank` says it cannot help,
/// then diversify: at most `MUNINN_MAX_PER_FILE` (default 0 = off; a cap of 3 cost a large repo
/// 0.03 recall@10 on real queries) hits per file ahead of the rest, when there are more than k. The response carries `timings_ms`:
/// `embed`, `rerank`, `total`, and `lexical`/`dense`/`fuse`/`texts` as the slowest repo's
/// stage time (repos run in parallel; merge and diversify count as `fuse`), plus `cached`.
fn search_blocking(app: &App, q: Q) -> Json<Value> {
    let t_total = Instant::now();
    let mode = q.mode.unwrap_or(Mode::Hybrid);
    let k = q.k.unwrap_or(10);
    let context = q.context.map(|c| c.min(50));
    let xmode = expand_mode(&q, mode);
    let key = json!([
        q.q,
        q.repo,
        q.branch,
        mode,
        k,
        q.path,
        q.kind,
        context,
        format!("{xmode:?}")
    ])
    .to_string();
    let cached = {
        let mut c = app.qcache.lock().unwrap();
        c.get(&key, Instant::now()).ok_or_else(|| c.generation())
    };
    let generation = match cached {
        Ok(mut v) => {
            v["timings_ms"] = json!({"expand": 0.0, "embed": 0.0, "lexical": 0.0, "dense": 0.0,
                "fuse": 0.0, "rerank": 0.0, "texts": 0.0, "total": ms_since(t_total), "cached": true});
            log_search(&q, mode, k, &v);
            return Json(v);
        }
        Err(g) => g,
    };
    let filtered = q.path.is_some() || q.kind.is_some();
    let fetch = if filtered {
        (k.max(20) * 4).min(200)
    } else {
        k.max(20)
    };
    let mut ix: Vec<(String, Arc<Mutex<Index>>)> = app
        .indexes
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| q.repo.as_ref().is_none_or(|r| r == *n))
        .map(|(n, i)| (n.clone(), i.clone()))
        .collect();
    // Each repo's n-th lexical hit has the same RRF score; the stable sort below keeps repo-name
    // order for those ties instead of HashMap order, which changed on every restart.
    ix.sort_by(|a, b| a.0.cmp(&b.0));
    let t = Instant::now();
    let (expansion, lexical_q, dense_q) = expand_query(&q.q, xmode, mode);
    let rerank_q = match xmode {
        muninn_core::ExpandMode::Llm | muninn_core::ExpandMode::Both => lexical_q.clone(),
        _ => q.q.clone(),
    };
    let expand_ms = ms_since(t);
    let t = Instant::now();
    let qv: Option<Vec<f32>> = match (mode, app.emb.as_deref()) {
        (Mode::Lexical, _) | (_, None) => None,
        (_, Some(e)) => Some(
            e.embed(&[muninn_core::query_text(&dense_q)])
                .pop()
                .unwrap_or_default(),
        ),
    };
    let embed_ms = ms_since(t);
    // Expansion terms reach BM25 only; the reranker below judges against the original query.
    let one = |i: &Mutex<Index>| {
        muninn_core::index::search_shared(
            i,
            &lexical_q,
            q.branch.as_deref(),
            mode,
            fetch,
            qv.as_deref(),
        )
    };
    // Repos run in parallel; results are collected in repo-name order.
    let results: Vec<(String, Option<muninn_core::index::RepoHits>)> = if ix.len() == 1 {
        ix.iter().map(|(n, i)| (n.clone(), one(i).ok())).collect()
    } else {
        std::thread::scope(|s| {
            let hs: Vec<_> = ix
                .iter()
                .map(|(n, i)| (n.clone(), s.spawn(move || one(i))))
                .collect();
            hs.into_iter()
                .map(|(n, h)| (n, h.join().ok().and_then(|r| r.ok())))
                .collect()
        })
    };
    let t_merge = Instant::now();
    let mut hits = Vec::new();
    // Poisoned lock (panicked ingest) or a panicked search thread.
    let mut busy = Vec::new();
    let mut agreed: Vec<(String, String, usize)> = Vec::new();
    let mut st = muninn_core::index::Timings::default();
    for (name, r) in results {
        let Some(r) = r else {
            busy.push(name);
            continue;
        };
        st.lexical = st.lexical.max(r.timings.lexical);
        st.dense = st.dense.max(r.timings.dense);
        st.fuse = st.fuse.max(r.timings.fuse);
        st.texts = st.texts.max(r.timings.texts);
        if r.top_agree
            && let Some(h) = r.hits.first()
        {
            agreed.push((h.repo.clone(), h.path.clone(), h.start_line));
        }
        hits.extend(r.hits);
    }
    if filtered {
        hits.retain(|h| {
            q.path.as_deref().is_none_or(|p| glob_match(p, &h.path))
                && q.kind.as_deref().is_none_or(|kd| h.kind == kd)
        });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut merge_ms = ms_since(t_merge);
    let mut rerank_ms = 0.0;
    if let (Some(r), true) = (app.rr.as_deref(), mode != Mode::Lexical) {
        // Rerank a window wider than k, so the reranker can promote a hit that fusion ranked
        // low, instead of only reordering the top 10.
        let rn = std::env::var("MUNINN_RERANK_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20)
            .max(k);
        hits.truncate(rn);
        if should_rerank(&hits, &agreed) {
            let t = Instant::now();
            let docs: Vec<String> = hits.iter().map(rerank_doc).collect();
            let rs = r.rerank(&rerank_q, &docs);
            if rs.iter().any(|s| *s > f32::MIN) {
                for (h, s) in hits.iter_mut().zip(rs) {
                    h.score = s as f64;
                }
                hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            }
            rerank_ms = ms_since(t);
        }
    }
    let t = Instant::now();
    let max_per_file = std::env::var("MUNINN_MAX_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut hits = diversify(hits, k, max_per_file);
    hits.truncate(k);
    merge_ms += ms_since(t);
    let starts = match context {
        Some(c) => expand_snippets(app, &mut hits, c),
        None => hits.iter().map(|h| h.start_line).collect(),
    };
    let mut hv = serde_json::to_value(&hits).unwrap_or_else(|_| json!([]));
    if let Some(a) = hv.as_array_mut() {
        for (h, s) in a.iter_mut().zip(starts) {
            h["snippet_start"] = json!(s);
        }
    }
    let round = |x: f64| (x * 100.0).round() / 100.0;
    let mut v = json!({
        "hits": hv,
        "busy_repos": busy,
        "expansion": {
            "mode": format!("{xmode:?}").to_lowercase(),
            "lexical_query": (lexical_q != q.q).then_some(&lexical_q),
            "hyde": expansion.hyde,
        },
        "timings_ms": {
            "expand": expand_ms,
            "embed": embed_ms,
            "lexical": round(st.lexical),
            "dense": round(st.dense),
            "fuse": round(st.fuse + merge_ms),
            "rerank": rerank_ms,
            "texts": round(st.texts),
            "total": 0.0,
            "cached": false,
        },
    });
    v["timings_ms"]["total"] = json!(ms_since(t_total));
    log_search(&q, mode, k, &v);
    // Results change file by file during an ingest; only cache when no repo is being indexed.
    let ingesting = app.active.lock().map(|a| a.is_some()).unwrap_or(true);
    if busy.is_empty() && !ingesting {
        app.qcache
            .lock()
            .unwrap()
            .put(key, v.clone(), Instant::now(), generation);
    }
    Json(v)
}

/// Replaces each committed hit's snippet with its full chunk lines plus `ctx` lines around them,
/// read from the git blob (cap 1 MB). Live hits and unreadable blobs keep their snippet. Returns
/// each hit's 1-based snippet start line.
fn expand_snippets(app: &App, hits: &mut [muninn_core::Hit], ctx: usize) -> Vec<usize> {
    let mut blobs: HashMap<(String, String, String), Option<Vec<String>>> = HashMap::new();
    hits.iter_mut()
        .map(|h| {
            if h.branch == muninn_core::LIVE || !safe_ref(&h.branch) || !safe_ref(&h.path) {
                return h.start_line;
            }
            let lines = blobs
                .entry((h.repo.clone(), h.branch.clone(), h.path.clone()))
                .or_insert_with(|| {
                    let b = git(
                        &app.repo_dir(&h.repo),
                        &["show", &format!("{}:{}", h.branch, h.path)],
                    )?;
                    (b.len() <= MAX_VIEW).then(|| {
                        String::from_utf8_lossy(&b)
                            .lines()
                            .map(String::from)
                            .collect()
                    })
                });
            let Some(lines) = lines.as_ref().filter(|l| !l.is_empty()) else {
                return h.start_line;
            };
            let s = h
                .start_line
                .max(1)
                .saturating_sub(ctx)
                .max(1)
                .min(lines.len());
            let e = (h.end_line + ctx).min(lines.len()).max(s);
            h.snippet = lines[s - 1..e].join("\n");
            s
        })
        .collect()
}

#[derive(Deserialize)]
struct GQ {
    scope: Option<String>,
    limit: Option<usize>,
    k: Option<i64>,
}

async fn graph(
    State(app): State<Arc<App>>,
    Query(q): Query<GQ>,
    h: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(store) = app.store.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no store").into_response();
    };
    let scope = q.scope.unwrap_or_else(|| "all".into());
    let limit = q.limit.unwrap_or(20000).clamp(100, 100000);
    let k = q.k.unwrap_or(4).clamp(1, 16);
    let key = format!("{scope}|{limit}|{k}");
    let a2 = app.clone();
    let r = tokio::task::spawn_blocking(move || {
        let ver = store.graph_version();
        let etag = format!("\"{:x}\"", {
            use sha2::Digest;
            let d = Sha256::digest(format!("{key}|{ver}").as_bytes());
            u64::from_be_bytes(d[..8].try_into().unwrap())
        });
        if let Some((e, b)) = a2.graph_cache.lock().unwrap().get(&key)
            && *e == etag
        {
            return (etag, b.clone());
        }
        let v = graph::build(&store, &scope, limit, k, &ver);
        let b = Arc::new(serde_json::to_vec(&v).unwrap());
        a2.graph_cache
            .lock()
            .unwrap()
            .insert(key, (etag.clone(), b.clone()));
        (etag, b)
    });
    let Ok((etag, body)) = r.await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "graph failed").into_response();
    };
    let hd = [
        ("etag", etag.clone()),
        ("content-type", "application/json".to_string()),
        ("cache-control", "no-cache".to_string()),
    ];
    if h.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [("etag", etag)]).into_response();
    }
    (hd, body.as_ref().clone()).into_response()
}

async fn repos(State(app): State<Arc<App>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(store) = app.store.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no store").into_response();
    };
    let Ok(mut list) = tokio::task::spawn_blocking(move || store.repo_stats()).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let loaded = app.indexes.lock().unwrap();
    for r in list.iter_mut() {
        let l = r["name"].as_str().is_some_and(|n| loaded.contains_key(n));
        r["loaded"] = l.into();
    }
    Json(json!({ "repos": list })).into_response()
}

const MAX_VIEW: usize = 1024 * 1024;

/// A git object id as push webhooks send it (hex, abbreviated or full SHA-1/SHA-256).
fn object_id(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Webhook fields go to git as a mirror path and as revisions: the repo must name a mirror in
/// MUNINN_REPOS_DIR (no path), and before/after must be object ids, never `--option`s.
fn push_args_ok(repo: &str, branch: &str, before: Option<&str>, after: &str) -> bool {
    safe_ref(repo)
        && !repo.contains('/')
        && safe_ref(branch)
        && object_id(after)
        && before.is_none_or(object_id)
}

/// Reject anything that could escape the repo or be read as a git option.
pub fn safe_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.starts_with('/')
        && !s.contains('\0')
        && !s.contains('\\')
        && !s.split('/').any(|p| p == ".." || p.is_empty())
        && !s.contains("..")
}

#[derive(Deserialize)]
struct FQ {
    repo: String,
    branch: String,
    path: String,
}

async fn file(State(app): State<Arc<App>>, Query(q): Query<FQ>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if ![&q.repo, &q.branch, &q.path].iter().all(|s| safe_ref(s)) || q.repo.contains('/') {
        return (StatusCode::BAD_REQUEST, "bad repo, branch or path").into_response();
    }
    let dir = app.repos_dir.join(format!("{}.git", q.repo));
    let r = tokio::task::spawn_blocking(move || {
        if !dir.exists() {
            return None;
        }
        git(
            &dir,
            &["cat-file", "blob", &format!("{}:{}", q.branch, q.path)],
        )
    })
    .await
    .ok()
    .flatten();
    match r {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(b) if b.len() > MAX_VIEW => {
            (StatusCode::PAYLOAD_TOO_LARGE, "file over 1 MB").into_response()
        }
        Some(b) => (
            [("content-type", "text/plain; charset=utf-8")],
            String::from_utf8_lossy(&b).into_owned(),
        )
            .into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok"
}
async fn readyz(State(app): State<Arc<App>>) -> (StatusCode, Json<Value>) {
    // The store client is blocking (it runs its own runtime), so query it off the async workers.
    let a = app.clone();
    let (missing, embed_pending) =
        tokio::task::spawn_blocking(move || (a.missing_repos(), a.embed_pending()))
            .await
            .unwrap_or_default();
    let n = app
        .indexes
        .try_lock()
        .map(|g| g.len())
        .unwrap_or(usize::MAX);
    let active = app.active.try_lock().ok().and_then(|g| g.clone());
    let qc = app
        .qcache
        .lock()
        .map(|c| (c.hits, c.len()))
        .unwrap_or_default();
    let ingest = json!({
        "active_repo": active.as_ref().map(|a| &a.0),
        "active_ms": active.as_ref().map(|a| now_ms().saturating_sub(a.1)),
        "files_done": app.files_done.load(Ordering::Relaxed),
        "files_total": app.files_total.load(Ordering::Relaxed),
        "queued": app.gate.available_permits() == 0,
    });
    (
        if n > 0 {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(
            json!({"missing": missing, "repos": if n == usize::MAX { Value::Null } else { json!(n) }, "ingest": ingest, "loaded": app.load_done.load(Ordering::Relaxed), "to_load": app.load_total.load(Ordering::Relaxed), "last_index_lag_ms": app.lag_ms.load(Ordering::Relaxed), "pushes": app.pushes.load(Ordering::Relaxed), "cache_hits": qc.0, "cache_size": qc.1, "embedded_backfill": app.embed_done.load(Ordering::Relaxed), "embed_pending": embed_pending, "queues": {"embed": muninn_core::EMBED_GATE.stats(), "rerank": muninn_core::RERANK_GATE.stats()}}),
        ),
    )
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/webhook/push", post(webhook))
        .route("/search", get(search))
        .route("/graph", get(graph))
        .route("/repos", get(repos))
        .route("/live", post(live))
        .route("/file", get(file))
        .route("/symbol", get(symbols::lookup))
        .route("/symbol/explain", get(symbols::explain))
        .route("/symbols/for_hits", post(symbols::for_hits))
        .route("/symbols/stats", get(symbols::stats))
        .route("/symbols/reindex", post(symbols::reindex))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(app)
}

const OPEN_PATHS: [&str; 3] = ["/healthz", "/readyz", "/webhook/push"];

fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Require `Authorization: Bearer <token>` on every route except health probes
/// and the HMAC-verified push webhook.
pub fn require_token(router: Router, token: String) -> Router {
    router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let token = token.clone();
            async move {
                let authed = OPEN_PATHS.contains(&req.uri().path())
                    || req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .is_some_and(|given| token_matches(given, &token));
                if authed {
                    next.run(req).await
                } else {
                    axum::response::IntoResponse::into_response(StatusCode::UNAUTHORIZED)
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::safe_ref;

    #[test]
    fn webhook_rejects_paths_and_options_as_push_fields() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(super::push_args_ok("api", "main", None, sha));
        assert!(super::push_args_ok("api", "feature/x", Some(sha), sha));
        assert!(!super::push_args_ok("../../etc", "main", None, sha));
        assert!(!super::push_args_ok("a/b", "main", None, sha));
        assert!(!super::push_args_ok("api", "../x", None, sha));
        assert!(!super::push_args_ok("api", "main", None, "--output=/tmp/x"));
        assert!(!super::push_args_ok(
            "api",
            "main",
            Some("--output=/tmp/x"),
            sha
        ));
        assert!(!super::push_args_ok("api", "main", None, "HEAD"));
    }

    #[test]
    fn unscoped_search_orders_tied_repos_by_name() {
        let app = super::App::new(std::env::temp_dir(), None);
        for name in ["zeta", "alpha", "mid", "beta", "omega", "kappa"] {
            let mut ix = muninn_core::Index::new(name);
            ix.apply_push(
                "main",
                vec![muninn_core::Change::Upsert {
                    path: "a.rs".into(),
                    text: "fn tiedword() {}\n".into(),
                }],
                None,
            );
            app.indexes
                .lock()
                .unwrap()
                .insert(name.into(), std::sync::Arc::new(std::sync::Mutex::new(ix)));
        }
        let q = super::Q {
            q: "tiedword".into(),
            repo: None,
            branch: None,
            mode: Some(muninn_core::Mode::Lexical),
            k: Some(3),
            path: None,
            kind: None,
            context: None,
            expand: None,
        };
        let v = super::search_blocking(&app, q).0;
        let repos: Vec<&str> = v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["repo"].as_str().unwrap())
            .collect();
        assert_eq!(repos, ["alpha", "beta", "kappa"]);
    }

    fn rss_kb() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn release_free_memory_returns_freed_heap_to_the_kernel() {
        // 100 MB of small blocks; the last one stays alive so the heap top cannot shrink on its own.
        let mut blocks: Vec<Box<[u8]>> = (0..100_000)
            .map(|i| vec![i as u8; 1024].into_boxed_slice())
            .collect();
        let keep = blocks.pop();
        drop(blocks);
        let before = rss_kb();
        super::release_free_memory();
        let after = rss_kb();
        assert!(before > after + 50_000, "rss {before} kB -> {after} kB");
        drop(keep);
    }
    #[test]
    fn glob_matches_segments_and_globstar() {
        use super::glob_match as g;
        assert!(g("crates/*/src/lib.rs", "crates/muninn-core/src/lib.rs"));
        assert!(!g("crates/*/lib.rs", "crates/muninn-core/src/lib.rs"));
        assert!(g("crates/**/lib.rs", "crates/muninn-core/src/lib.rs"));
        assert!(g("crates/**/lib.rs", "crates/lib.rs"));
        assert!(g("**/*.rs", "a.rs"));
        assert!(g("**", "a/b/c"));
        assert!(g("src/?.rs", "src/a.rs"));
        assert!(!g("src/?.rs", "src/ab.rs"));
        assert!(!g("src?a.rs", "src/a.rs"));
        assert!(
            g("*.rs", "deep/dir/x.rs"),
            "slashless pattern matches the basename"
        );
        assert!(!g("*.rs", "deep/dir/x.ts"));
        assert!(g("src/*", "src/main.rs"));
        assert!(!g("src/*", "src/a/main.rs"));
        assert!(g("crates/muninn-mcp/**", "crates/muninn-mcp/src/lib.rs"));
        assert!(!g("crates/muninn-mcp/**", "crates/muninn-core/src/lib.rs"));
        assert!(g("exact.txt", "exact.txt"));
        assert!(!g("", "a"));
    }

    #[test]
    fn query_cache_ttl_eviction_and_invalidation() {
        use std::time::{Duration, Instant};
        let mut c = super::QueryCache::default();
        let t0 = Instant::now();
        let g = c.generation();
        c.put("a".into(), serde_json::json!(1), t0, g);
        assert_eq!(
            c.get("a", t0 + Duration::from_secs(599)),
            Some(serde_json::json!(1))
        );
        assert_eq!(c.hits, 1);
        assert_eq!(
            c.get("a", t0 + Duration::from_secs(601)),
            None,
            "expired after 10 min"
        );
        assert_eq!(c.len(), 0);
        // Full: the oldest entry is evicted.
        for i in 0..super::QC_CAP {
            c.put(
                format!("k{i}"),
                serde_json::json!(i),
                t0 + Duration::from_millis(i as u64),
                g,
            );
        }
        assert_eq!(c.len(), super::QC_CAP);
        c.put(
            "new".into(),
            serde_json::json!("n"),
            t0 + Duration::from_secs(1),
            g,
        );
        assert_eq!(c.len(), super::QC_CAP);
        assert!(c.get("k0", t0 + Duration::from_secs(1)).is_none());
        assert!(c.get("k1", t0 + Duration::from_secs(1)).is_some());
        // Full with expired entries: all expired ones go.
        c.put(
            "late".into(),
            serde_json::json!(0),
            t0 + Duration::from_secs(700),
            g,
        );
        assert_eq!(c.len(), 1);
        // Clear drops everything and refuses stores from searches that began before it.
        c.clear();
        assert!(c.is_empty());
        c.put("stale".into(), serde_json::json!(0), t0, g);
        assert!(c.is_empty());
        c.put("fresh".into(), serde_json::json!(0), t0, c.generation());
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn search_results_are_cached_until_mutation() {
        let app = super::App::new(std::env::temp_dir(), None);
        let mut ix = muninn_core::Index::new("r");
        ix.apply_push(
            "main",
            vec![muninn_core::Change::Upsert {
                path: "a.rs".into(),
                text: "fn cachedword() {}\n".into(),
            }],
            None,
        );
        app.indexes
            .lock()
            .unwrap()
            .insert("r".into(), std::sync::Arc::new(std::sync::Mutex::new(ix)));
        let q = || super::Q {
            q: "cachedword".into(),
            repo: None,
            branch: None,
            mode: Some(muninn_core::Mode::Lexical),
            k: None,
            path: None,
            kind: None,
            context: None,
            expand: None,
        };
        let a = super::search_blocking(&app, q()).0;
        assert_eq!(a["hits"].as_array().unwrap().len(), 1);
        assert_eq!(app.qcache.lock().unwrap().len(), 1);
        let b = super::search_blocking(&app, q()).0;
        assert_eq!(a["hits"], b["hits"]);
        assert_eq!(a["timings_ms"]["cached"], false);
        assert_eq!(b["timings_ms"]["cached"], true, "served from the cache");
        assert_eq!(app.qcache.lock().unwrap().hits, 1);
        app.live_update(
            "r",
            vec![(
                "b.rs".into(),
                Some("fn cachedword_two() { cachedword() }\n".into()),
            )],
            false,
        );
        assert_eq!(
            app.qcache.lock().unwrap().len(),
            0,
            "live update clears the cache"
        );
        let c = super::search_blocking(&app, q()).0;
        assert_eq!(c["hits"].as_array().unwrap().len(), 2);
    }

    fn hit(repo: &str, path: &str, line: usize, score: f64) -> muninn_core::Hit {
        muninn_core::Hit {
            repo: repo.into(),
            branch: "main".into(),
            path: path.into(),
            symbol: None,
            kind: "function_item".into(),
            start_line: line,
            end_line: line + 1,
            score,
            snippet: String::new(),
            text: String::new(),
        }
    }

    #[test]
    fn search_reports_stage_timings() {
        let app = super::App::new(std::env::temp_dir(), None);
        for name in ["r1", "r2"] {
            let mut ix = muninn_core::Index::new(name);
            ix.apply_push(
                "main",
                vec![muninn_core::Change::Upsert {
                    path: "a.rs".into(),
                    text: "fn timedword() {}\n".into(),
                }],
                None,
            );
            app.indexes
                .lock()
                .unwrap()
                .insert(name.into(), std::sync::Arc::new(std::sync::Mutex::new(ix)));
        }
        let q = super::Q {
            q: "timedword".into(),
            repo: None,
            branch: None,
            mode: Some(muninn_core::Mode::Hybrid),
            k: None,
            path: None,
            kind: None,
            context: None,
            expand: None,
        };
        let v = super::search_blocking(&app, q).0;
        assert_eq!(v["hits"].as_array().unwrap().len(), 2);
        let t = &v["timings_ms"];
        for f in [
            "embed", "lexical", "dense", "fuse", "rerank", "texts", "total",
        ] {
            assert!(t[f].as_f64().is_some_and(|x| x >= 0.0), "{f}: {t}");
        }
        assert!(t["total"].as_f64() >= t["lexical"].as_f64());
        assert_eq!(t["cached"], false);
    }

    #[test]
    fn diversify_caps_hits_per_file_and_backfills() {
        let hits: Vec<_> = (0..6)
            .map(|i| hit("r", "big.rs", i * 10, 10.0 - i as f64))
            .chain([hit("r", "other.rs", 1, 3.0), hit("s", "big.rs", 1, 2.0)])
            .collect();
        let order = |v: &[muninn_core::Hit]| {
            v.iter()
                .map(|h| format!("{}:{}:{}", h.repo, h.path, h.start_line))
                .collect::<Vec<_>>()
        };
        let d = super::diversify(hits.clone(), 5, 3);
        assert_eq!(
            order(&d[..5]),
            [
                "r:big.rs:0",
                "r:big.rs:10",
                "r:big.rs:20",
                "r:other.rs:1",
                "s:big.rs:1"
            ],
            "third hit per (repo, path) at most; same path in another repo is another file"
        );
        assert_eq!(
            d.len(),
            hits.len(),
            "demoted hits follow, so k is still reachable"
        );
        assert_eq!(
            order(&d[5..]),
            ["r:big.rs:30", "r:big.rs:40", "r:big.rs:50"]
        );
        assert_eq!(
            order(&super::diversify(hits.clone(), 8, 3)),
            order(&hits),
            "no more than k candidates: unchanged"
        );
        assert_eq!(order(&super::diversify(hits.clone(), 5, 0)), order(&hits));
    }

    #[test]
    fn rerank_skipped_for_one_candidate_or_agreed_leader() {
        let agreed = vec![("r".to_string(), "a.rs".to_string(), 1)];
        let a = hit("r", "a.rs", 1, 0.03);
        let b = hit("r", "b.rs", 1, 0.02);
        assert!(!super::should_rerank(&[], &agreed));
        assert!(!super::should_rerank(std::slice::from_ref(&a), &[]));
        assert!(!super::should_rerank(&[a.clone(), b.clone()], &agreed));
        assert!(super::should_rerank(&[a.clone(), b.clone()], &[]));
        assert!(super::should_rerank(&[b.clone(), a.clone()], &agreed));
        let tied = hit("s", "c.rs", 1, 0.03);
        assert!(
            super::should_rerank(&[a, tied], &agreed),
            "another repo's hit ties the leader"
        );
    }

    #[test]
    fn rerank_doc_has_header_and_clipped_text() {
        let mut h = hit("r", "src/a.rs", 1, 1.0);
        h.symbol = Some("alpha".into());
        h.text = "x".repeat(5000);
        let d = super::rerank_doc(&h);
        assert!(d.starts_with("src/a.rs alpha\nxxx"));
        assert_eq!(
            d.chars().count(),
            "src/a.rs alpha\n".len() + super::RERANK_DOC_CHARS
        );
        h.symbol = None;
        h.text.clear();
        h.snippet = "fn a() {}".into();
        assert_eq!(super::rerank_doc(&h), "src/a.rs\nfn a() {}");
    }

    #[test]
    fn safe_ref_rejects_traversal() {
        assert!(safe_ref("crates/a/src/lib.rs"));
        assert!(safe_ref("feat/x"));
        for bad in [
            "",
            "../x",
            "a/../b",
            "/etc/passwd",
            "-p",
            "a//b",
            "a\\b",
            "a/b/",
        ] {
            assert!(!safe_ref(bad), "{bad}");
        }
    }
}

#[cfg(test)]
mod ingest_tests {
    use muninn_core::Change;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    /// A fresh repo under the system temp dir; returns (repos_dir, repo_dir).
    fn temp_repo(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "muninn-ingest-{tag}-{}-{}",
            std::process::id(),
            super::now_ms()
        ));
        let dir = root.join("r");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        (root, dir)
    }

    fn by_path(ch: &[Change]) -> Vec<(String, &'static str)> {
        let mut v: Vec<(String, &'static str)> = ch
            .iter()
            .map(|c| match c {
                Change::Upsert { path, .. } => (path.clone(), "upsert"),
                Change::Delete { path } => (path.clone(), "delete"),
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn git_changes_reads_blobs_in_one_batch_and_classifies() {
        let (root, dir) = temp_repo("blobs");
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("with space.txt"), "spaced name\n").unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(super::MAX_FILE + 1)).unwrap();
        std::fs::write(dir.join("edge.txt"), "y".repeat(super::MAX_FILE)).unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 159, 146, 150, 255, 0, 1]).unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::create_dir_all(dir.join("gen")).unwrap();
        std::fs::write(dir.join("gen/out.rs"), "fn generated() {}\n").unwrap();
        std::fs::write(dir.join(".muninn-ignore"), "# generated\ngen/\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "one"]);
        let c1 = git(&dir, &["rev-parse", "HEAD"]);

        let full = super::git_changes(&dir, None, &c1);
        assert_eq!(
            by_path(&full),
            [
                (".muninn-ignore".to_string(), "upsert"),
                ("a.rs".into(), "upsert"),
                ("big.txt".into(), "delete"),
                ("bin.dat".into(), "delete"),
                ("edge.txt".into(), "upsert"),
                ("empty.txt".into(), "upsert"),
                ("with space.txt".into(), "upsert"),
            ]
        );
        let text = |p: &str| {
            full.iter()
                .find_map(|c| match c {
                    Change::Upsert { path, text } if path == p => Some(text.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(text("a.rs"), "fn alpha() {}\n");
        assert_eq!(text("with space.txt"), "spaced name\n");
        assert_eq!(text("edge.txt").len(), super::MAX_FILE);
        assert_eq!(text("empty.txt"), "");

        // Diff mode: modified, added and deleted files; the all-zero "before" means full tree.
        std::fs::write(dir.join("a.rs"), "fn alpha2() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn beta() {}\n").unwrap();
        std::fs::remove_file(dir.join("empty.txt")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "two"]);
        let c2 = git(&dir, &["rev-parse", "HEAD"]);
        let diff = super::git_changes(&dir, Some(&c1), &c2);
        assert_eq!(
            by_path(&diff),
            [
                ("a.rs".to_string(), "upsert"),
                ("b.rs".into(), "upsert"),
                ("empty.txt".into(), "delete"),
            ]
        );
        let zero = "0".repeat(40);
        assert_eq!(super::git_changes(&dir, Some(&zero), &c2).len(), 7);

        // Missing paths and a broken repo yield None, never a hang or a panic.
        let got = super::read_blobs(&dir, &c2, &["nope.rs".into(), "b.rs".into(), "x\ny".into()]);
        assert_eq!(got, [None, Some("fn beta() {}\n".into()), None]);
        assert_eq!(
            super::read_blobs(&root.join("absent"), "HEAD", &["a.rs".into()]),
            [None]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Records the size of every embed() call.
    struct CountingEmb(Arc<Mutex<Vec<usize>>>);
    impl muninn_core::Embedder for CountingEmb {
        fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
            self.0.lock().unwrap().push(texts.len());
            texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let mut v = vec![0.0; muninn_core::pg::DIM];
                    v[i % muninn_core::pg::DIM] = 1.0;
                    v
                })
                .collect()
        }
    }

    #[test]
    fn index_push_embeds_across_files_in_large_calls() {
        let (root, dir) = temp_repo("embed");
        let files = 60;
        for i in 0..files {
            std::fs::write(
                dir.join(format!("f{i}.rs")),
                format!(
                    "fn one_{i}() -> u32 {{ {i} }}\n\nfn two_{i}() -> u32 {{ {i} + 1 }}\n\nfn three_{i}() -> u32 {{ {i} + 2 }}\n"
                ),
            )
            .unwrap();
        }
        std::fs::write(dir.join("bin.dat"), [0u8, 255, 0]).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "one"]);
        let sha = git(&dir, &["rev-parse", "HEAD"]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let app = super::App::with_backends(
            root.clone(),
            None,
            None,
            Some(Box::new(CountingEmb(calls.clone()))),
            None,
        );
        let n = app.index_push("r", "main", None, &sha);
        assert_eq!(n, files + 1);
        assert_eq!(
            app.files_done.load(std::sync::atomic::Ordering::Relaxed),
            (files + 1) as u64
        );
        let ix = app.indexes.lock().unwrap().get("r").unwrap().clone();
        let chunks = ix.lock().unwrap().chunk_count();
        let sizes = calls.lock().unwrap().clone();
        assert_eq!(sizes.iter().sum::<usize>(), chunks, "{sizes:?}");
        assert!(chunks >= files * 3, "{chunks}");
        let (last, full) = sizes.split_last().unwrap();
        assert!(
            full.iter().all(|&s| s >= super::INGEST_EMBED_TEXTS),
            "{sizes:?}"
        );
        assert!(*last >= 1 && sizes.len() < files / 4, "{sizes:?}");
        for i in 0..files {
            assert!(ix.lock().unwrap().base_has(&format!("f{i}.rs"), None));
        }
        assert!(!ix.lock().unwrap().base_has("bin.dat", None));

        // A second push of one changed file embeds just that file's new chunk(s).
        std::fs::write(dir.join("f0.rs"), "fn changed() {}\n").unwrap();
        git(&dir, &["commit", "-qam", "two"]);
        let sha2 = git(&dir, &["rev-parse", "HEAD"]);
        calls.lock().unwrap().clear();
        assert_eq!(app.index_push("r", "main", Some(&sha), &sha2), 1);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(app.files_done.load(std::sync::atomic::Ordering::Relaxed), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
