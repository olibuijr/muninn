// Live test (scratch database only, MUNINN_TEST_PG_URL): per-repo dense search over the store,
// with the branch filter applied in memory, through `search_shared`.
use muninn_core::index::search_shared;
use muninn_core::pg::DIM;
use muninn_core::{Change, Embedder, Index, Mode, PgStore, query_text};
use std::sync::{Arc, Mutex};

/// Bag of words hashed into DIM buckets: texts sharing words are near in cosine space.
struct Bag;
impl Embedder for Bag {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; DIM];
                for w in t
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() > 3)
                {
                    let h = w.bytes().fold(7u64, |h, b| h.wrapping_mul(31) ^ b as u64);
                    v[(h % DIM as u64) as usize] += 1.0;
                }
                v
            })
            .collect()
    }
}

fn up(p: &str, t: &str) -> Change {
    Change::Upsert {
        path: p.into(),
        text: t.into(),
    }
}

#[test]
fn small_repo_dense_hits_and_branch_filter_in_memory() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let store = Arc::new(PgStore::connect(&url).unwrap());
    let pid = std::process::id();
    let (big, small) = (format!("dense-big-{pid}"), format!("dense-small-{pid}"));
    let mut b = Index::with_store(&big, store.clone());
    let files: Vec<Change> = (0..300)
        .map(|i| {
            up(
                &format!("f{i}.rs"),
                &format!("fn filler{i}() {{ generic common words number{i} }}\n"),
            )
        })
        .collect();
    b.apply_push("main", files, Some(&Bag));
    let mut s = Index::with_store(&small, store.clone());
    s.apply_push(
        "main",
        vec![up(
            "w.rs",
            "fn wombat_burrow() { marsupial tunnel digging }\n",
        )],
        Some(&Bag),
    );
    s.apply_push(
        "dev",
        vec![up(
            "x.rs",
            "fn wombat_dev() { marsupial tunnel prototype }\n",
        )],
        Some(&Bag),
    );
    let qv = Bag.embed(&[query_text("marsupial tunnel")]).pop().unwrap();
    let (b, s) = (Mutex::new(b), Mutex::new(s));

    let r = search_shared(&s, "marsupial tunnel", None, Mode::Semantic, 5, Some(&qv)).unwrap();
    let paths: Vec<&str> = r.hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(paths.len(), 2, "both branches' chunks: {paths:?}");
    assert!(r.hits.iter().all(|h| h.snippet.contains("marsupial")));
    let main_only = search_shared(&s, "x", Some("main"), Mode::Semantic, 5, Some(&qv)).unwrap();
    assert_eq!(
        main_only
            .hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<Vec<_>>(),
        ["w.rs"],
        "dev-only chunk filtered in memory"
    );

    // The big repo answers from its own rows, not a global top-k shared with the small one.
    let q7 = Bag.embed(&[query_text("number7")]).pop().unwrap();
    let r = search_shared(&b, "number7", None, Mode::Hybrid, 5, Some(&q7)).unwrap();
    assert!(!r.hits.is_empty() && r.hits.iter().all(|h| h.repo == big));
    assert_eq!(r.hits[0].path, "f7.rs");
    assert!(r.timings.dense >= 0.0 && r.timings.lexical >= 0.0);

    // Cached dense results are dropped by a write: a new chunk shows up.
    s.lock().unwrap().apply_push(
        "main",
        vec![up("y.rs", "fn marsupial_tunnel() {}\n")],
        Some(&Bag),
    );
    let r = search_shared(&s, "x", Some("main"), Mode::Semantic, 5, Some(&qv)).unwrap();
    assert!(r.hits.iter().any(|h| h.path == "y.rs"));

    for (ix, n) in [(b, 300), (s, 1)] {
        let mut ix = ix.into_inner().unwrap();
        let dels = (0..n)
            .map(|i| Change::Delete {
                path: format!("f{i}.rs"),
            })
            .chain(["w.rs", "y.rs"].map(|p| Change::Delete { path: p.into() }))
            .collect();
        ix.apply_push("main", dels, None);
        ix.apply_push(
            "dev",
            vec![Change::Delete {
                path: "x.rs".into(),
            }],
            None,
        );
    }
}

/// Latency of `search_shared` on a real repo copied into the scratch database (no embeddings
/// needed for lexical): MUNINN_BENCH_REPO=myrepo cargo test --test dense_live -- --ignored
#[test]
#[ignore]
fn bench_lexical_search() {
    let (Ok(url), Ok(repo)) = (
        std::env::var("MUNINN_TEST_PG_URL"),
        std::env::var("MUNINN_BENCH_REPO"),
    ) else {
        return;
    };
    let store = Arc::new(PgStore::connect(&url).unwrap());
    let t = std::time::Instant::now();
    let ix = Mutex::new(Index::with_store(&repo, store));
    eprintln!(
        "loaded {} chunks in {:?}",
        ix.lock().unwrap().chunk_count(),
        t.elapsed()
    );
    for q in [
        "fn apply_push",
        "the self",
        "fn",
        "search_shared lock",
        "impl Display for",
    ] {
        let _ = search_shared(&ix, q, None, Mode::Lexical, 20, None);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let r = search_shared(&ix, q, None, Mode::Lexical, 20, None).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            assert!(r.hits.len() <= 20);
        }
        eprintln!("{q:?}: best of 5 {best:.2} ms");
    }
}
