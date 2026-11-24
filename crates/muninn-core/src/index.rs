use crate::chunk::{Chunk, chunk_file, sha_hex};
use crate::pg::PgStore;
use crate::rank::{cosine, rrf, tokenize, top_agree};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub enum Change {
    Upsert { path: String, text: String },
    Delete { path: String },
}

/// Pseudo-branch holding uncommitted working-tree edits (fed by muninn-watch).
pub const LIVE: &str = "live";

/// EmbeddingGemma is trained with asymmetric prompts: queries and documents get different
/// prefixes. Both sides must use these for the cosine space to line up; `EMB_VERSION` marks
/// vectors written with the current prompt + header scheme so old ones can be refilled.
pub const QUERY_PROMPT: &str = "task: code retrieval | query: ";
pub const EMB_VERSION: i16 = 1;

/// Text to embed for a query.
pub fn query_text(q: &str) -> String {
    format!("{QUERY_PROMPT}{}", q.trim())
}

/// Text to embed for a chunk: the EmbeddingGemma document prompt with the file path and
/// enclosing symbol as title, so a chunk carries its context into the vector.
pub fn doc_text(path: &str, symbol: Option<&str>, text: &str) -> String {
    match symbol.filter(|s| !s.is_empty()) {
        Some(s) => format!("title: {path} {s} | text: {text}"),
        None => format!("title: {path} | text: {text}"),
    }
}

pub struct Prepared {
    branch: String,
    path: String,
    sha: String,
    hashes: Vec<String>,
    fresh: Vec<Chunk>,
}

impl Prepared {
    /// Embedding inputs (document prompt + path/symbol header) of the chunks that still need a vector.
    pub fn texts(&self) -> Vec<String> {
        self.fresh
            .iter()
            .map(|c| doc_text(&c.path, c.symbol.as_deref(), &c.text))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub repo: String,
    pub branch: String,
    pub path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f64,
    pub snippet: String,
    /// Full chunk text (for reranking); not serialized.
    #[serde(skip)]
    pub text: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    pub indexed: usize,
    pub skipped: usize,
    pub tombstoned: usize,
}

pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>>;
}
pub trait Reranker: Send + Sync {
    fn rerank(&self, query: &str, docs: &[String]) -> Vec<f32>;
}

struct Stored {
    chunk: Chunk,
    vec: Option<Vec<f32>>,
    /// Stable position in `Index::ids`; postings refer to it. Never reused.
    id: u32,
    /// BM25 document length (tokens of text + 3x symbol + path).
    len: u32,
}

const NO_BRANCH: u16 = u16::MAX;

/// The chunks one search may return, resolved once per branch filter and cached until the next
/// mutation: for each chunk id the branch it is shown under (deduplicated across branches).
struct CandSet {
    branch_of: Vec<u16>,
    names: Vec<String>,
    n: usize,
    avg_len: f64,
    /// term id -> document frequency within this set, filled lazily by `bm25_scored`.
    df: Mutex<HashMap<u32, u32>>,
}

impl CandSet {
    fn allowed(&self, id: u32) -> bool {
        self.branch_of
            .get(id as usize)
            .is_some_and(|b| *b != NO_BRANCH)
    }
    fn branch(&self, id: u32) -> &str {
        &self.names[self.branch_of[id as usize] as usize]
    }
}

/// Branch preference when the same chunk exists on several branches: the working tree first,
/// then the default branch, then the rest by name.
fn branch_rank(b: &str) -> (u8, &str) {
    match b {
        LIVE => (0, b),
        "main" => (1, b),
        "master" => (2, b),
        _ => (3, b),
    }
}

#[derive(Default)]
pub struct Index {
    repo: String,
    /// (branch, path) -> (blob sha, chunk hashes)
    files: HashMap<(String, String), (String, Vec<String>)>,
    /// content hash -> chunk; shared across branches and paths with equal content
    chunks: HashMap<String, Stored>,
    /// chunk id -> content hash; None once the chunk was dropped (ids are never reused)
    ids: Vec<Option<Box<str>>>,
    /// chunk id -> BM25 document length, so scoring a posting list needs no hash lookups
    lens: Vec<u32>,
    /// BM25 term dictionary shared by every chunk of this index. Grows only.
    terms: HashMap<Box<str>, u32>,
    /// Inverted index: term id -> (chunk id, term frequency), in insertion order. Entries of
    /// dropped chunks stay until `compact`.
    postings: HashMap<u32, Vec<(u32, u32)>>,
    dead: usize,
    /// Candidate sets per branch filter; cleared on every mutation.
    cands: Mutex<HashMap<Option<String>, Arc<CandSet>>>,
    /// Reusable BM25 accumulator indexed by chunk id; all zero between searches.
    scratch: Mutex<Vec<f64>>,
    /// tombstones: (branch, path) -> blob sha at deletion
    pub tombstones: HashMap<(String, String), String>,
    store: Option<Arc<PgStore>>,
}

impl Index {
    pub fn new(repo: &str) -> Self {
        Self {
            repo: repo.into(),
            ..Default::default()
        }
    }

    /// Index backed by Postgres: state is loaded from it and every change is written through.
    pub fn with_store(repo: &str, store: Arc<PgStore>) -> Self {
        let (files, chunks) = store.load(repo);
        let mut ix = Self::new(repo);
        ix.files = files;
        for c in chunks {
            ix.insert_chunk(c, None, true);
        }
        ix.store = Some(store);
        ix
    }

