// Live test of recall's project filter against a scratch Postgres (MUNINN_TEST_PG_URL); skipped
// when unset. Fake embedder and reranker; runs in its own schema, dropped afterwards.
use muninn_core::{
    Embedder, Reranker,
    memory::{Ctx, MemStore, NewMem},
    pg::DIM,
};

const SCHEMA: &str = "recall_project_test";

/// One near-orthogonal vector per memory (keyed on its last word) so none dedupe or conflict.
struct FakeEmb;
impl Embedder for FakeEmb {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; DIM];
                v[0] = 0.3;
                let n = t
                    .bytes()
                    .fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize))
                    % (DIM - 1);
                v[n + 1] = 1.0;
                v
            })
            .collect()
    }
}

struct FakeRr;
impl Reranker for FakeRr {
    fn rerank(&self, _query: &str, docs: &[String]) -> Vec<f32> {
        vec![0.5; docs.len()]
    }
}

#[test]
fn recall_filters_by_project_id() {
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
    let st = MemStore::connect(&scoped, Box::new(FakeEmb), Box::new(FakeRr)).unwrap();
    let ctx = Ctx {
        actor: "rp-test".into(),
        scopes: vec!["memory:read".into(), "memory:write".into()],
        projects: vec!["alpha".into(), "beta".into()],
        run_id: None,
    };
    let mem = |text: &str, scope: &str, project: Option<&str>| NewMem {
        text: text.into(),
        typ: "semantic".into(),
        scope: scope.into(),
        project_id: project.map(String::from),
        source: None,
        confidence: 0.9,
        importance: 0.5,
        supersedes: None,
    };
    for (text, scope, project) in [
        ("zqxproj deploy note alpha", "project", Some("alpha")),
        ("zqxproj deploy note beta", "project", Some("beta")),
        ("zqxproj deploy note global", "global", None),
        ("zqxproj deploy note agentalpha", "agent", Some("alpha")),
    ] {
        let r = st.remember(&ctx, &mem(text, scope, project)).unwrap();
        assert_eq!(r["status"], "active", "{r}");
    }
    let projects = |project: Option<&str>| -> Vec<Option<String>> {
        let rec = st
            .recall(&ctx, "zqxproj deploy", None, project, 10)
            .unwrap();
        let mut p: Vec<Option<String>> = rec["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["project_id"].as_str().map(String::from))
            .collect();
        p.sort();
        p
    };
    assert_eq!(
        projects(None),
        vec![
            None,
            Some("alpha".into()),
            Some("alpha".into()),
            Some("beta".into())
        ]
    );
    assert_eq!(
        projects(Some("alpha")),
        vec![Some("alpha".into()), Some("alpha".into())]
    );
    assert_eq!(projects(Some("beta")), vec![Some("beta".into())]);
    assert!(projects(Some("gamma")).is_empty());
    // A project outside the token's list stays invisible even when asked for by name.
    let narrow = Ctx {
        projects: vec!["beta".into()],
        ..ctx.clone()
    };
    let rec = st
        .recall(
            &narrow,
            "zqxproj deploy",
            Some("project"),
            Some("alpha"),
            10,
        )
        .unwrap();
    assert!(rec["hits"].as_array().unwrap().is_empty(), "{rec}");
    drop(st);
    db.batch_execute(&format!("drop schema {SCHEMA} cascade"))
        .unwrap();
}
