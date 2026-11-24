// Live lifecycle test against real Postgres + embed/rerank services.
// Skipped unless MUNINN_TEST_PG_URL (a scratch database!), MUNINN_EMBED_URL and MUNINN_RERANK_URL are set.
use muninn_core::{
    HttpEmbedder, HttpReranker,
    lifecycle::Params,
    memory::{Ctx, MemStore, NewMem},
};

#[test]
fn consolidate_expire_reembed() {
    let (Ok(url), Ok(eu), Ok(ru)) = (
        std::env::var("MUNINN_TEST_PG_URL"),
        std::env::var("MUNINN_EMBED_URL"),
        std::env::var("MUNINN_RERANK_URL"),
    ) else {
        return;
    };
    let st = MemStore::connect(
        &url,
        Box::new(HttpEmbedder { url: eu }),
        Box::new(HttpReranker { url: ru }),
    )
    .unwrap();
    let ctx = Ctx {
        actor: "lc-test".into(),
        scopes: vec!["memory:read".into(), "memory:write".into()],
        projects: vec![],
        run_id: None,
    };
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute("delete from memory where owner='lc-test' and consolidated_into is not null; delete from memory where owner='lc-test'").ok();
    for t in [
        "Run X deployed muninn-indexd to staging with cargo release build on Monday",
        "Deployed muninn-indexd release build on staging after cargo build finished",
        "muninn-indexd was redeployed to staging using a cargo release build",
        "Totally unrelated: lunch menu includes soup on Friday",
    ] {
        let m = NewMem {
            text: t.into(),
            typ: "episodic".into(),
            scope: "agent".into(),
            project_id: None,
            source: None,
            confidence: 0.9,
            importance: 0.5,
            supersedes: None,
        };
        st.remember(&ctx, &m).unwrap();
    }
    db.batch_execute(
        "update memory set valid_from = now() - interval '30 days' where owner='lc-test'",
    )
    .unwrap();
    let p = Params {
        cluster_cosine: 0.75,
        ..Default::default()
    };
    let dry = st
        .consolidate(&Params {
            dry_run: true,
            cluster_cosine: 0.75,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(dry["clusters"], 1, "{dry}");
    let r = st.consolidate(&p).unwrap();
    assert_eq!(r["clusters"], 1, "{r}");
    let n: i64 = db.query_one("select count(*) from memory where owner='lc-test' and type='semantic' and status='active' and emb is not null", &[]).unwrap().get(0);
    let linked: i64 = db.query_one("select count(*) from memory where owner='lc-test' and consolidated_into is not null and status='superseded'", &[]).unwrap().get(0);
    assert_eq!((n, linked), (1, 3));
    db.batch_execute("update memory set valid_until = now() - interval '1 day' where owner='lc-test' and type='episodic' and status='active'").unwrap();
    let x = st.expire_episodic(&p).unwrap();
    assert_eq!(x["expired"], 1, "{x}");
    db.batch_execute("update memory set embedding_version=0 where owner='lc-test'")
        .unwrap();
    let re = st.reembed(&p, 8, 1000).unwrap();
    assert!(re["reembedded"].as_u64().unwrap() >= 5, "{re}");
    let stale: i64 = db
        .query_one(
            "select count(*) from memory where owner='lc-test' and embedding_version<>1",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(stale, 0);
    let rec = st
        .recall(&ctx, "muninn-indexd deploy on staging", None, None, 3)
        .unwrap();
    assert!(
        rec["hits"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Consolidated"),
        "{rec}"
    );
}