    /// Tokenizes, interns and posts a chunk; `drop_text` leaves the text to Postgres.
    fn insert_chunk(&mut self, mut chunk: Chunk, vec: Option<Vec<f32>>, drop_text: bool) {
        let mut t = tokenize(&chunk.text);
        for _ in 0..3 {
            t.extend(tokenize(chunk.symbol.as_deref().unwrap_or("")));
        }
        t.extend(tokenize(&chunk.path));
        let len = t.len() as u32;
        let mut tids: Vec<u32> = t
            .into_iter()
            .map(|w| {
                let next = self.terms.len() as u32;
                *self.terms.entry(w.into_boxed_str()).or_insert(next)
            })
            .collect();
        tids.sort_unstable();
        let id = self.ids.len() as u32;
        self.ids.push(Some(chunk.content_hash.as_str().into()));
        self.lens.push(len);
        let mut i = 0;
        while i < tids.len() {
            let t = tids[i];
            let mut n = 1;
            while i + n < tids.len() && tids[i + n] == t {
                n += 1;
            }
            self.postings.entry(t).or_default().push((id, n as u32));
            i += n;
        }
        if drop_text {
            chunk.text = String::new(); // Postgres holds it; see `texts`
        }
        let hash = chunk.content_hash.clone();
        self.chunks.insert(
            hash,
            Stored {
                chunk,
                vec,
                id,
                len,
            },
        );
    }

