use crate::gate::{Lane, with_lane};
use crate::index::{Embedder, Reranker};
use crate::pg::DIM;
use crate::rank::rrf;
use crate::recall_cache::{CacheKey, RecallCache};
use crate::recall_latency::LatencyWindow;
use postgres::{Client, NoTls, Transaction};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const RECALL_CACHE_CAP: usize = 256;
const RECALL_LATENCY_WINDOW: usize = 512;

pub const DEDUPE_COSINE: f64 = 0.97; // plan said 0.92; reranker cannot tell paraphrase from contradiction, see ADR 005
const TAIL_SCORE_SCALE: f64 = 0.01; // unreranked tail ranks below every reranked hit
/// Recall reranks this many fused candidates at least (and at most), widened to k in between.
/// Memories are short, so reranking 20 costs about 70 ms; the old fixed window of 5 left hits 6..k
/// in RRF order with a 0.01 score.
pub const RECALL_RERANK_MIN: usize = 10;
pub const RECALL_RERANK_MAX: usize = 20;

/// Number of fused recall candidates sent to the reranker for a request of `k` hits.
pub fn recall_rerank_window(k: usize) -> usize {
    k.clamp(RECALL_RERANK_MIN, RECALL_RERANK_MAX)
}
pub const DEDUPE_RERANK: f32 = 0.5;
pub const CONFLICT_COSINE: f64 = 0.85;
pub const REVIEW_BELOW: f64 = 0.6;
pub const EPISODIC_TTL_DAYS: i32 = 90;
pub const MODEL_ID: &str = "embeddinggemma-2-q8";

#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub actor: String,
    pub scopes: Vec<String>,
    pub projects: Vec<String>,
    pub run_id: Option<String>,
}

impl Ctx {
    pub fn has(&self, s: &str) -> bool {
        self.scopes.iter().any(|x| x == s)
    }
}

#[derive(Clone, Debug)]
pub struct NewMem {
    pub text: String,
    pub typ: String,
    pub scope: String,
    pub project_id: Option<String>,
    pub source: Option<String>,
    pub confidence: f64,
    pub importance: f64,
    pub supersedes: Option<String>,
}

pub struct MemStore {
    pub(crate) db: crate::pg::Conn,
    pub emb: Box<dyn Embedder>,
    pub rr: Box<dyn Reranker>,
    corpus_version: AtomicU64,
    cache: Mutex<RecallCache>,
    recall_latency: Mutex<LatencyWindow>,
}

type R<T> = Result<T, String>;
fn e<T: std::fmt::Display>(x: T) -> String {
    x.to_string()
}

fn vec_lit(v: &[f32]) -> String {
    let p: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", p.join(","))
}

/// Returns the reason a text must not be stored (secrets), or None.
pub fn secret_reason(t: &str) -> Option<&'static str> {
    let l = t.to_lowercase();
    if t.contains("-----BEGIN") && l.contains("private key") {
        return Some("private key block");
    }
    if t.contains("AKIA")
        && t.split("AKIA").skip(1).any(|r| {
            r.chars()
                .take(16)
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && r.len() >= 16
        })
    {
        return Some("aws access key");
    }
    if t.contains("Bearer ey")
        || t.contains("ghp_")
        || t.contains("sk-ant-")
        || t.contains("glpat-")
    {
        return Some("token");
    }
    for key in ["password", "passwd", "secret", "api_key", "apikey", "token"] {
        for (i, _) in l.match_indices(key) {
            let rest = l[i + key.len()..].trim_start_matches(['"', '\'', ' ']);
            if let Some(v) = rest.strip_prefix(['=', ':']) {
                let v = v.trim_start_matches([' ', '"', '\'']);
                let val: String = v
                    .chars()
                    .take_while(|c| !c.is_whitespace() && !"\"',;)".contains(*c))
                    .collect();
                if val.len() >= 12
                    && val.chars().any(|c| c.is_ascii_digit())
                    && !val.starts_with('<')
                    && !val.starts_with('$')
                {
                    return Some("credential assignment");
                }
            }
        }
    }
    None
}

