use crate::chunk::Chunk;
use postgres::{Client, NoTls};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// Embedding width: EmbeddingGemma 2 (768-d; was 1024 for Qwen3-Embedding-0.6B).
pub const DIM: usize = 768;

/// Recent per-repo dense results: (repo, hash of the query vector bits, n) -> chunk hashes.
type DenseKey = (String, u64, usize);
const DENSE_CACHE: usize = 32;
/// Rows of the one global HNSW query shared by every repo of a search. 92% of the chunks are
/// one repo's, so its `2n` nearest are almost always among the global `GLOBAL_LIMIT`; a repo
/// with fewer rows in that list gets its own repo-filtered query instead.
const GLOBAL_LIMIT: i64 = 300;
/// (query vector key, (repo, hash) nearest first).
type GlobalNearest = (u64, Vec<(String, String)>);

/// Session settings of every search read connection, sent once right after it connects:
/// `iterative_scan` lets a repo-filtered HNSW scan keep walking the graph until it found enough
/// rows of that repo instead of returning the few among the global top `ef_search`.
const READ_SESSION: &str = "set hnsw.ef_search = 100; set hnsw.iterative_scan = relaxed_order";
/// Dedicated read connections for searches (dense + texts), so parallel per-repo searches do
/// not queue behind each other or behind an ingest transaction on the write connection.
const READ_POOL: usize = 8;

/// A lazily connected pool of read clients; each slot reconnects (and re-applies
/// `READ_SESSION`) when its client was closed.
struct ReadPool {
    url: String,
    slots: Vec<Mutex<Option<Client>>>,
    next: std::sync::atomic::AtomicUsize,
}

impl ReadPool {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            slots: (0..READ_POOL).map(|_| Mutex::new(None)).collect(),
            next: Default::default(),
        }
    }

    /// A connected read client, or None when Postgres cannot be reached.
    fn get(&self) -> Option<MutexGuard<'_, Option<Client>>> {
        let free = self.slots.iter().find_map(|s| s.try_lock().ok());
        let mut g = match free {
            Some(g) => g,
            None => {
                let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.slots[i % self.slots.len()]
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
            }
        };
        if g.as_ref().is_none_or(|c| c.is_closed()) {
            *g = match Client::connect(&self.url, NoTls) {
                Ok(mut c) => {
                    if let Err(e) = c.batch_execute(READ_SESSION) {
                        eprintln!("muninn-core: read session setup failed: {e}");
                    }
                    Some(c)
                }
                Err(e) => {
                    eprintln!("muninn-core: postgres read connect failed: {e}");
                    None
                }
            };
        }
        g.is_some().then_some(g)
    }
}