    fn invalidate(&self) {
        self.cands.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub fn apply_push(
        &mut self,
        branch: &str,
        changes: Vec<Change>,
        emb: Option<&dyn Embedder>,
    ) -> PushStats {
        let mut st = PushStats::default();
        for ch in changes {
            match ch {
                Change::Delete { path } => {
                    if self.delete(branch, &path) {
                        st.tombstoned += 1;
                    }
                }
                Change::Upsert { path, text } => match self.prepare(branch, &path, &text) {
                    None => st.skipped += 1,
                    Some(p) => {
                        let vecs = emb.map(|e| e.embed(&p.texts()));
                        self.commit(p, vecs);
                        st.indexed += 1;
                    }
                },
            }
        }
        self.finish();
        st
    }

    /// Removes `branch:path`, leaving a tombstone. False if it was not indexed.
    pub fn delete(&mut self, branch: &str, path: &str) -> bool {
        let Some((sha, _)) = self.files.remove(&(branch.into(), path.into())) else {
            return false;
        };
        if let Some(s) = &self.store {
            s.del_file(&self.repo, branch, path, &sha);
        }
        self.tombstones.insert((branch.into(), path.into()), sha);
        self.invalidate();
        true
    }

    /// Read-only half of an upsert, so callers can embed without holding the index lock.
    /// None when the file is unchanged.
    pub fn prepare(&self, branch: &str, path: &str, text: &str) -> Option<Prepared> {
        let sha = sha_hex(text.as_bytes());
        if self
            .files
            .get(&(branch.into(), path.into()))
            .is_some_and(|(s, _)| *s == sha)
        {
            return None;
        }
        let new = chunk_file(path, text);
        let hashes = new.iter().map(|c| c.content_hash.clone()).collect();
        let fresh = new
            .into_iter()
            .filter(|c| !self.chunks.contains_key(&c.content_hash))
            .collect();
        Some(Prepared {
            branch: branch.into(),
            path: path.into(),
            sha,
            hashes,
            fresh,
        })
    }

    pub fn commit(&mut self, p: Prepared, vecs: Option<Vec<Vec<f32>>>) {
        let key = (p.branch, p.path);
        self.tombstones.remove(&key);
        let mut batch: Vec<(Chunk, Option<Vec<f32>>)> = Vec::new();
        for (i, c) in p.fresh.into_iter().enumerate() {
            if self.chunks.contains_key(&c.content_hash) {
                continue; // committed by a concurrent upsert since prepare
            }
            let vec = vecs.as_ref().and_then(|v| v.get(i).cloned());
            batch.push((c, vec));
        }
        if let Some(s) = &self.store {
            let rows: Vec<(&Chunk, Option<&[f32]>)> =
                batch.iter().map(|(c, v)| (c, v.as_deref())).collect();
            s.put_chunks(&self.repo, &rows);
        }
        let store_backed = self.store.is_some();
        for (c, vec) in batch {
            self.insert_chunk(c, if store_backed { None } else { vec }, store_backed);
        }
        if let Some(s) = &self.store {
            s.put_file(&self.repo, &key.0, &key.1, &p.sha, &p.hashes);
        }
        self.files.insert(key, (p.sha, p.hashes));
        self.invalidate();
    }

    /// Drops chunks no file references (memory and store). Run once after a push.
    pub fn finish(&mut self) {
        self.gc();
        if let Some(s) = &self.store {
            s.gc(&self.repo);
        }
    }

    /// In-memory part of `finish`; the caller runs the store gc without holding the index lock.
    pub fn finish_memory(&mut self) {
        self.gc();
    }

    /// Whether a committed branch holds `path` (with content `sha`, when given).
    pub fn base_has(&self, path: &str, sha: Option<&str>) -> bool {
        self.files
            .iter()
            .any(|((b, p), (s, _))| b != LIVE && p == path && sha.is_none_or(|x| x == s))
    }

    /// Paths in the live overlay: (path, Some(sha)) for edited files, (path, None) for deleted ones.
    pub fn live_paths(&self) -> Vec<(String, Option<String>)> {
        let mut v: Vec<_> = self
            .files
            .iter()
            .filter(|((b, _), _)| b == LIVE)
            .map(|((_, p), (s, _))| (p.clone(), Some(s.clone())))
            .collect();
        v.extend(
            self.tombstones
                .keys()
                .filter(|(b, _)| b == LIVE)
                .map(|(_, p)| (p.clone(), None)),
        );
        v
    }

    /// Removes `path` from the live overlay (edit reverted or committed); the base branch shows again.
    pub fn live_revert(&mut self, path: &str) {
        let key = (LIVE.to_string(), path.to_string());
        self.tombstones.remove(&key);
        if let Some((sha, _)) = self.files.remove(&key)
            && let Some(s) = &self.store
        {
            s.del_file(&self.repo, LIVE, path, &sha);
            s.del_tombstone(&self.repo, LIVE, path);
        }
        self.invalidate();
    }

    /// Marks `path` deleted in the working tree: hides it from every branch in unfiltered searches.
    pub fn live_delete(&mut self, path: &str) {
        self.live_revert(path);
        self.tombstones
            .insert((LIVE.into(), path.into()), String::new());
        self.invalidate();
    }

    fn gc(&mut self) {
        let live: HashSet<&String> = self.files.values().flat_map(|(_, h)| h).collect();
        let mut dropped = 0;
        self.chunks.retain(|k, _| {
            let keep = live.contains(k);
            if !keep {
                dropped += 1;
            }
            keep
        });
        if dropped > 0 {
            for slot in self.ids.iter_mut() {
                if slot
                    .as_deref()
                    .is_some_and(|h| !self.chunks.contains_key(h))
                {
                    *slot = None;
                }
            }
            self.dead += dropped;
            self.invalidate();
        }
        // Postings of dropped chunks are skipped at query time; rebuild once they are a quarter
        // of the index, so a long run of pushes does not leave the scans dominated by dead ids.
        if self.dead > 1000 && self.dead * 4 > self.ids.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let ids = &self.ids;
        self.postings.retain(|_, v| {
            v.retain(|(id, _)| ids[*id as usize].is_some());
            !v.is_empty()
        });
        self.dead = 0;
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Bytes of chunk text held in memory: zero for a store-backed index.
    pub fn resident_text_bytes(&self) -> usize {
        self.chunks.values().map(|s| s.chunk.text.len()).sum()
    }

    /// Total posting entries (one per distinct term per chunk), live and dead.
    pub fn posting_count(&self) -> usize {
        self.postings.values().map(Vec::len).sum()
    }

    /// Deterministic order for equal scores: file path, then line.
    fn tie_key(&self, id: u32) -> (&str, usize) {
        self.stored(id)
            .map(|s| (s.chunk.path.as_str(), s.chunk.start_line))
            .unwrap_or(("", 0))
    }

    fn stored(&self, id: u32) -> Option<&Stored> {
        self.ids
            .get(id as usize)?
            .as_deref()
            .and_then(|h| self.chunks.get(h))
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Candidate set for a branch filter, built once and cached until the next mutation.
    fn cand_set(&self, branch: Option<&str>) -> Arc<CandSet> {
        let key = branch.map(String::from);
        if let Some(c) = self
            .cands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return c.clone();
        }
        let cs = Arc::new(self.build_cands(branch));
        self.cands
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, cs.clone());
        cs
    }

    fn build_cands(&self, branch: Option<&str>) -> CandSet {
        let mut names: Vec<String> = Vec::new();
        let mut branch_of = vec![NO_BRANCH; self.ids.len()];
        let shadowed = |b: &str, p: &str| {
            b != LIVE && {
                let k = (LIVE.to_string(), p.to_string());
                self.files.contains_key(&k) || self.tombstones.contains_key(&k)
            }
        };
        let mut sum_len = 0u64;
        let mut n = 0usize;
        for ((b, path), (_, hashes)) in &self.files {
            // Unfiltered searches see the live overlay in place of the committed file.
            if branch.is_some_and(|x| x != b) || (branch.is_none() && shadowed(b, path)) {
                continue;
            }
            let bi = match names.iter().position(|x| x == b) {
                Some(i) => i as u16,
                None => {
                    names.push(b.clone());
                    (names.len() - 1) as u16
                }
            };
            for h in hashes {
                let Some(s) = self.chunks.get(h) else {
                    continue;
                };
                let slot = &mut branch_of[s.id as usize];
                if *slot == NO_BRANCH {
                    *slot = bi;
                    sum_len += s.len as u64;
                    n += 1;
                } else if branch_rank(b) < branch_rank(&names[*slot as usize]) {
                    // Same chunk on several branches: show it once, under the preferred branch.
                    *slot = bi;
                }
            }
        }
        CandSet {
            branch_of,
            names,
            n,
            avg_len: (sum_len.max(1) as f64) / (n.max(1) as f64),
            df: Mutex::default(),
        }
    }

    fn bm25(&self, q: &str, cs: &CandSet, n: usize) -> Vec<u32> {
        self.bm25_scored(q, cs, n)
            .into_iter()
            .map(|x| x.0)
            .collect()
    }

    /// Document frequency of `t` within the candidate set (cached on the set).
    fn df(&self, cs: &CandSet, t: u32, p: &[(u32, u32)]) -> u32 {
        let mut cache = cs.df.lock().unwrap_or_else(|p| p.into_inner());
        *cache
            .entry(t)
            .or_insert_with(|| p.iter().filter(|(id, _)| cs.allowed(*id)).count() as u32)
    }

    /// BM25 over the inverted index, restricted to the candidate set, best `n` first (ties by
    /// path, then line). Query terms the index has never seen match nothing.
    ///
    /// Scores are exact; the cost guards only skip work that cannot change the top `n`:
    /// terms are processed rarest first into a dense accumulator, and once the remaining terms
    /// are all "common" (in more than half of the set) and the n-th best partial score already
    /// reaches the most those terms could add (`idf * 2.2` each; the BM25 term weight is
    /// below `k1 + 1`), a chunk none of the rarer terms matched cannot reach the top `n`, so
    /// the common terms only update the chunks already scored (by binary search into their
    /// id-sorted posting lists when that is cheaper than a scan).
    fn bm25_scored(&self, q: &str, cs: &CandSet, n: usize) -> Vec<(u32, f64)> {
        if n == 0 {
            return vec![];
        }
        let mut qt: Vec<(u32, &[(u32, u32)])> = tokenize(q)
            .into_iter()
            .collect::<HashSet<String>>()
            .iter()
            .filter_map(|t| self.terms.get(t.as_str()).copied())
            .filter_map(|t| self.postings.get(&t).map(|p| (t, p.as_slice())))
            .map(|(t, p)| (self.df(cs, t, p), p))
            .filter(|(d, _)| *d > 0)
            .collect();
        if qt.is_empty() {
            return vec![];
        }
        qt.sort_by_key(|(d, p)| (*d, p.len()));
        let nd = cs.n as f64;
        let idf = |d: u32| ((nd - d as f64 + 0.5) / (d as f64 + 0.5) + 1.0).ln();
        let weight = |id: u32, f: u32| {
            let (f, len) = (f as f64, self.lens[id as usize] as f64);
            f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / cs.avg_len))
        };
        let mut guard = self.scratch.try_lock().ok();
        let mut local = Vec::new();
        let acc: &mut Vec<f64> = match guard.as_deref_mut() {
            Some(v) => v,
            None => &mut local,
        };
        if acc.len() < self.ids.len() {
            acc.resize(self.ids.len(), 0.0);
        }
        let mut touched: Vec<u32> = Vec::new();
        let common_from = qt
            .iter()
            .position(|(d, _)| *d as usize * 2 > cs.n)
            .unwrap_or(qt.len());
        let mut only_touched = false;
        for (i, &(d, p)) in qt.iter().enumerate() {
            let w = idf(d);
            if i == common_from && i > 0 && touched.len() >= n {
                let bound: f64 = qt[i..].iter().map(|(d, _)| idf(*d) * 2.2).sum();
                let mut part: Vec<f64> = touched.iter().map(|&id| acc[id as usize]).collect();
                let (_, kth, _) = part.select_nth_unstable_by(n - 1, |a, b| b.total_cmp(a));
                only_touched = *kth >= bound;
            }
            if !only_touched {
                for &(id, f) in p {
                    if !cs.allowed(id) {
                        continue;
                    }
                    let slot = &mut acc[id as usize];
                    if *slot == 0.0 {
                        touched.push(id);
                    }
                    *slot += w * weight(id, f);
                }
            } else if touched.len() * 16 < p.len() {
                touched.sort_unstable();
                let mut rest = p;
                for &id in &touched {
                    let at = rest.partition_point(|(x, _)| *x < id);
                    rest = &rest[at..];
                    if let Some(&(x, f)) = rest.first()
                        && x == id
                    {
                        acc[id as usize] += w * weight(id, f);
                    }
                }
            } else {
                for &(id, f) in p {
                    let slot = &mut acc[id as usize];
                    if *slot > 0.0 {
                        *slot += w * weight(id, f);
                    }
                }
            }
        }
        let scored: Vec<(u32, f64)> = touched
            .iter()
            .map(|&id| (id, std::mem::take(&mut acc[id as usize])))
            .filter(|x| x.1 > 0.0)
            .collect();
        self.top_n(scored, n)
    }

    /// The best `n` of `v` by score, ties by (path, line), sorted. Selects before sorting, so
    /// tie keys (hash lookups) are only computed for the final `n` and the ties at the cut.
    fn top_n(&self, mut v: Vec<(u32, f64)>, n: usize) -> Vec<(u32, f64)> {
        if v.len() > n {
            v.select_nth_unstable_by(n - 1, |a, b| b.1.total_cmp(&a.1));
            let cut = v[n - 1].1;
            let mut top: Vec<(u32, f64)> = v[..n].iter().copied().filter(|x| x.1 > cut).collect();
            // Equal scores may straddle the partition: pick among all of them by tie key.
            let mut ties: Vec<(u32, f64)> = v.iter().copied().filter(|x| x.1 == cut).collect();
            ties.sort_by_cached_key(|x| self.tie_key(x.0));
            ties.truncate(n - top.len());
            top.extend(ties);
            v = top;
        }
        // Stable sort by score over the tie-key order: (score desc, path, line).
        v.sort_by_cached_key(|x| self.tie_key(x.0));
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v
    }

    /// Lexical half of a search plus everything else that needs the index but not Postgres.
    fn stage_lexical(
        &self,
        query: &str,
        branch: Option<&str>,
        mode: Mode,
        qv: Option<&[f32]>,
    ) -> Stage {
        let cs = self.cand_set(branch);
        let lex = if mode != Mode::Semantic {
            self.bm25(query, &cs, 50)
        } else {
            vec![]
        };
        let mem_dense = match (mode, qv, &self.store) {
            (Mode::Lexical, _, _) | (_, None, _) | (_, _, Some(_)) => None,
            (_, Some(qv), None) => {
                let mut s: Vec<(u32, f32)> = self
                    .chunks
                    .values()
                    .filter(|c| cs.allowed(c.id))
                    .filter_map(|c| c.vec.as_ref().map(|v| (c.id, cosine(qv, v))))
                    .collect();
                s.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                Some(s.into_iter().take(50).map(|x| x.0).collect())
            }
        };
        Stage {
            cs,
            lex,
            mem_dense,
            store: self.store.clone(),
            repo: self.repo.clone(),
        }
    }

    /// Fuses the lexical and dense lists into the best `k` hits (text filled in only without a
    /// store; see `fill_texts`). The flag says whether both lists put the same chunk first,
    /// which is then the first hit.
    fn stage_fuse(&self, st: &Stage, dense_hashes: &[String], k: usize) -> (Vec<Pending>, bool) {
        let cs = &st.cs;
        let dense: Vec<u32> = match &st.mem_dense {
            Some(d) => d.clone(),
            None => dense_hashes
                .iter()
                .filter_map(|h| self.chunks.get(h.as_str()))
                .filter(|s| cs.allowed(s.id))
                .map(|s| s.id)
                .take(50)
                .collect(),
        };
        let to_ids = |v: &[u32]| v.iter().map(|i| i.to_string()).collect::<Vec<_>>();
        let (lex_s, dense_s) = (to_ids(&st.lex), to_ids(&dense));
        let agree = top_agree(&dense_s, &lex_s);
        let mut fused: Vec<(u32, f64)> = rrf(&[lex_s, dense_s])
            .into_iter()
            .filter_map(|(id, s)| id.parse().ok().map(|id| (id, s)))
            .collect();
        fused.truncate(k);
        let out = fused
            .iter()
            .filter_map(|(id, score)| self.stored(*id).map(|s| (*id, *score, s)))
            .map(|(id, score, s)| Pending {
                hash: s.chunk.content_hash.clone(),
                hit: Hit {
                    repo: self.repo.clone(),
                    branch: cs.branch(id).into(),
                    path: s.chunk.path.clone(),
                    symbol: s.chunk.symbol.clone(),
                    kind: s.chunk.kind.clone(),
                    start_line: s.chunk.start_line,
                    end_line: s.chunk.end_line,
                    score,
                    snippet: String::new(),
                    text: if self.store.is_some() {
                        String::new()
                    } else {
                        s.chunk.text.clone()
                    },
                },
            })
            .collect();
        (out, agree)
    }

    /// Search of one index. Used by tests and single-threaded callers; the daemon uses
    /// `search_shared`, which does not hold the index lock across Postgres round trips.
    pub fn search(
        &self,
        query: &str,
        branch: Option<&str>,
        mode: Mode,
        k: usize,
        emb: Option<&dyn Embedder>,
        rr: Option<&dyn Reranker>,
    ) -> Vec<Hit> {
        let qv = match (mode, emb) {
            (Mode::Lexical, _) | (_, None) => None,
            (_, Some(e)) => Some(e.embed(&[query_text(query)]).pop().unwrap_or_default()),
        };
        let st = self.stage_lexical(query, branch, mode, qv.as_deref());
        let dense = st.dense(branch, mode, qv.as_deref());
        let (pending, _) = self.stage_fuse(&st, &dense, if rr.is_some() { 20 } else { k });
        let mut hits = fill_texts(&st, pending);
        if let Some(r) = rr {
            let docs: Vec<String> = hits.iter().map(|h| h.text.clone()).collect();
            for (h, rs) in hits.iter_mut().zip(r.rerank(query, &docs)) {
                h.score = rs as f64;
            }
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        }
        hits.truncate(k);
        hits
    }
}

