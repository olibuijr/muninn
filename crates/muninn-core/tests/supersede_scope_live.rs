// Live test (scratch database only): superseding one memory must not retire any other active memory.
use muninn_core::{
    HttpEmbedder, HttpReranker,
    memory::{Ctx, MemStore, NewMem},
};

fn mem(text: &str, supersedes: Option<String>) -> NewMem {
    NewMem {
        text: text.into(),
        typ: "semantic".into(),
        scope: "global".into(),
        project_id: None,
        source: None,
        confidence: 0.9,
        importance: 0.5,
        supersedes,
    }
}

#[test]
fn supersede_retires_only_its_target() {
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
        actor: "sup-test".into(),
        scopes: vec!["memory:read".into(), "memory:write".into()],
        projects: vec![],
        run_id: None,
    };
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute("delete from memory where owner='sup-test'")
        .unwrap();
    let id_of = |r: serde_json::Value| r["id"].as_str().unwrap().to_string();
    let old = id_of(
        st.remember(&ctx, &mem("Forge is loopback on port 3210 old note", None))
            .unwrap(),
    );
    let bystander = id_of(
        st.remember(
            &ctx,
            &mem("Postgres for muninn runs on loopback 5433", None),
        )
        .unwrap(),
    );
    let new = id_of(
        st.remember(
            &ctx,
            &mem(
                "Forge loopback is http://127.0.0.1:3210 new note",
                Some(old.clone()),
            ),
        )
        .unwrap(),
    );
    let mut status = |id: &str| -> String {
        db.query_one("select status from memory where id=$1::text::uuid", &[&id])
            .unwrap()
            .get(0)
    };
    assert_eq!(status(&old), "superseded");
    assert_eq!(status(&bystander), "active");
    assert_eq!(status(&new), "active");
    db.batch_execute("delete from memory where owner='sup-test'")
        .ok();
}