impl MemStore {
    pub fn connect(url: &str, emb: Box<dyn Embedder>, rr: Box<dyn Reranker>) -> R<Self> {
        let mut c = Client::connect(url, NoTls).map_err(e)?;
        c.batch_execute(&schema_sql()).map_err(e)?;
        Ok(Self {
            db: crate::pg::Conn::new(url, c),
            emb,
            rr,
            corpus_version: AtomicU64::new(0),
            cache: Mutex::new(RecallCache::new(RECALL_CACHE_CAP)),
            recall_latency: Mutex::new(LatencyWindow::new(RECALL_LATENCY_WINDOW)),
        })
    }

    fn txn<T>(&self, ctx: &Ctx, f: impl FnOnce(&mut Transaction) -> R<T>) -> R<T> {
        let mut db = self.db.lock();
        let mut t = db.transaction().map_err(e)?;
        t.execute("set local role muninn_app", &[]).map_err(e)?;
        for (k, v) in [
            ("muninn.scopes", ctx.scopes.join(" ")),
            ("muninn.projects", ctx.projects.join(",")),
            ("muninn.actor", ctx.actor.clone()),
        ] {
            t.execute("select set_config($1,$2,true)", &[&k, &v])
                .map_err(e)?;
        }
        t.execute("set local hnsw.ef_search = 100", &[])
            .map_err(e)?;
        match f(&mut t) {
            Ok(v) => {
                t.commit().map_err(e)?;
                Ok(v)
            }
            Err(x) => {
                let _ = t.rollback();
                Err(x)
            }
        }
    }