fn vec_key(v: &[f32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for x in v {
        x.to_bits().hash(&mut h);
    }
    h.finish()
}

/// A Postgres client that reconnects after the server went away (restart, dump/restore drill).
/// The first call after a restart still fails, because that is how the client learns the socket
/// is closed; every call after that gets a fresh connection instead of "connection closed" forever.
pub struct Conn {
    url: String,
    client: Mutex<Client>,
}

impl Conn {
    pub fn connect(url: &str) -> Result<Self, postgres::Error> {
        Ok(Self::new(url, Client::connect(url, NoTls)?))
    }

    pub(crate) fn new(url: &str, client: Client) -> Self {
        Self {
            url: url.to_string(),
            client: Mutex::new(client),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Client> {
        let mut c = self.client.lock().unwrap_or_else(|p| p.into_inner());
        if c.is_closed() {
            match Client::connect(&self.url, NoTls) {
                Ok(fresh) => *c = fresh,
                Err(e) => eprintln!("muninn-core: postgres reconnect failed: {e}"),
            }
        }
        c
    }
}

pub struct PgStore {
    pub(crate) db: Conn,
    url: String,
    read: ReadPool,
    dense_cache: Mutex<std::collections::VecDeque<(DenseKey, Vec<String>)>>,
    /// The last global query: (vector key, (repo, hash) nearest first).
    global_cache: Mutex<Option<GlobalNearest>>,
}

pub struct GChunk {
    pub repo: String,
    pub hash: String,
    pub path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub start: i32,
    pub end: i32,
}

pub struct GSim {
    pub a: (String, String, String),
    pub b: (String, String, String),
    pub sim: f32,
}

pub type LoadedFiles = HashMap<(String, String), (String, Vec<String>)>;

fn vec_lit(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

impl PgStore {
    pub fn connect(url: &str) -> Result<Self, postgres::Error> {
        // The index is derived from git and can be rebuilt, so its commits skip the WAL flush:
        // one fsync per chunk insert made ingest ~0.5 files/s on this disk. Reconnects keep the option.
        let sep = if url.contains('?') { '&' } else { '?' };
        let url = &format!("{url}{sep}options=-c%20synchronous_commit%3Doff");
        let mut c = Client::connect(url, NoTls)?;
        c.batch_execute(&format!(
            "create extension if not exists vector;
             create table if not exists ix_chunks(
               repo text not null, hash text not null, path text not null, symbol text,
               kind text not null, start_line int not null, end_line int not null,
               text text not null, emb vector({DIM}), primary key(repo, hash));
             create table if not exists ix_files(
               repo text not null, branch text not null, path text not null,
               blob_sha text not null, hashes text[] not null, primary key(repo, branch, path));
             create table if not exists ix_tombstones(
               repo text not null, branch text not null, path text not null,
               blob_sha text not null, at timestamptz not null default now(),
               primary key(repo, branch, path));
             create index if not exists ix_chunks_emb on ix_chunks using hnsw (emb vector_cosine_ops);
             alter table ix_chunks add column if not exists emb_v smallint not null default 0;
             create index if not exists ix_chunks_symbol on ix_chunks(repo, lower(symbol)) where symbol is not null;
             create index if not exists ix_chunks_emb_pending on ix_chunks(repo) where emb_v < 1;"
        ))?;
        c.batch_execute(crate::symbols::SCHEMA)?;
        Ok(Self {
            db: Conn::new(url, c),
            dense_cache: Mutex::new(Default::default()),
            global_cache: Mutex::new(None),
            read: ReadPool::new(url),
            url: url.to_string(),
        })
    }

    pub fn put_chunk(&self, repo: &str, c: &Chunk, emb: Option<&[f32]>) {
        self.put_chunks(repo, &[(c, emb)]);
    }

    /// Inserts a file's new chunks in one transaction (one round trip per chunk made a full
    /// large-repo backfill spend most of its time waiting on Postgres).
    pub fn put_chunks(&self, repo: &str, rows: &[(&Chunk, Option<&[f32]>)]) {
        if rows.is_empty() {
            return;
        }
        self.bust_dense();
        let mut db = self.db.lock();
        let Ok(mut t) = db.transaction() else { return };
        let Ok(st) = t.prepare(
            "insert into ix_chunks(repo,hash,path,symbol,kind,start_line,end_line,text,emb,emb_v)
             values($1,$2,$3,$4,$5,$6,$7,$8,$9::text::vector,$10) on conflict do nothing",
        ) else {
            return;
        };
        for (c, emb) in rows {
            let e = emb.filter(|e| e.len() == DIM).map(vec_lit);
            let v: i16 = if e.is_some() {
                crate::index::EMB_VERSION
            } else {
                0
            };
            let _ = t.execute(
                &st,
                &[
                    &repo,
                    &c.content_hash,
                    &c.path,
                    &c.symbol,
                    &c.kind,
                    &(c.start_line as i32),
                    &(c.end_line as i32),
                    &c.text,
                    &e,
                    &v,
                ],
            );
        }
        let _ = t.commit();
    }

    /// (repo, hash, embedding input) of chunks that lack a vector or carry one from an older
    /// prompt scheme, across every indexed repo. The text already has the document prompt and
    /// path/symbol header applied (see `index::doc_text`).
    pub fn missing_emb(&self, limit: i64) -> Vec<(String, String, String)> {
        self.db
            .lock()
            .query(
                "select repo,hash,path,symbol,text from ix_chunks
                 where emb_v < $2 limit $1",
                &[&limit, &crate::index::EMB_VERSION],
            )
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                let (path, symbol, text): (String, Option<String>, String) =
                    (r.get(2), r.get(3), r.get(4));
                (
                    r.get(0),
                    r.get(1),
                    crate::index::doc_text(&path, symbol.as_deref(), &text),
                )
            })
            .collect()
    }

    /// Number of chunks `missing_emb` still has to fill.
    pub fn missing_emb_count(&self) -> i64 {
        self.db
            .lock()
            .query_one(
                "select count(*) from ix_chunks where emb_v < $1",
                &[&crate::index::EMB_VERSION],
            )
            .map(|r| r.get(0))
            .unwrap_or(0)
    }

    pub fn set_emb(&self, repo: &str, hash: &str, emb: &[f32]) {
        if emb.len() != DIM {
            return;
        }
        self.bust_dense();
        let _ = self.db.lock().execute(
            "update ix_chunks set emb=$3::text::vector, emb_v=$4 where repo=$1 and hash=$2",
            &[&repo, &hash, &vec_lit(emb), &crate::index::EMB_VERSION],
        );
    }

    pub fn put_file(&self, repo: &str, branch: &str, path: &str, sha: &str, hashes: &[String]) {
        let mut db = self.db.lock();
        let _ = db.execute(
            "insert into ix_files values($1,$2,$3,$4,$5)
             on conflict(repo,branch,path) do update set blob_sha=$4, hashes=$5",
            &[&repo, &branch, &path, &sha, &hashes],
        );
        let _ = db.execute(
            "delete from ix_tombstones where repo=$1 and branch=$2 and path=$3",
            &[&repo, &branch, &path],
        );
    }

    pub fn del_file(&self, repo: &str, branch: &str, path: &str, sha: &str) {
        let mut db = self.db.lock();
        let _ = db.execute(
            "delete from ix_files where repo=$1 and branch=$2 and path=$3",
            &[&repo, &branch, &path],
        );
        let _ = db.execute(
            "insert into ix_tombstones(repo,branch,path,blob_sha) values($1,$2,$3,$4)
             on conflict(repo,branch,path) do update set blob_sha=$4, at=now()",
            &[&repo, &branch, &path, &sha],
        );
    }

    pub fn del_tombstone(&self, repo: &str, branch: &str, path: &str) {
        let _ = self.db.lock().execute(
            "delete from ix_tombstones where repo=$1 and branch=$2 and path=$3",
            &[&repo, &branch, &path],
        );
    }

    /// Drops cached dense results; called on every chunk or embedding write.
    fn bust_dense(&self) {
        self.dense_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        *self.global_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Runs `sql` on a read connection (the write connection with the session settings when
    /// none can be opened).
    fn read_query(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, postgres::Error> {
        let rows = match self.read.get() {
            Some(mut c) => c.as_mut().map(|c| c.query(sql, params)),
            None => None,
        };
        match rows {
            Some(r) => r,
            None => {
                let mut db = self.db.lock();
                let _ = db.batch_execute(READ_SESSION);
                db.query(sql, params)
            }
        }
    }

    /// (repo, hash) of the `GLOBAL_LIMIT` nearest chunks over every repo, cached for the last
    /// query vector so the parallel per-repo searches of one request share one HNSW traversal.
    fn global_nearest(&self, key: u64, lit: &str) -> Vec<(String, String)> {
        // The lock is held across the query on purpose: the per-repo searches of one request
        // run in parallel and all miss at the same moment, so without it every repo ran its
        // own copy of this traversal (5 concurrent scans, 100-300 ms each instead of ~7 ms).
        let mut g = self.global_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((k, v)) = g.as_ref()
            && *k == key
        {
            return v.clone();
        }
        let rows = self
            .read_query(
                "select repo, hash from ix_chunks where emb is not null
                 order by emb <=> $1::text::vector limit $2",
                &[&lit, &GLOBAL_LIMIT],
            )
            .unwrap_or_default();
        let out: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        *g = Some((key, out.clone()));
        out
    }

    pub fn gc(&self, repo: &str) {
        self.bust_dense();
        let _ = self.db.lock().execute(
            // Hash anti-join: `c.hash = any(f.hashes)` per chunk took 38 s on a 36k-chunk repo, this ~90 ms.
            "with live as (select distinct unnest(hashes) h from ix_files where repo=$1)
             delete from ix_chunks c where c.repo=$1 and not exists (select 1 from live where live.h=c.hash)",
            &[&repo],
        );
        self.ensure_repo_emb_index(repo);
    }

    /// Repos at or below this many chunks get their own partial HNSW index.
    pub const PARTIAL_EMB_MAX_CHUNKS: i64 = 50_000;

    /// A partial HNSW index `ix_chunks_emb_<repo>` for a repo that is a small share of the
    /// table. The global HNSW scan filtered by repo must walk the graph until it has found
    /// enough rows of that repo (61 ms for a 6% repo), and an exact scan over the repo's
    /// rows costs ~25 ms per 1k chunks; a partial index answers in 2-5 ms. The planner uses it
    /// because `dense` is planned with its parameter values. Idempotent and cheap when the
    /// index exists (catalog lookup); a few seconds to build the first time.
    pub fn ensure_repo_emb_index(&self, repo: &str) {
        if repo.is_empty()
            || !repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return;
        }
        let name = format!("ix_chunks_emb_{}", repo.replace('-', "_"));
        let mut db = self.db.lock();
        let n: i64 = db
            .query_one(
                "select count(*) from ix_chunks where repo=$1 and emb is not null",
                &[&repo],
            )
            .map(|r| r.get(0))
            .unwrap_or(0);
        if n == 0 || n > Self::PARTIAL_EMB_MAX_CHUNKS {
            return;
        }
        let lit = repo.replace('\'', "''");
        if let Err(e) = db.batch_execute(&format!(
            "create index if not exists {name} on ix_chunks using hnsw (emb vector_cosine_ops) where repo = '{lit}'"
        )) {
            eprintln!(
                "{}",
                serde_json::json!({"event": "partial_emb_index_failed", "repo": repo, "error": e.to_string()})
            );
        }
    }

    pub fn has_chunk(&self, repo: &str, hash: &str) -> bool {
        self.db
            .lock()
            .query_opt(
                "select 1 from ix_chunks where repo=$1 and hash=$2",
                &[&repo, &hash],
            )
            .ok()
            .flatten()
            .is_some()
    }

    pub fn load(&self, repo: &str) -> (LoadedFiles, Vec<Chunk>) {
        self.ensure_repo_emb_index(repo);
        let mut db = self.db.lock();
        let mut files = LoadedFiles::new();
        for r in db
            .query(
                "select branch,path,blob_sha,hashes from ix_files where repo=$1",
                &[&repo],
            )
            .unwrap_or_default()
        {
            files.insert((r.get(0), r.get(1)), (r.get(2), r.get(3)));
        }
        let chunks = db
            .query(
                "select hash,path,symbol,kind,start_line,end_line,text from ix_chunks where repo=$1",
                &[&repo],
            )
            .unwrap_or_default()
            .into_iter()
            .map(|r| Chunk {
                content_hash: r.get(0),
                path: r.get(1),
                symbol: r.get(2),
                kind: r.get(3),
                start_line: r.get::<_, i32>(4) as usize,
                end_line: r.get::<_, i32>(5) as usize,
                text: r.get(6),
            })
            .collect();
        (files, chunks)
    }

    /// Chunk texts by content hash, for the few hits a search returns.
    pub fn texts(&self, repo: &str, hashes: &[&str]) -> HashMap<String, String> {
        if hashes.is_empty() {
            return HashMap::new();
        }
        self.read_query(
            "select hash, text from ix_chunks where repo=$1 and hash = any($2)",
            &[&repo, &hashes],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
    }

    pub fn repos(&self) -> Vec<String> {
        self.db
            .lock()
            .query("select distinct repo from ix_files", &[])
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.get(0))
            .collect()
    }

    /// Per repo: branches with file counts, chunk count, embedded chunk count, symbol def count.
    pub fn repo_stats(&self) -> Vec<serde_json::Value> {
        let mut db = self.db.lock();
        let mut out: std::collections::BTreeMap<String, serde_json::Value> = Default::default();
        let files = db
            .query("select repo,branch,count(*) from ix_files group by repo,branch order by repo,branch", &[])
            .unwrap_or_default();
        for r in files {
            let (repo, branch, n): (String, String, i64) = (r.get(0), r.get(1), r.get(2));
            let e = out.entry(repo.clone()).or_insert_with(|| serde_json::json!({"name": repo, "branches": [], "chunks": 0, "embedded": 0, "symbols": 0}));
            e["branches"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": branch, "files": n}));
        }
        let chunks = db
            .query(
                "select repo,count(*),count(emb) from ix_chunks group by repo",
                &[],
            )
            .unwrap_or_default();
        for r in chunks {
            if let Some(e) = out.get_mut(&r.get::<_, String>(0)) {
                e["chunks"] = r.get::<_, i64>(1).into();
                e["embedded"] = r.get::<_, i64>(2).into();
            }
        }
        let syms = db
            .query("select repo,count(*) from ix_sym_defs group by repo", &[])
            .unwrap_or_default();
        for r in syms {
            if let Some(e) = out.get_mut(&r.get::<_, String>(0)) {
                e["symbols"] = r.get::<_, i64>(1).into();
            }
        }
        out.into_values().collect()
    }

    /// Up to `2n` nearest chunk hashes (cosine) of `repo`, nearest first.
    ///
    /// `branch` is accepted but ignored: the caller filters the rows by branch and working-tree
    /// state in memory (`CandSet::allowed`), so the query over-fetches 2x instead of joining
    /// `ix_files` (that `exists` join cost ~50 ms per small repo). One repo-filtered HNSW scan
    /// with `iterative_scan` (see `READ_SESSION`) takes ~5-10 ms; Postgres picks the primary key
    /// plus an exact sort for small repos. Results are cached per (repo, query vector, n) until
    /// the next chunk or embedding write.
    pub fn dense(&self, repo: &str, _branch: Option<&str>, q: &[f32], n: usize) -> Vec<String> {
        if q.len() != DIM || n == 0 {
            return vec![];
        }
        let key: DenseKey = (repo.to_string(), vec_key(q), n);
        if let Some((_, v)) = self
            .dense_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(k, _)| *k == key)
        {
            return v.clone();
        }
        let lit = vec_lit(q);
        let limit = (n * 2) as i64;
        // One global traversal serves the big repos; a repo with fewer than n rows in it
        // (small repos, or an off-topic vector) gets its own repo-filtered scan.
        let mine: Vec<String> = self
            .global_nearest(key.1, &lit)
            .into_iter()
            .filter(|(r, _)| r == repo)
            .map(|(_, h)| h)
            .take(limit as usize)
            .collect();
        let out: Vec<String> = if mine.len() >= n {
            mine
        } else {
            // relaxed_order may return rows slightly out of distance order; the outer sort fixes that.
            let sql = "with r as materialized (
                         select hash, emb <=> $2::text::vector d from ix_chunks
                         where repo=$1 and emb is not null order by d limit $3)
                       select hash from r order by d";
            match self.read_query(sql, &[&repo, &lit, &limit]) {
                Ok(rows) => rows.iter().map(|r| r.get(0)).collect(),
                Err(e) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({"event": "dense_error", "repo": repo, "error": e.to_string()})
                    );
                    return vec![];
                }
            }
        };
        let mut c = self.dense_cache.lock().unwrap_or_else(|p| p.into_inner());
        if c.len() >= DENSE_CACHE {
            c.pop_front();
        }
        c.push_back((key, out.clone()));
        out
    }

    /// Cheap fingerprint of index content; changes on any push/embedding.
    pub fn graph_version(&self) -> String {
        let Ok(mut db) = Client::connect(&self.url, NoTls) else {
            return String::new();
        };
        let f = db
            .query_one("select count(*)::text||':'||coalesce(sum(hashtext(blob_sha||path)),0)::text from ix_files", &[])
            .map(|r| r.get::<_, String>(0))
            .unwrap_or_default();
        let c = db
            .query_one(
                "select count(*)::text||':'||count(emb)::text from ix_chunks",
                &[],
            )
            .map(|r| r.get::<_, String>(0))
            .unwrap_or_default();
        format!("{f}:{c}")
    }

    /// Distinct (repo, path, chunk_count) files.
    pub fn graph_files(&self, repo: Option<&str>) -> Vec<(String, String, i32)> {
        let mut c = Client::connect(&self.url, NoTls).expect("pg");
        c.query(
            "select repo,path,max(array_length(hashes,1)) from ix_files
             where ($1::text is null or repo=$1) group by repo,path",
            &[&repo],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.get(0), r.get(1), r.get::<_, Option<i32>>(2).unwrap_or(1)))
        .collect()
    }

    pub fn graph_chunks(&self, repo: &str, cap: i64) -> Vec<GChunk> {
        let mut c = Client::connect(&self.url, NoTls).expect("pg");
        c.query(
            "select repo,hash,path,symbol,kind,start_line,end_line from ix_chunks
             where repo=$1 order by (end_line-start_line) desc limit $2",
            &[&repo, &cap],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| GChunk {
            repo: r.get(0),
            hash: r.get(1),
            path: r.get(2),
            symbol: r.get(3),
            kind: r.get(4),
            start: r.get(5),
            end: r.get(6),
        })
        .collect()
    }

    /// Top-k cosine neighbours per embedded chunk via the HNSW index (one row per ordered pair).
    pub fn graph_similar(&self, repo: Option<&str>, k: i64, min_sim: f64, cap: i64) -> Vec<GSim> {
        let mut c = Client::connect(&self.url, NoTls).expect("pg");
        let _ = c.batch_execute(
            "set hnsw.ef_search = 40; set hnsw.iterative_scan = relaxed_order; set statement_timeout = '50s'",
        );
        c.query(
            "select a.repo,a.hash,a.path,b.repo,b.hash,b.path,(1-(a.emb<=>b.emb))::float4
             from (select repo,hash,path,emb from ix_chunks where emb is not null
                   and ($1::text is null or repo=$1) limit $4) a
             cross join lateral (select repo,hash,path,emb from ix_chunks x
                   where x.emb is not null and ($1::text is null or x.repo=$1)
                     and not (x.repo=a.repo and x.hash=a.hash)
                   order by x.emb <=> a.emb limit $2) b
             where 1-(a.emb<=>b.emb) >= $3",
            &[&repo, &k, &min_sim, &cap],
        )
        .map_err(|e| eprintln!("{{\"event\":\"graph_similar_error\",\"error\":\"{e}\"}}"))
        .unwrap_or_default()
        .into_iter()
        .map(|r| GSim {
            a: (r.get(0), r.get(1), r.get(2)),
            b: (r.get(3), r.get(4), r.get(5)),
            sim: r.get(6),
        })
        .collect()
    }
}
