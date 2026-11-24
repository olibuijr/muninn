// Live test (scratch database only, MUNINN_TEST_PG_URL): a store-backed index keeps chunk text in
// Postgres only, and still returns full snippets and lexical ranking.
use muninn_core::{Change, Index, Mode, PgStore};
use std::sync::Arc;

#[test]
fn store_backed_index_serves_snippets_from_postgres() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let store = Arc::new(PgStore::connect(&url).unwrap());
    let repo = format!("text-live-{}", std::process::id());
    let text = "fn resident_probe() {\n    // quokka marker\n}\n";
    let mut ix = Index::with_store(&repo, store.clone());
    ix.apply_push(
        "main",
        vec![Change::Upsert {
            path: "a.rs".into(),
            text: text.into(),
        }],
        None,
    );
    assert_eq!(ix.resident_text_bytes(), 0, "chunk text stays in Postgres");
    let hits = ix.search("quokka", None, Mode::Lexical, 5, None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].snippet, text.trim_end());

    // A fresh load (daemon restart) behaves the same.
    let ix2 = Index::with_store(&repo, store.clone());
    assert_eq!(ix2.resident_text_bytes(), 0);
    assert_eq!(
        ix2.search("quokka", None, Mode::Lexical, 5, None, None)[0].snippet,
        text.trim_end()
    );

    ix.apply_push(
        "main",
        vec![Change::Delete {
            path: "a.rs".into(),
        }],
        None,
    );
}

#[test]
fn in_memory_index_keeps_text() {
    let mut ix = Index::new("r");
    ix.apply_push(
        "main",
        vec![Change::Upsert {
            path: "a.rs".into(),
            text: "fn x() {}\n".into(),
        }],
        None,
    );
    assert!(ix.resident_text_bytes() > 0);
}
