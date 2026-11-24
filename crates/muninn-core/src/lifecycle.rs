//! Memory lifecycle jobs: weekly consolidation, episodic expiry and re-embedding.
//! Runs with the owner connection (bypasses RLS) and writes an audit row per change.
use crate::lifecycle_util::{cosine, parse_vec};
use crate::memory::{MODEL_ID, MemStore};
use crate::pg::DIM;
use serde_json::{Value, json};
use std::collections::HashMap;

type R<T> = Result<T, String>;
fn e<T: std::fmt::Display>(x: T) -> String {
    x.to_string()
}

pub struct Params {
    pub min_age_days: i32,
    pub cluster_cosine: f64,
    pub min_cluster: usize,
    pub dry_run: bool,
    pub embed_version: i32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            min_age_days: 14,
            cluster_cosine: 0.80,
            min_cluster: 3,
            dry_run: false,
            embed_version: std::env::var("MUNINN_EMBED_VERSION")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
        }
    }
}

struct Cand {
    id: String,
    text: String,
    importance: f32,
    emb: Vec<f32>,
    key: (String, String, Option<String>),
}

/// Extractive summary: highest-importance member first, then distinct others, capped.
pub fn summarize(members: &mut [(f32, String)]) -> String {
    members.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out = format!("Consolidated from {} events: ", members.len());
    let mut seen: Vec<String> = Vec::new();
    for (_, t) in members.iter() {
        let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if seen.contains(&one) {
            continue;
        }
        if out.len() + one.len() + 3 > 1500 {
            break;
        }
        if !seen.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&one);
        seen.push(one);
    }
    out
}

/// Greedy clustering of indices by cosine to the cluster's first member.
pub fn cluster(
    embs: &[Vec<f32>],
    keys: &[(String, String, Option<String>)],
    thr: f64,
    min: usize,
) -> Vec<Vec<usize>> {
    let mut used = vec![false; embs.len()];
    let mut out = Vec::new();
    for i in 0..embs.len() {
        if used[i] {
            continue;
        }
        let mut c = vec![i];
        for j in i + 1..embs.len() {
            if !used[j] && keys[j] == keys[i] && cosine(&embs[i], &embs[j]) >= thr {
                c.push(j);
            }
        }
        if c.len() >= min {
            for &x in &c {
                used[x] = true;
            }
            out.push(c);
        }
    }
    out
}

impl MemStore {
    fn audit_sys(
        t: &mut postgres::Transaction,
        action: &str,
        target: &str,
        detail: Value,
    ) -> R<()> {
        t.execute(
            "insert into audit_log(actor,action,target,scopes,detail) values('lifecycle',$1,$2,'{}',$3)",
            &[&action, &target, &detail],
        )
        .map_err(e)?;
        Ok(())
    }