/// State carried from the locked lexical stage to the unlocked dense query and back.
struct Stage {
    cs: Arc<CandSet>,
    lex: Vec<u32>,
    /// Dense ranking computed in memory (index without a store).
    mem_dense: Option<Vec<u32>>,
    store: Option<Arc<PgStore>>,
    repo: String,
}

impl Stage {
    /// Nearest chunk hashes from Postgres; empty without a store or query vector.
    fn dense(&self, branch: Option<&str>, mode: Mode, qv: Option<&[f32]>) -> Vec<String> {
        match (&self.store, qv) {
            (Some(st), Some(q)) if mode != Mode::Lexical => st.dense(&self.repo, branch, q, 50),
            _ => vec![],
        }
    }
}

struct Pending {
    hit: Hit,
    hash: String,
}

/// Fills hit texts from Postgres (one round trip) and derives the 12-line snippets.
fn fill_texts(st: &Stage, pending: Vec<Pending>) -> Vec<Hit> {
    let by_hash = match &st.store {
        Some(s) if !pending.is_empty() => {
            let hashes: Vec<&str> = pending.iter().map(|p| p.hash.as_str()).collect();
            s.texts(&st.repo, &hashes)
        }
        _ => HashMap::new(),
    };
    pending
        .into_iter()
        .map(|p| {
            let mut h = p.hit;
            if st.store.is_some() {
                h.text = by_hash.get(&p.hash).cloned().unwrap_or_default();
            }
            h.snippet = h.text.lines().take(12).collect::<Vec<_>>().join("\n");
            h
        })
        .collect()
}

