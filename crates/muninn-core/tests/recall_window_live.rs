// Live test of the recall rerank window against a scratch Postgres (MUNINN_TEST_PG_URL); skipped
// when unset. Embedder and reranker are fakes, so no model services are needed. It runs in its own
// schema (dropped afterwards), so the scratch database's `memory` table and its embedding
// dimension are left alone.
use muninn_core::{
    Embedder, Reranker,
    memory::{Ctx, MemStore, NewMem, recall_rerank_window},
    pg::DIM,
};
use std::sync::{Arc, Mutex};

const OWNER: &str = "rw-test";
const SCHEMA: &str = "recall_window_test";
const ITEMS: usize = 16;

/// The query and the decoy share one vector, so the dense list's top hit is the decoy, which has no
/// FTS match: the two lists disagree on their top hit and recall must rerank. Items get near-orthogonal
/// vectors (cosine about 0.08 to each other), far from the dedupe and conflict thresholds.
struct FakeEmb;
impl Embedder for FakeEmb {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; DIM];
                v[0] = 1.0;
                if let Some(n) = t
                    .rsplit_once("item ")
                    .and_then(|(_, n)| n.parse::<usize>().ok())
                {
                    v[0] = 0.3;
                    v[n + 1] = 1.0;
                }
                v
            })
            .collect()
    }
}

/// Records how many documents each rerank call received; scores favour later documents so the
/// rerank visibly reorders the fused list.
struct CountingRr(Arc<Mutex<Vec<usize>>>);
impl Reranker for CountingRr {
    fn rerank(&self, _query: &str, docs: &[String]) -> Vec<f32> {
        self.0.lock().unwrap().push(docs.len());
        (0..docs.len())
            .map(|i| i as f32 / docs.len() as f32)
            .collect()
    }
}

/// The tests share one database; the first MemStore::connect on a fresh one creates roles and
/// policies, which races when two tests connect at once.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn recall_reranks_up_to_twenty_candidates() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(&format!(
        "drop schema if exists {SCHEMA} cascade; create schema {SCHEMA}; grant usage on schema {SCHEMA} to muninn_app"
    ))
    .unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    let scoped = format!("{url}{sep}options=-c%20search_path%3D{SCHEMA}%2Cpublic");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let st = MemStore::connect(
        &scoped,
        Box::new(FakeEmb),
        Box::new(CountingRr(calls.clone())),
    )
    .unwrap();
    let ctx = Ctx {
        actor: OWNER.into(),
        scopes: vec!["memory:read".into(), "memory:write".into()],
        projects: vec![],
        run_id: None,
    };
    let mem = |text: String| NewMem {
        text,
        typ: "semantic".into(),
        scope: "agent".into(),
        project_id: None,
        source: None,
        confidence: 0.9,
        importance: 0.5,
        supersedes: None,
    };
    let r = st
        .remember(&ctx, &mem("decoy note about lunch menus".into()))
        .unwrap();
    assert_eq!(r["status"], "active", "{r}");
    for n in 0..ITEMS {
        let r = st
            .remember(&ctx, &mem(format!("zqxwindow fact number item {n}")))
            .unwrap();
        assert_eq!(r["status"], "active", "{r}");
    }
    let total = ITEMS + 1;
    for k in [3usize, 12, 20] {
        calls.lock().unwrap().clear();
        let rec = st
            .recall(&ctx, "zqxwindow", Some("agent"), None, k)
            .unwrap();
        let hits = rec["hits"].as_array().unwrap();
        assert_eq!(hits.len(), k.min(total), "k={k}: {rec}");
        let want = recall_rerank_window(k).min(total);
        assert_eq!(*calls.lock().unwrap(), vec![want], "k={k}");
        let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
        assert!(scores.windows(2).all(|w| w[0] >= w[1]), "k={k}: {scores:?}");
    }
    drop(st);
    db.batch_execute(&format!("drop schema {SCHEMA} cascade"))
        .unwrap();
}

struct FixedRr(Arc<Mutex<usize>>);
impl Reranker for FixedRr {
    fn rerank(&self, _query: &str, docs: &[String]) -> Vec<f32> {
        *self.0.lock().unwrap() += 1;
        vec![0.25; docs.len()]
    }
}

/// Dense and FTS both rank "item 3" first. Recall must still rerank, so its score is on the
/// reranker's scale (0.25 x weights), not the normalised RRF 1.0 the agree shortcut used to give.
#[test]
fn recall_reranks_when_dense_and_fts_agree() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    const SCHEMA: &str = "recall_agree_test";
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(&format!(
        "drop schema if exists {SCHEMA} cascade; create schema {SCHEMA}; grant usage on schema {SCHEMA} to muninn_app"
    ))
    .unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    let scoped = format!("{url}{sep}options=-c%20search_path%3D{SCHEMA}%2Cpublic");
    let calls = Arc::new(Mutex::new(0));
    let st =
        MemStore::connect(&scoped, Box::new(FakeEmb), Box::new(FixedRr(calls.clone()))).unwrap();
    let ctx = Ctx {
        actor: OWNER.into(),
        scopes: vec!["memory:read".into(), "memory:write".into()],
        projects: vec![],
        run_id: None,
    };
    for n in 0..4 {
        let m = NewMem {
            text: format!("zqxagree fact number item {n}"),
            typ: "semantic".into(),
            scope: "agent".into(),
            project_id: None,
            source: None,
            confidence: 0.9,
            importance: 0.5,
            supersedes: None,
        };
        assert_eq!(st.remember(&ctx, &m).unwrap()["status"], "active");
    }
    let rec = st
        .recall(&ctx, "zqxagree fact number item 3", Some("agent"), None, 3)
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), 1, "{rec}");
    let top = &rec["hits"][0];
    assert_eq!(top["text"], "zqxagree fact number item 3", "{rec}");
    let score = top["score"].as_f64().unwrap();
    assert!(score > 0.2 && score <= 0.25, "{rec}");
    drop(st);
    db.batch_execute(&format!("drop schema {SCHEMA} cascade"))
        .unwrap();
}
