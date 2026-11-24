use muninn_indexd::{App, router};
use serde_json::Value;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn git(dir: &std::path::Path, args: &[&str]) {
    let s = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(s.success());
}

const LIB: &str =
    "// header\nuse x;\n\nfn filterword_alpha() {\n    let a = 1;\n}\n\nfn other() {}\n";
const MAIN: &str = "fn filterword_beta() {}\n";
const DOC: &str = "filterword in a readme\n";

/// A repo `demo` on disk (non-bare, under repos_dir) plus an in-memory index of the same files.
fn setup() -> Arc<App> {
    let root = std::env::temp_dir().join(format!(
        "indexd-filters-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("demo");
    std::fs::create_dir_all(repo.join("src/bin")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(repo.join("src/bin/main.rs"), MAIN).unwrap();
    std::fs::write(repo.join("README.md"), DOC).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "x",
        ],
    );
    let app = App::new(root, None);
    let mut ix = muninn_core::Index::new("demo");
    let files = [
        ("src/lib.rs", LIB),
        ("src/bin/main.rs", MAIN),
        ("README.md", DOC),
    ];
    ix.apply_push(
        "main",
        files
            .iter()
            .map(|(p, t)| muninn_core::Change::Upsert {
                path: (*p).into(),
                text: (*t).into(),
            })
            .collect(),
        None,
    );
    app.indexes
        .lock()
        .unwrap()
        .insert("demo".into(), Arc::new(Mutex::new(ix)));
    app
}

async fn get(app: Arc<App>, q: &'static str) -> Value {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, router(app)).into_future());
    tokio::task::spawn_blocking(move || {
        let body = ureq::get(format!("http://{addr}{q}"))
            .call()
            .unwrap()
            .into_body()
            .read_to_string()
            .unwrap();
        serde_json::from_str(&body).unwrap()
    })
    .await
    .unwrap()
}

fn paths(v: &Value) -> Vec<String> {
    let mut p: Vec<String> = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_string())
        .collect();
    p.sort();
    p
}

#[tokio::test]
async fn search_filters_by_path_glob_and_kind() {
    let app = setup();
    let all = get(app.clone(), "/search?q=filterword&mode=lexical").await;
    assert_eq!(paths(&all), ["README.md", "src/bin/main.rs", "src/lib.rs"]);
    let rs = get(
        app.clone(),
        "/search?q=filterword&mode=lexical&path=src/**/*.rs",
    )
    .await;
    assert_eq!(paths(&rs), ["src/bin/main.rs", "src/lib.rs"]);
    let one = get(
        app.clone(),
        "/search?q=filterword&mode=lexical&path=src/*.rs",
    )
    .await;
    assert_eq!(paths(&one), ["src/lib.rs"]);
    let f = get(
        app.clone(),
        "/search?q=filterword&mode=lexical&kind=function_item&path=**/*.rs",
    )
    .await;
    assert!(!paths(&f).is_empty());
    assert!(
        f["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["kind"] == "function_item"),
        "{f}"
    );
    let none = get(app, "/search?q=filterword&mode=lexical&kind=no_such_kind").await;
    assert!(none["hits"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn search_context_expands_snippet_from_git() {
    let app = setup();
    let v = get(
        app,
        "/search?q=filterword_alpha&mode=lexical&path=src/lib.rs&kind=function_item&context=2",
    )
    .await;
    let h = &v["hits"][0];
    assert_eq!(h["start_line"], 4, "{v}");
    assert_eq!(h["snippet_start"], 2);
    assert_eq!(
        h["snippet"],
        "use x;\n\nfn filterword_alpha() {\n    let a = 1;\n}\n\nfn other() {}"
    );
}