/// Wall time of one repo's search stages, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Timings {
    /// Candidate set + BM25 (+ in-memory dense), including the wait for the index lock.
    pub lexical: f64,
    /// Postgres HNSW query.
    pub dense: f64,
    /// RRF fusion and hit assembly, including the second wait for the index lock.
    pub fuse: f64,
    /// Chunk text round trip.
    pub texts: f64,
}

/// One repo's search result from `search_shared`.
#[derive(Debug, Default)]
pub struct RepoHits {
    pub hits: Vec<Hit>,
    pub timings: Timings,
    /// Lexical and dense rankings put the same chunk first (it is `hits[0]` unless `k` was 0).
    pub top_agree: bool,
}

/// The index lock is poisoned (an ingest panicked while holding it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Search of a shared index that holds its lock only for the in-memory stages: lock, candidate
/// set + BM25, unlock; Postgres dense query; lock, fuse + assemble hits, unlock; chunk texts.
/// Ingest and other searches of the repo interleave with the Postgres round trips. Chunk ids
/// are never reused, so a chunk dropped in between is simply left out of the hits.
/// `qv` is the query embedding (embedded once by the caller for every repo).
pub fn search_shared(
    ix: &Mutex<Index>,
    query: &str,
    branch: Option<&str>,
    mode: Mode,
    k: usize,
    qv: Option<&[f32]>,
) -> Result<RepoHits, Busy> {
    let t = Instant::now();
    let st = ix
        .lock()
        .map_err(|_| Busy)?
        .stage_lexical(query, branch, mode, qv);
    let lexical = ms_since(t);
    let t = Instant::now();
    let dense_hashes = st.dense(branch, mode, qv);
    let dense = ms_since(t);
    let t = Instant::now();
    let (pending, top_agree) = ix
        .lock()
        .map_err(|_| Busy)?
        .stage_fuse(&st, &dense_hashes, k);
    let fuse = ms_since(t);
    let t = Instant::now();
    let hits = fill_texts(&st, pending);
    Ok(RepoHits {
        hits,
        timings: Timings {
            lexical,
            dense,
            fuse,
            texts: ms_since(t),
        },
        top_agree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn up(p: &str, t: &str) -> Change {
        Change::Upsert {
            path: p.into(),
            text: t.into(),
        }
    }

    #[test]
    fn incremental_skip_and_tombstone() {
        let mut ix = Index::new("r");
        let s = ix.apply_push(
            "main",
            vec![up("a.rs", "fn alpha() {}\n"), up("b.rs", "fn beta() {}\n")],
            None,
        );
        assert_eq!(
            s,
            PushStats {
                indexed: 2,
                skipped: 0,
                tombstoned: 0
            }
        );
        let s = ix.apply_push(
            "main",
            vec![
                up("a.rs", "fn alpha() {}\n"),
                Change::Delete {
                    path: "b.rs".into(),
                },
            ],
            None,
        );
        assert_eq!(
            s,
            PushStats {
                indexed: 0,
                skipped: 1,
                tombstoned: 1
            }
        );
        assert!(
            ix.search("beta", None, Mode::Lexical, 5, None, None)
                .is_empty()
        );
        assert_eq!(
            ix.search("alpha", None, Mode::Lexical, 5, None, None)[0]
                .symbol
                .as_deref(),
            Some("alpha")
        );
        assert_eq!(ix.chunk_count(), 1);
    }

    #[test]
    fn branches_share_chunks_and_unfiltered_search_dedups_them() {
        let mut ix = Index::new("r");
        ix.apply_push("dev", vec![up("a.rs", "fn same() {}\n")], None);
        ix.apply_push("main", vec![up("a.rs", "fn same() {}\n")], None);
        assert_eq!(ix.chunk_count(), 1);
        assert_eq!(
            ix.search("same", Some("dev"), Mode::Lexical, 5, None, None)
                .len(),
            1
        );
        let all = ix.search("same", None, Mode::Lexical, 5, None, None);
        assert_eq!(all.len(), 1, "one hit per chunk, not per branch");
        assert_eq!(all[0].branch, "main", "default branch preferred");
    }

    #[test]
    fn live_overlay_shadows_committed_files() {
        let mut ix = Index::new("r");
        ix.apply_push(
            "main",
            vec![
                up("a.rs", "fn alphaword() {}\n"),
                up("b.rs", "fn betaword() {}\n"),
            ],
            None,
        );
        let p = ix.prepare(LIVE, "a.rs", "fn omegaword() {}\n").unwrap();
        ix.commit(p, None);
        ix.live_delete("b.rs");
        let names = |ix: &Index, q| ix.search(q, None, Mode::Lexical, 5, None, None).len();
        assert_eq!(
            names(&ix, "alphaword"),
            0,
            "edited file shows its live text"
        );
        assert_eq!(names(&ix, "omegaword"), 1);
        assert_eq!(names(&ix, "betaword"), 0, "deleted in the working tree");
        assert_eq!(
            ix.search("alphaword", Some("main"), Mode::Lexical, 5, None, None)
                .len(),
            1
        );
        assert_eq!(ix.live_paths().len(), 2);
        ix.live_revert("a.rs");
        ix.live_revert("b.rs");
        assert_eq!(
            (
                names(&ix, "alphaword"),
                names(&ix, "omegaword"),
                names(&ix, "betaword")
            ),
            (1, 0, 1)
        );
        assert!(ix.base_has("a.rs", None) && !ix.base_has("c.rs", None));
    }

    /// The pre-inverted-index BM25: per-chunk HashMap<String, f64> term frequencies.
    fn reference_bm25(q: &str, docs: &[(String, Option<String>, String)]) -> Vec<(String, f64)> {
        let tfs: Vec<(HashMap<String, f64>, f64)> = docs
            .iter()
            .map(|(path, sym, text)| {
                let mut t = tokenize(text);
                for _ in 0..3 {
                    t.extend(tokenize(sym.as_deref().unwrap_or("")));
                }
                t.extend(tokenize(path));
                let mut tf: HashMap<String, f64> = HashMap::new();
                for w in &t {
                    *tf.entry(w.clone()).or_default() += 1.0;
                }
                (tf, t.len() as f64)
            })
            .collect();
        let qt: HashSet<String> = tokenize(q).into_iter().collect();
        let nd = tfs.len() as f64;
        let avg = tfs.iter().map(|x| x.1).sum::<f64>().max(1.0) / nd;
        let mut out = vec![];
        for (i, (tf, len)) in tfs.iter().enumerate() {
            let mut sc = 0.0;
            for t in &qt {
                if let Some(f) = tf.get(t) {
                    let d = tfs.iter().filter(|x| x.0.contains_key(t)).count() as f64;
                    let idf = ((nd - d + 0.5) / (d + 0.5) + 1.0).ln();
                    sc += idf * f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / avg));
                }
            }
            if sc > 0.0 {
                out.push((docs[i].0.clone(), sc));
            }
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    #[test]
    fn inverted_bm25_scores_match_reference() {
        let files = [
            (
                "src/cache.rs",
                "fn evict_cache() { /* cache cache disk */ }\n",
            ),
            ("src/disk.rs", "fn read_disk() { let diskCache = 1; }\n"),
            ("src/auth.rs", "fn check_auth() { /* login token */ }\n"),
            ("docs/cache.md", "# Cache\n\nThe cache keeps hot chunks.\n"),
        ];
        let mut ix = Index::new("r");
        ix.apply_push("main", files.iter().map(|(p, t)| up(p, t)).collect(), None);
        let cs = ix.cand_set(None);
        let docs: Vec<_> = ix
            .chunks
            .values()
            .map(|s| {
                (
                    s.chunk.path.clone(),
                    s.chunk.symbol.clone(),
                    s.chunk.text.clone(),
                )
            })
            .collect();
        for q in [
            "cache disk",
            "diskCache",
            "login",
            "hot chunks cache",
            "nothing",
        ] {
            let want = reference_bm25(q, &docs);
            let got: Vec<(String, f64)> = ix
                .bm25_scored(q, &cs, 50)
                .into_iter()
                .map(|(i, s)| (ix.stored(i).unwrap().chunk.path.clone(), s))
                .collect();
            assert_eq!(got.len(), want.len(), "{q}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g.0, w.0, "{q}");
                assert!((g.1 - w.1).abs() < 1e-12, "{q}: {} vs {}", g.1, w.1);
            }
        }
    }

    /// Top-n selection, tie order and the common-term pruning (both the binary-search and the
    /// scan path) must return exactly the reference's first n, also on a branch-filtered set.
    #[test]
    fn bm25_top_n_with_common_terms_matches_reference() {
        let text = |i: usize| {
            format!(
                "fn f{i}() {{ common {} {} {} {} }}\n",
                if i % 5 < 3 { "mid" } else { "" },
                if i.is_multiple_of(13) { "low" } else { "" },
                if i.is_multiple_of(80) { "rare" } else { "" },
                "pad ".repeat(i % 7),
            )
        };
        let mut ix = Index::new("r");
        let main: Vec<Change> = (0..400)
            .map(|i| up(&format!("f{i:03}.rs"), &text(i)))
            .collect();
        ix.apply_push("main", main, None);
        let dev: Vec<Change> = (0..400)
            .step_by(3)
            .map(|i| up(&format!("d{i:03}.rs"), &format!("{} extra\n", text(i))))
            .collect();
        ix.apply_push("dev", dev, None);
        for branch in [Some("main"), Some("dev"), None] {
            let cs = ix.cand_set(branch);
            let docs: Vec<_> = ix
                .chunks
                .values()
                .filter(|s| cs.allowed(s.id))
                .map(|s| {
                    (
                        s.chunk.path.clone(),
                        s.chunk.symbol.clone(),
                        s.chunk.text.clone(),
                    )
                })
                .collect();
            for (q, n) in [
                ("rare common", 2),
                ("rare mid common", 3),
                ("low common", 5),
                ("low mid", 10),
                ("common", 7),
                ("mid", 50),
                ("pad common", 20),
                ("rare low mid common pad", 400),
            ] {
                let want = reference_bm25(q, &docs);
                let got: Vec<(String, f64)> = ix
                    .bm25_scored(q, &cs, n)
                    .into_iter()
                    .map(|(i, s)| (ix.stored(i).unwrap().chunk.path.clone(), s))
                    .collect();
                assert_eq!(got.len(), want.len().min(n), "{branch:?} {q}");
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.0, w.0, "{branch:?} {q}");
                    assert!((g.1 - w.1).abs() < 1e-9, "{branch:?} {q}");
                }
            }
        }
        assert!(
            ix.scratch.lock().unwrap().iter().all(|x| *x == 0.0),
            "accumulator left zeroed"
        );
    }

    #[test]
    fn terms_are_interned_once_per_index() {
        let mut ix = Index::new("r");
        ix.apply_push(
            "main",
            vec![
                up("a.rs", "fn shared_word() {}\n"),
                up("b.rs", "fn shared_word2() { shared }\n"),
            ],
            None,
        );
        let mut distinct = HashSet::new();
        for s in ix.chunks.values() {
            let mut t = tokenize(&s.chunk.text);
            t.extend(tokenize(s.chunk.symbol.as_deref().unwrap_or("")));
            t.extend(tokenize(&s.chunk.path));
            distinct.extend(t);
        }
        assert_eq!(ix.term_count(), distinct.len());
    }

    #[test]
    fn dropped_chunks_leave_search_and_postings_compact() {
        let mut ix = Index::new("r");
        let files: Vec<Change> = (0..3000)
            .map(|i| up(&format!("f{i}.rs"), &format!("fn word{i}() {{ common }}\n")))
            .collect();
        ix.apply_push("main", files, None);
        let before = ix.posting_count();
        let dels: Vec<Change> = (0..2000)
            .map(|i| Change::Delete {
                path: format!("f{i}.rs"),
            })
            .collect();
        ix.apply_push("main", dels, None);
        assert!(
            ix.search("word5", None, Mode::Lexical, 5, None, None)
                .is_empty()
        );
        assert_eq!(
            ix.search("word2999", None, Mode::Lexical, 5, None, None)
                .len(),
            1
        );
        assert!(ix.posting_count() < before / 2, "postings compacted");
        assert_eq!(
            ix.search("common", None, Mode::Lexical, 50, None, None)
                .len(),
            50
        );
    }

    struct Bag;
    impl Embedder for Bag {
        fn embed(&self, t: &[String]) -> Vec<Vec<f32>> {
            t.iter()
                .map(|s| {
                    let mut v = vec![0.0; 4];
                    let l = s.to_lowercase();
                    for (i, w) in ["auth", "login", "cache", "disk"].iter().enumerate() {
                        if l.contains(w) {
                            v[i] = 1.0;
                        }
                    }
                    v
                })
                .collect()
        }
    }

    #[test]
    fn hybrid_uses_dense() {
        let mut ix = Index::new("r");
        ix.apply_push(
            "main",
            vec![
                up("a.rs", "fn check_auth() { /* login */ }\n"),
                up("b.rs", "fn evict() { /* disk cache */ }\n"),
            ],
            Some(&Bag),
        );
        let h = ix.search("cache disk", None, Mode::Hybrid, 2, Some(&Bag), None);
        assert_eq!(h[0].path, "b.rs");
        assert!(
            h[0].text.contains("evict"),
            "full text carried for reranking"
        );
    }

    #[test]
    fn search_shared_matches_search_and_reports_agreement() {
        let mut ix = Index::new("r");
        ix.apply_push(
            "main",
            vec![
                up("a.rs", "fn check_auth() { /* login */ }\n"),
                up("b.rs", "fn evict() { /* disk cache */ }\n"),
            ],
            Some(&Bag),
        );
        let qv = Bag.embed(&[query_text("cache disk")]).pop().unwrap();
        let want = ix.search("cache disk", None, Mode::Hybrid, 2, Some(&Bag), None);
        let ix = Mutex::new(ix);
        let r = search_shared(&ix, "cache disk", None, Mode::Hybrid, 2, Some(&qv)).unwrap();
        let paths = |h: &[Hit]| h.iter().map(|h| h.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&r.hits), paths(&want));
        assert_eq!(r.hits[0].path, "b.rs");
        assert!(r.top_agree, "lexical and dense both rank b.rs first");
        assert!(r.hits[0].snippet.contains("evict"));
        let r = search_shared(&ix, "login", None, Mode::Lexical, 2, Some(&qv)).unwrap();
        assert!(!r.top_agree, "no dense list in lexical mode");
        assert_eq!(r.hits[0].path, "a.rs");
    }

    #[test]
    fn embedding_texts_carry_prompts_and_headers() {
        assert_eq!(query_text(" x "), "task: code retrieval | query: x");
        assert_eq!(
            doc_text("src/a.rs", Some("alpha"), "fn alpha() {}"),
            "title: src/a.rs alpha | text: fn alpha() {}"
        );
        assert_eq!(
            doc_text("README.md", None, "hi"),
            "title: README.md | text: hi"
        );
        let ix = Index::new("r");
        let p = ix.prepare("main", "a.rs", "fn alpha() {}\n").unwrap();
        assert!(p.texts()[0].starts_with("title: a.rs alpha | text: "));
    }
}