    pub fn consolidate(&self, p: &Params) -> R<Value> {
        let mut db = self.db.lock();
        let rows = db
            .query(
                "select id::text,text,importance,emb::text,owner,scope,project_id from memory
                 where type='episodic' and status='active' and emb is not null and consolidated_into is null
                   and (valid_until is null or valid_until>now()) and valid_from < now() - ($1::int||' days')::interval
                 order by owner,scope,project_id,valid_from limit 2000",
                &[&p.min_age_days],
            )
            .map_err(e)?;
        let cands: Vec<Cand> = rows
            .iter()
            .filter_map(|r| {
                let emb = parse_vec(&r.get::<_, String>(3))?;
                Some(Cand {
                    id: r.get(0),
                    text: r.get(1),
                    importance: r.get(2),
                    emb,
                    key: (r.get(4), r.get(5), r.get(6)),
                })
            })
            .collect();
        let embs: Vec<Vec<f32>> = cands.iter().map(|c| c.emb.clone()).collect();
        let keys: Vec<_> = cands.iter().map(|c| c.key.clone()).collect();
        let clusters = cluster(&embs, &keys, p.cluster_cosine, p.min_cluster);
        let mut made = Vec::new();
        for c in &clusters {
            let mut mem: Vec<(f32, String)> = c
                .iter()
                .map(|&i| (cands[i].importance, cands[i].text.clone()))
                .collect();
            let text = summarize(&mut mem);
            let ids: Vec<String> = c.iter().map(|&i| cands[i].id.clone()).collect();
            let imp = c
                .iter()
                .map(|&i| cands[i].importance)
                .fold(0.0f32, f32::max);
            made.push(json!({"sources": ids, "importance": imp, "text": text.clone()}));
            if p.dry_run {
                continue;
            }
            let k = &cands[c[0]].key;
            let emb = self
                .emb
                .embed(std::slice::from_ref(&text))
                .into_iter()
                .next()
                .filter(|v| v.len() == DIM);
            let lit = emb.as_deref().map(|v| {
                format!(
                    "[{}]",
                    v.iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            });
            let mut t = db.transaction().map_err(e)?;
            let nid: String = t
                .query_one(
                    "insert into memory(type,text,scope,project_id,owner,source,confidence,importance,status,emb,embedding_model,embedding_version)
                     values('semantic',$1,$2,$3,$4,$5,0.7,$6,'active',$7::text::vector,$8,$9) returning id::text",
                    &[&text, &k.1, &k.2, &k.0, &format!("consolidation:{}", ids.len()), &imp, &lit, &MODEL_ID, &p.embed_version],
                )
                .map_err(e)?
                .get(0);
            t.execute(
                "update memory set status='superseded', valid_until=now(), consolidated_into=$2::text::uuid
                 where id::text = any($1)",
                &[&ids, &nid],
            )
            .map_err(e)?;
            Self::audit_sys(&mut t, "consolidate", &nid, json!({"sources": ids}))?;
            t.commit().map_err(e)?;
        }
        Ok(
            json!({"job": "consolidate", "candidates": cands.len(), "clusters": clusters.len(), "dry_run": p.dry_run,
                  "summaries": made.iter().map(|m| json!({"sources": m["sources"].as_array().map(|a| a.len()), "text": m["text"]})).collect::<Vec<_>>()}),
        )
    }

    /// TTL: episodic memories past valid_until leave `active` (rows and links are kept).
    pub fn expire_episodic(&self, p: &Params) -> R<Value> {
        let mut db = self.db.lock();
        let ids: Vec<String> = db
            .query(
                "select id::text from memory where type='episodic' and status='active' and valid_until is not null and valid_until<=now()",
                &[],
            )
            .map_err(e)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if !p.dry_run && !ids.is_empty() {
            let mut t = db.transaction().map_err(e)?;
            t.execute(
                "update memory set status='superseded' where id::text = any($1)",
                &[&ids],
            )
            .map_err(e)?;
            Self::audit_sys(&mut t, "expire", "-", json!({"count": ids.len()}))?;
            t.commit().map_err(e)?;
        }
        Ok(json!({"job": "expire", "expired": ids.len(), "dry_run": p.dry_run}))
    }

    /// Re-embed rows whose model/version differs from the current one, in batches; idempotent and resumable.
    pub fn reembed(&self, p: &Params, batch: usize, max: usize) -> R<Value> {
        let model = std::env::var("MUNINN_EMBED_MODEL").unwrap_or_else(|_| MODEL_ID.into());
        let mut done = 0usize;
        let mut pending: i64;
        loop {
            let rows: Vec<(String, String)> = {
                let mut db = self.db.lock();
                pending = db
                    .query_one(
                        "select count(*) from memory where status<>'forgotten' and (embedding_version<>$1 or embedding_model is distinct from $2)",
                        &[&p.embed_version, &model],
                    )
                    .map_err(e)?
                    .get(0);
                db.query(
                    "select id::text,text from memory where status<>'forgotten' and (embedding_version<>$1 or embedding_model is distinct from $2) limit $3",
                    &[&p.embed_version, &model, &(batch as i64)],
                )
                .map_err(e)?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect()
            };
            if rows.is_empty() || p.dry_run || done >= max {
                break;
            }
            let texts: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
            let vecs = self.emb.embed(&texts);
            if vecs.len() != rows.len() || vecs.iter().any(|v| v.len() != DIM) {
                return Err(format!(
                    "embedder returned unusable vectors after {done} rows"
                ));
            }
            let mut db = self.db.lock();
            let mut t = db.transaction().map_err(e)?;
            for ((id, _), v) in rows.iter().zip(&vecs) {
                let lit = format!(
                    "[{}]",
                    v.iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                t.execute(
                    "update memory set emb=$2::text::vector, embedding_model=$3, embedding_version=$4 where id=$1::text::uuid",
                    &[id, &lit, &model, &p.embed_version],
                )
                .map_err(e)?;
            }
            Self::audit_sys(
                &mut t,
                "reembed",
                "-",
                json!({"rows": rows.len(), "model": model, "version": p.embed_version}),
            )?;
            t.commit().map_err(e)?;
            done += rows.len();
        }
        Ok(
            json!({"job": "reembed", "reembedded": done, "remaining_before_last_batch": pending, "model": model,
                  "version": p.embed_version, "dry_run": p.dry_run}),
        )
    }

    pub fn run_lifecycle(&self, job: &str, p: &Params) -> R<Value> {
        let mut out: HashMap<&str, Value> = HashMap::new();
        if matches!(job, "all" | "consolidate") {
            out.insert("consolidate", self.consolidate(p)?);
        }
        if matches!(job, "all" | "expire") {
            out.insert("expire", self.expire_episodic(p)?);
        }
        if matches!(job, "all" | "reembed") {
            out.insert("reembed", self.reembed(p, 64, 100_000)?);
        }
        if out.is_empty() {
            return Err("job must be all|consolidate|expire|reembed".into());
        }
        Ok(json!(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_by_cosine_and_key() {
        let k = |o: &str| (o.to_string(), "agent".to_string(), None);
        let embs = vec![
            vec![1.0, 0.0],
            vec![0.99, 0.1],
            vec![0.98, 0.15],
            vec![0.0, 1.0],
            vec![1.0, 0.0],
        ];
        let keys = vec![k("a"), k("a"), k("a"), k("a"), k("b")];
        let c = cluster(&embs, &keys, 0.9, 3);
        assert_eq!(c, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn summary_orders_by_importance_and_dedupes() {
        let mut m = vec![
            (0.2, "low  one".to_string()),
            (0.9, "top".to_string()),
            (0.5, "low one".to_string()),
        ];
        let s = summarize(&mut m);
        assert_eq!(s, "Consolidated from 3 events: top | low one");
    }
}
