// Live test (scratch database only, MUNINN_TEST_PG_URL): a tiny SCIP def/ref/call graph exercises
// symbol_explain, symbols_for_hits, word-aware symbol_search and refs grouped by file; a chunk-only
// repo exercises the tree-sitter fallback.
use muninn_core::PgStore;
use muninn_core::scip::{Call, Def, Graph, Ref};
use muninn_core::symbols::HitSpan;

fn def(sym: &str, name: &str, kind: &str, path: &str, s: i32, e: i32) -> Def {
    Def {
        symbol: sym.into(),
        name: name.into(),
        kind: kind.into(),
        path: path.into(),
        start_line: s,
        end_line: e,
        doc: Some(format!("{name} docs")),
    }
}

pub fn graph() -> Graph {
    let r = |sym: &str, path: &str, line: i32| Ref {
        symbol: sym.into(),
        path: path.into(),
        line,
        col: 4,
    };
    let c = |a: &str, b: &str, path: &str, line: i32| Call {
        caller: a.into(),
        callee: b.into(),
        path: path.into(),
        line,
    };
    Graph {
        defs: vec![
            def("t m main().", "main", "function", "src/main.rs", 1, 10),
            def("t m helper().", "helper", "function", "src/main.rs", 12, 20),
            def("t m helper().x.", "x", "variable", "src/main.rs", 13, 13),
            def(
                "t m parseHttpRequest().",
                "parseHttpRequest",
                "function",
                "src/http.rs",
                1,
                30,
            ),
            def("t m Config#", "Config", "struct", "src/http.rs", 32, 40),
        ],
        refs: vec![
            r("t m helper().", "src/main.rs", 3),
            r("t m helper().", "src/main.rs", 5),
            r("t m helper().", "src/main.rs", 7),
            r("t m helper().", "src/http.rs", 9),
        ],
        calls: vec![
            c("t m main().", "t m helper().", "src/main.rs", 3),
            c("t m main().", "t m helper().", "src/main.rs", 5),
            c("t m parseHttpRequest().", "t m helper().", "src/http.rs", 9),
            c(
                "t m helper().",
                "rust-analyzer cargo core 1.0 ops/add().",
                "src/main.rs",
                14,
            ),
        ],
    }
}

/// One store per test binary: concurrent PgStore::connect calls run the schema DDL at the same
/// time and can deadlock.
fn store(url: &str) -> &'static PgStore {
    static S: std::sync::OnceLock<PgStore> = std::sync::OnceLock::new();
    S.get_or_init(|| PgStore::connect(url).unwrap())
}

fn cleanup(url: &str, repo: &str) {
    let mut c = postgres::Client::connect(url, postgres::NoTls).unwrap();
    for t in [
        "ix_sym_defs",
        "ix_sym_refs",
        "ix_sym_calls",
        "ix_sym_meta",
        "ix_chunks",
    ] {
        c.execute(&format!("delete from {t} where repo=$1"), &[&repo])
            .unwrap();
    }
}

#[test]
fn explain_hits_search_and_grouped_refs() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let store = store(&url);
    let repo = format!("sym-live-{}", std::process::id());
    store.replace_symbols(&repo, "abc1234", &graph()).unwrap();

    let v = store
        .symbol_explain("helper", Some(&repo), 10, &|_| true)
        .unwrap();
    assert_eq!(v["source"], "scip");
    let d = &v["definitions"][0];
    assert_eq!(v["definitions"].as_array().unwrap().len(), 1);
    assert_eq!(
        (d["path"].as_str(), d["start_line"].as_i64()),
        (Some("src/main.rs"), Some(12))
    );
    assert_eq!(d["commit"], "abc1234");
    assert_eq!(d["doc"], "helper docs");
    assert_eq!(d["references"]["count"], 4);
    assert_eq!(d["references"]["file_count"], 2);
    assert_eq!(d["references"]["files"][0]["path"], "src/main.rs");
    assert_eq!(d["references"]["files"][0]["count"], 3);
    assert_eq!(d["callers"]["distinct"], 2);
    assert_eq!(d["callers"]["call_sites"], 3);
    assert_eq!(d["callers"]["top"][0]["name"], "main");
    assert_eq!(d["callers"]["top"][0]["calls"], 2);
    assert_eq!(d["callers"]["top"][0]["first_call"]["line"], 3);
    assert_eq!(d["callees"]["top"][0]["name"], "add");
    assert_eq!(d["callees"]["top"][0]["external"], true);
    // A hidden repo is skipped and noted.
    let v = store
        .symbol_explain("helper", Some(&repo), 10, &|_| false)
        .unwrap();
    assert!(v["note"].as_str().unwrap().contains("no longer hosted"));
    // Full SCIP symbols work too.
    let v = store
        .symbol_explain("t m Config#", None, 10, &|_| true)
        .unwrap();
    assert_eq!(v["definitions"][0]["name"], "Config");

    let span = |path: &str, s, e| HitSpan {
        repo: repo.clone(),
        path: path.into(),
        start_line: s,
        end_line: e,
    };
    let h = store
        .symbols_for_hits(&[
            span("src/main.rs", 11, 21),
            span("src/nope.rs", 1, 5),
            span("src/http.rs", 30, 41),
            HitSpan {
                repo: "no-such-repo".into(),
                ..span("src/main.rs", 1, 10)
            },
        ])
        .unwrap();
    assert_eq!(h[0]["name"], "helper", "variables are skipped");
    assert_eq!(
        (h[0]["callers"].as_i64(), h[0]["callees"].as_i64()),
        (Some(2), Some(1))
    );
    assert!(h[1].is_null());
    assert_eq!(h[2]["name"], "Config");
    assert!(h[3].is_null());

    let hits = store.symbol_search("http request", 200);
    let mine: Vec<_> = hits.iter().filter(|x| x["repo"] == repo.as_str()).collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["name"], "parseHttpRequest");
    assert_eq!(mine[0]["match"], "words");

    let g = store
        .symbol_lookup_opts("helper", "refs", Some(&repo), 50, true)
        .unwrap();
    assert_eq!(g["total_references"], 4);
    assert_eq!(g["total_files"], 2);
    assert_eq!(g["files"][0]["lines"], serde_json::json!([3, 5, 7]));
    cleanup(&url, &repo);
}

#[test]
fn explain_falls_back_to_tree_sitter_chunks() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let store = store(&url);
    let repo = format!("sym-live-ts-{}", std::process::id());
    let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    c.execute(
        "insert into ix_chunks(repo,hash,path,symbol,kind,start_line,end_line,text) values($1,'h1','App.swift','quokkaView','function_declaration',5,7,$2)",
        &[&repo, &"func quokkaView() {\n  body\n}"],
    )
    .unwrap();
    let v = store
        .symbol_explain("quokkaView", Some(&repo), 10, &|_| true)
        .unwrap();
    assert_eq!(v["source"], "tree-sitter");
    let d = &v["definitions"][0];
    assert_eq!(d["code"], "func quokkaView() {\n  body\n}");
    assert_eq!(
        (d["code_start"].as_i64(), d["code_end"].as_i64()),
        (Some(5), Some(7))
    );
    assert!(v["note"].as_str().unwrap().contains("not available"));
    let none = store
        .symbol_explain("noSuchThingAnywhere", Some(&repo), 10, &|_| true)
        .unwrap();
    assert_eq!(none["source"], "none");
    cleanup(&url, &repo);
}