    /// Full memory id from a full UUID or a unique prefix of one (recall hints show 8-char ids).
    fn resolve_id(t: &mut Transaction, id: &str) -> R<String> {
        let id = id.trim().to_ascii_lowercase();
        if id.len() < 6 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(format!(
                "invalid memory id `{id}`: give the full UUID or at least 6 leading hex chars"
            ));
        }
        if id.len() == 36 {
            return Ok(id);
        }
        let rows = t
            .query(
                "select id::text from memory where id::text like $1 || '%' limit 2",
                &[&id],
            )
            .map_err(e)?;
        match rows.len() {
            1 => Ok(rows[0].get(0)),
            0 => Err(format!("no memory with id prefix `{id}`")),
            _ => Err(format!(
                "memory id prefix `{id}` is ambiguous; give more characters"
            )),
        }
    }

    fn audit(t: &mut Transaction, ctx: &Ctx, action: &str, target: &str, detail: Value) -> R<()> {
        t.execute(
            "insert into audit_log(actor,run_id,action,target,scopes,detail) values($1,$2,$3,$4,$5,$6)",
            &[&ctx.actor, &ctx.run_id, &action, &target, &ctx.scopes, &detail],
        )
        .map_err(e)?;
        Ok(())
    }

    fn embed_one(&self, text: &str) -> Option<Vec<f32>> {
        let v = with_lane(Lane::Query, || self.emb.embed(&[text.to_string()]));
        v.into_iter().next().filter(|x| x.len() == DIM)
    }

    pub fn remember(&self, ctx: &Ctx, m: &NewMem) -> R<Value> {
        if !ctx.has("memory:write") {
            return Err("forbidden: memory:write required".into());
        }
        if m.text.trim().is_empty() || m.text.len() > 4000 {
            return Err("text must be 1..4000 chars".into());
        }
        if !["episodic", "semantic", "procedural"].contains(&m.typ.as_str()) {
            return Err("type must be episodic|semantic|procedural".into());
        }
        if !["global", "project", "agent"].contains(&m.scope.as_str()) {
            return Err("scope must be global|project|agent".into());
        }
        if m.scope == "project" && m.project_id.is_none() {
            return Err("project scope requires project_id".into());
        }
        if let Some(r) = secret_reason(&m.text) {
            let _ = self.txn(ctx, |t| {
                Self::audit(t, ctx, "remember.rejected", "-", json!({"reason": r}))
            });
            return Err(format!(
                "rejected: text looks like a secret ({r}); store names only"
            ));
        }
        let emb = self.embed_one(&m.text);
        let lit = emb.as_deref().map(vec_lit);
        let stored = self.txn(ctx, |t| {
            let supersedes = match &m.supersedes {
                Some(s) => Some(Self::resolve_id(t, s)?),
                None => None,
            };
            let mut status = "active";
            let mut conflicts: Option<String> = None;
            if let Some(l) = &lit {
                let rows = t
                    .query(
                        "select id::text, text, 1 - (emb <=> $1::text::vector) as cos from memory
                         where status='active' and scope=$2 and project_id is not distinct from $3
                           and (valid_until is null or valid_until>now()) and emb is not null
                         order by emb <=> $1::text::vector limit 5",
                        &[l, &m.scope, &m.project_id],
                    )
                    .map_err(e)?;
                let cands: Vec<(String, String, f64)> =
                    rows.iter().map(|r| (r.get(0), r.get(1), r.get::<_, f64>(2))).collect();
                let close: Vec<&(String, String, f64)> =
                    cands.iter().filter(|c| c.2 >= DEDUPE_COSINE).collect();
                if !close.is_empty() {
                    let docs: Vec<String> = close.iter().map(|c| c.1.clone()).collect();
                    let sc = with_lane(Lane::Query, || self.rr.rerank(&m.text, &docs));
                    let best = sc.iter().cloned().enumerate().max_by(|a, b| a.1.total_cmp(&b.1));
                    // reranker unavailable (MIN) or confirming: cosine>=0.97 alone is enough to dedupe
                    if let Some((i, s)) = best
                        && (s >= DEDUPE_RERANK || s == f32::MIN)
                    {
                        let id = close[i].0.clone();
                        Self::audit(t, ctx, "remember.duplicate", &id, json!({"cosine": close[i].2, "rerank": s}))?;
                        return Ok(json!({"status": "duplicate", "id": id, "cosine": close[i].2}));
                    }
                }
                if m.supersedes.is_none()
                    && m.typ != "episodic"
                    && let Some(c) = cands.iter().find(|c| c.2 >= CONFLICT_COSINE && c.2 < DEDUPE_COSINE)
                {
                    conflicts = Some(c.0.clone());
                }
            }
            if m.confidence < REVIEW_BELOW || conflicts.is_some() {
                status = "review";
            }
            let ttl = (m.typ == "episodic").then_some(EPISODIC_TTL_DAYS);
            let id: String = t
                .query_one(
                    "insert into memory(type,text,scope,project_id,owner,source,confidence,importance,
                        valid_until,supersedes_id,conflicts_with,status,emb,embedding_model,embedding_version)
                     values($1,$2,$3,$4,$5,$6,$7,$8, case when $9::int is null then null else now()+($9::int||' days')::interval end,
                        $10::text::uuid,$11::text::uuid,$12,$13::text::vector,$14,1) returning id::text",
                    &[&m.typ, &m.text, &m.scope, &m.project_id, &ctx.actor, &m.source, &(m.confidence as f32),
                      &(m.importance as f32), &ttl, &supersedes, &conflicts, &status, &lit, &MODEL_ID],
                )
                .map_err(e)?
                .get(0);
            let mut superseded = Value::Null;
            if let Some(old) = &supersedes {
                let n = t
                    .execute(
                        "update memory set status='superseded', valid_until=now() where id=$1::text::uuid and status='active'",
                        &[old],
                    )
                    .map_err(e)?;
                if n == 0 {
                    return Err("supersedes: target not found, not active or not owned by you".into());
                }
                superseded = json!(old);
            }
            Self::audit(t, ctx, "remember", &id, json!({"status": status, "type": m.typ, "scope": m.scope}))?;
            Ok(json!({"status": status, "id": id, "superseded": superseded, "conflicts_with": conflicts,
                      "embedded": lit.is_some()}))
        });
        self.bump_corpus_version();
        stored
    }

    fn bump_corpus_version(&self) {
        self.corpus_version.fetch_add(1, Ordering::SeqCst);
    }

    /// Hybrid recall. `scope` and `project` narrow the result (a project keeps only memories whose
    /// `project_id` equals it); row-level security still decides what the caller may see at all.
    pub fn recall(
        &self,
        ctx: &Ctx,
        query: &str,
        scope: Option<&str>,
        project: Option<&str>,
        k: usize,
    ) -> R<Value> {
        let started = Instant::now();
        let out = self.recall_inner(ctx, query, scope, project, k);
        self.recall_latency
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .record(started.elapsed().as_millis() as u64);
        out
    }

    fn recall_inner(
        &self,
        ctx: &Ctx,
        query: &str,
        scope: Option<&str>,
        project: Option<&str>,
        k: usize,
    ) -> R<Value> {
        if !ctx.has("memory:read") {
            return Err("forbidden: memory:read required".into());
        }
        let k = k.clamp(1, 20);
        let key = CacheKey {
            query: query.to_string(),
            scope: scope.map(String::from),
            project: project.map(String::from),
            k,
            caller: format!(
                "{}|{}|{}",
                ctx.actor,
                ctx.scopes.join(" "),
                ctx.projects.join(",")
            ),
            version: self.corpus_version.load(Ordering::SeqCst),
        };
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key);
        if let Some(v) = cached.and_then(|s| serde_json::from_str(&s).ok()) {
            return Ok(v);
        }
        let out = self.recall_uncached(ctx, query, scope, project, k)?;
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .put(key, out.to_string());
        Ok(out)
    }

    fn recall_uncached(
        &self,
        ctx: &Ctx,
        query: &str,
        scope: Option<&str>,
        project: Option<&str>,
        k: usize,
    ) -> R<Value> {
        let qv = self.embed_one(query).map(|v| vec_lit(&v));
        let hits = self.txn(ctx, |t| {
            let mut lists: Vec<Vec<String>> = Vec::new();
            if let Some(l) = &qv {
                let r = t
                    .query(
                        "select id::text from memory where status='active' and emb is not null
                           and (valid_until is null or valid_until>now()) and ($2::text is null or scope=$2)
                           and ($3::text is null or project_id=$3)
                         order by emb <=> $1::text::vector limit 50",
                        &[l, &scope, &project],
                    )
                    .map_err(e)?;
                lists.push(r.iter().map(|x| x.get(0)).collect());
            }
            let r = t
                .query(
                    "select id::text from memory where status='active' and tsv @@ websearch_to_tsquery('english',$1)
                       and (valid_until is null or valid_until>now()) and ($2::text is null or scope=$2)
                       and ($3::text is null or project_id=$3)
                     order by ts_rank_cd(tsv, websearch_to_tsquery('english',$1)) desc limit 50",
                    &[&query, &scope, &project],
                )
                .map_err(e)?;
            lists.push(r.iter().map(|x| x.get(0)).collect());
            let fused = rrf(&lists);
            let top: Vec<String> = fused.iter().take(RECALL_RERANK_MAX).map(|x| x.0.clone()).collect();
            if top.is_empty() {
                return Ok(vec![]);
            }
            let rows = t
                .query(
                    "select id::text,text,type,scope,project_id,source,importance,confidence,
                            (extract(epoch from now()-valid_from)/86400.0)::float8, valid_from::text
                     from memory where id::text = any($1)",
                    &[&top],
                )
                .map_err(e)?;
            let by: HashMap<String, _> = rows.iter().map(|r| (r.get::<_, String>(0), r)).collect();
            let ordered: Vec<_> = top.iter().filter_map(|i| by.get(i).map(|r| (i.clone(), r))).collect();
            let window = ordered.len().min(recall_rerank_window(k));
            let docs: Vec<String> = ordered[..window].iter().map(|(_, r)| r.get::<_, String>(1)).collect();
            // Always rerank, even when dense and FTS agree on the top hit: the window is small
            // (10-20 short memories, about 70 ms) and a normalised RRF score is on another scale
            // than reranker scores, so thresholds such as MUNINN_HOOK_MIN_SCORE would mean
            // different things per query. RRF stays only as the fallback when the reranker is down.
            let rs = with_lane(Lane::Query, || self.rr.rerank(query, &docs));
            let rrf_max = fused.first().map(|x| x.1).unwrap_or(1.0);
            let rrf_of: HashMap<&str, f64> = fused.iter().map(|x| (x.0.as_str(), x.1)).collect();
            let mut out: Vec<(f64, Value)> = ordered[..window]
                .iter()
                .enumerate()
                .map(|(i, (id, r))| {
                    let rel = if rs[i] == f32::MIN { rrf_of[id.as_str()] / rrf_max } else { rs[i] as f64 };
                    let imp = r.get::<_, f32>(6) as f64;
                    let age: f64 = r.get(8);
                    let decay = (-age / 90.0).exp();
                    let score = rel * (0.7 + 0.2 * imp + 0.1 * decay);
                    (score, json!({"id": id, "text": r.get::<_,String>(1), "type": r.get::<_,String>(2),
                        "scope": r.get::<_,String>(3), "project_id": r.get::<_,Option<String>>(4),
                        "source": r.get::<_,Option<String>>(5), "confidence": r.get::<_,f32>(7),
                        "valid_from": r.get::<_,String>(9), "score": score}))
                })
                .collect();
            out.sort_by(|a, b| b.0.total_cmp(&a.0));
            let tail = ordered[window..].iter().map(|(id, r)| {
                json!({"id": id, "text": r.get::<_,String>(1), "type": r.get::<_,String>(2),
                    "scope": r.get::<_,String>(3), "project_id": r.get::<_,Option<String>>(4),
                    "source": r.get::<_,Option<String>>(5), "confidence": r.get::<_,f32>(7),
                    "valid_from": r.get::<_,String>(9), "score": rrf_of[id.as_str()] / rrf_max * TAIL_SCORE_SCALE})
            });
            Ok(out.into_iter().map(|x| x.1).chain(tail).take(k).collect())
        })?;
        Ok(json!({"hits": hits}))
    }

    pub fn forget(&self, ctx: &Ctx, id: &str) -> R<Value> {
        if !ctx.has("memory:forget") {
            return Err("forbidden: memory:forget required".into());
        }
        let forgotten = self.txn(ctx, |t| {
            let id = Self::resolve_id(t, id)?;
            let id = id.as_str();
            let n = t
                .execute(
                    "update memory set status='forgotten', text='[forgotten]', emb=null, valid_until=now()
                     where id=$1::text::uuid and status<>'forgotten'",
                    &[&id],
                )
                .map_err(e)?;
            if n == 0 {
                return Err("not found, already forgotten, or not owned by you".into());
            }
            Self::audit(t, ctx, "forget", id, json!({}))?;
            Ok(json!({"forgotten": id}))
        });
        self.bump_corpus_version();
        forgotten
    }

    pub fn review_list(&self, ctx: &Ctx) -> R<Value> {
        if !ctx.has("memory:forget") {
            return Err(
                "forbidden: memory:forget required (review queue is owner/board only)".into(),
            );
        }
        self.txn(ctx, |t| {
            let rows = t
                .query(
                    "select id::text,text,type,scope,confidence,conflicts_with::text,source,valid_from::text,
                            (extract(epoch from now()-valid_from)/86400.0)::float8
                     from memory where status='review' and owner=$1 order by valid_from",
                    &[&ctx.actor],
                )
                .map_err(e)?;
            Ok(json!({"review": rows.iter().map(|r| json!({"id": r.get::<_,String>(0), "text": r.get::<_,String>(1),
                "type": r.get::<_,String>(2), "scope": r.get::<_,String>(3), "confidence": r.get::<_,f32>(4),
                "conflicts_with": r.get::<_,Option<String>>(5), "source": r.get::<_,Option<String>>(6),
                "valid_from": r.get::<_,String>(7), "age_days": r.get::<_,f64>(8)})).collect::<Vec<_>>()}))
        })
    }

    /// Count only (no text); any memory:read actor, for dashboards.
    pub fn review_count(&self, ctx: &Ctx) -> R<Value> {
        if !ctx.has("memory:read") {
            return Err("forbidden: memory:read required".into());
        }
        self.txn(ctx, |t| {
            let n: i64 = t
                .query_one("select count(*) from memory where status='review'", &[])
                .map_err(e)?
                .get(0);
            Ok(json!({"pending": n}))
        })
    }

    /// Aggregates only (no text); any memory:read actor, for dashboards.
    pub fn stats(&self, ctx: &Ctx) -> R<Value> {
        if !ctx.has("memory:read") {
            return Err("forbidden: memory:read required".into());
        }
        let (by_type, by_owner, review) = self.txn(ctx, |t| {
            let group = |t: &mut Transaction, col: &str| -> R<Value> {
                let rows = t
                    .query(&format!("select {col}, count(*) from memory where status='active' group by 1 order by 1"), &[])
                    .map_err(e)?;
                Ok(Value::Object(rows.iter().map(|r| (r.get::<_, String>(0), json!(r.get::<_, i64>(1)))).collect()))
            };
            let review: i64 = t
                .query_one("select count(*) from memory where status='review'", &[])
                .map_err(e)?
                .get(0);
            Ok((group(t, "type")?, group(t, "owner")?, review))
        })?;
        let recall = self
            .recall_latency
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .summary()
            .map(|s| json!({"n": s.n, "p50_ms": s.p50_ms, "p95_ms": s.p95_ms}));
        Ok(
            json!({"by_type": by_type, "by_owner": by_owner, "review_pending": review, "recall": recall}),
        )
    }

    /// approve: activate (and supersede conflicts_with if `supersede`); reject: tombstone.
    pub fn review_decide(&self, ctx: &Ctx, id: &str, approve: bool, supersede: bool) -> R<Value> {
        if !ctx.has("memory:forget") {
            return Err("forbidden: memory:forget required".into());
        }
        let decided = self.txn(ctx, |t| {
            let row = t
                .query_opt("select conflicts_with::text from memory where id=$1::text::uuid and status='review' and owner=$2",
                    &[&id, &ctx.actor]).map_err(e)?
                .ok_or("not in review queue")?;
            let old: Option<String> = row.get(0);
            if approve {
                t.execute("update memory set status='active', valid_from=now(), supersedes_id=case when $2 then conflicts_with else supersedes_id end where id=$1::text::uuid", &[&id, &supersede]).map_err(e)?;
                if supersede && let Some(o) = &old {
                    t.execute("update memory set status='superseded', valid_until=now() where id=$1::text::uuid and status='active'", &[o]).map_err(e)?;
                }
            } else {
                t.execute("update memory set status='forgotten', text='[rejected]', emb=null, valid_until=now() where id=$1::text::uuid", &[&id]).map_err(e)?;
            }
            Self::audit(t, ctx, if approve { "review.approve" } else { "review.reject" }, id, json!({"supersede": supersede}))?;
            Ok(json!({"id": id, "approved": approve}))
        });
        self.bump_corpus_version();
        decided
    }

    pub fn ping(&self) -> bool {
        self.db.lock().simple_query("select 1").is_ok()
    }
}

pub fn schema_sql() -> String {
    format!(
        r#"create extension if not exists vector;
create table if not exists memory(
  id uuid primary key default gen_random_uuid(),
  type text not null check (type in ('episodic','semantic','procedural')),
  text text not null,
  scope text not null check (scope in ('global','project','agent')),
  project_id text, owner text not null, source text,
  confidence real not null default 1 check (confidence between 0 and 1),
  importance real not null default 0.5 check (importance between 0 and 1),
  valid_from timestamptz not null default now(), valid_until timestamptz,
  supersedes_id uuid references memory(id), conflicts_with uuid references memory(id),
  status text not null default 'active' check (status in ('active','review','superseded','forgotten')),
  emb vector({DIM}), embedding_model text, embedding_version int not null default 1,
  tsv tsvector generated always as (to_tsvector('english', text)) stored,
  check (scope<>'project' or project_id is not null));
create index if not exists memory_emb on memory using hnsw (emb vector_cosine_ops);
create index if not exists memory_tsv on memory using gin(tsv);
create index if not exists memory_status on memory(status, scope);
create table if not exists audit_log(
  id bigint generated always as identity primary key, ts timestamptz not null default now(),
  actor text not null, run_id text, action text not null, target text, scopes text[] not null, detail jsonb not null default '{{}}');
create or replace function audit_immutable() returns trigger language plpgsql as $$ begin raise exception 'audit_log is append-only'; end $$;
drop trigger if exists audit_no_mod on audit_log;
create trigger audit_no_mod before update or delete on audit_log for each row execute function audit_immutable();
drop trigger if exists audit_no_trunc on audit_log;
create trigger audit_no_trunc before truncate on audit_log for each statement execute function audit_immutable();
do $$ begin if not exists (select 1 from pg_roles where rolname='muninn_app') then create role muninn_app nologin; end if; end $$;
alter table memory enable row level security; alter table memory force row level security;
drop policy if exists mem_read on memory; drop policy if exists mem_write on memory;
drop policy if exists mem_forget on memory; drop policy if exists mem_supersede on memory;
create policy mem_read on memory for select to muninn_app using (
  'memory:read' = any (string_to_array(current_setting('muninn.scopes', true), ' '))
  and (scope='global'
    or (scope='project' and project_id = any (string_to_array(current_setting('muninn.projects', true), ',')))
    or (scope='agent' and owner = current_setting('muninn.actor', true))));
create policy mem_write on memory for insert to muninn_app with check (
  'memory:write' = any (string_to_array(current_setting('muninn.scopes', true), ' ')) and owner = current_setting('muninn.actor', true));
create policy mem_forget on memory for update to muninn_app using (
  'memory:forget' = any (string_to_array(current_setting('muninn.scopes', true), ' ')) and owner = current_setting('muninn.actor', true));
create policy mem_supersede on memory for update to muninn_app using (
  'memory:write' = any (string_to_array(current_setting('muninn.scopes', true), ' ')) and owner = current_setting('muninn.actor', true))
  with check (status in ('active','superseded'));
alter table memory add column if not exists consolidated_into uuid references memory(id);
create index if not exists memory_consolidated on memory(consolidated_into) where consolidated_into is not null;
grant select,insert,update on memory to muninn_app;
grant insert,select on audit_log to muninn_app; grant usage on all sequences in schema public to muninn_app;
"#
    )
}

#[cfg(test)]
mod tests {
    use super::{recall_rerank_window, secret_reason};
    #[test]
    fn recall_reranks_at_least_ten_and_at_most_twenty() {
        assert_eq!(recall_rerank_window(1), 10);
        assert_eq!(recall_rerank_window(3), 10);
        assert_eq!(recall_rerank_window(10), 10);
        assert_eq!(recall_rerank_window(15), 15);
        assert_eq!(recall_rerank_window(20), 20);
        assert_eq!(recall_rerank_window(50), 20);
    }
    #[test]
    fn rejects_secrets_keeps_prose() {
        assert!(secret_reason("db password=hunter2hunter2abc").is_some());
        assert!(secret_reason("-----BEGIN RSA PRIVATE KEY-----").is_some());
        assert!(secret_reason("Authorization: Bearer eyJhbGciOi").is_some());
        assert!(secret_reason("token is stored in PassVault under name muninn/webhook").is_none());
        assert!(secret_reason("set password=<from-passvault>").is_none());
        assert!(secret_reason("Forge is loopback-only").is_none());
    }
}
