// Live test (scratch database only, MUNINN_TEST_PG_URL): /symbol/explain reads definition code from
// git at the indexed commit and flags a stale index; /symbols/for_hits decorates spans.
use muninn_core::PgStore;
use muninn_core::scip::{Call, Def, Graph};
use muninn_indexd::{App, router};
use serde_json::Value;
use std::{path::Path, process::Command, sync::Arc};

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}");
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

#[test]
fn explain_reads_code_at_indexed_commit() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let repo = format!("sym-http-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("muninn-sym-live-{}", std::process::id()));
    let dir = root.join(&repo);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.join("src/lib.rs"), &body).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    let sha = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("src/lib.rs"), format!("new first line\n{body}")).unwrap();
    git(&dir, &["commit", "-qam", "two"]);

    let store = Arc::new(PgStore::connect(&url).unwrap());
    let d = |sym: &str, name: &str, s, e| Def {
        symbol: sym.into(),
        name: name.into(),
        kind: "function".into(),
        path: "src/lib.rs".into(),
        start_line: s,
        end_line: e,
        doc: None,
    };
    let g = Graph {
        defs: vec![
            d("t x outer().", "outer", 1, 9),
            d("t x inner().", "inner", 10, 14),
        ],
        refs: vec![],
        calls: vec![Call {
            caller: "t x outer().".into(),
            callee: "t x inner().".into(),
            path: "src/lib.rs".into(),
            line: 3,
        }],
    };
    store.replace_symbols(&repo, &sha, &g).unwrap();

    let app = App::with_backends(root.clone(), None, Some(store.clone()), None, None);
    // The blocking Postgres client must stay outside the async runtime; serve from its own.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let l = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = l.local_addr().unwrap();
    rt.spawn(axum::serve(l, router(app)).into_future());
    let v = ureq::get(format!(
        "http://{addr}/symbol/explain?name=inner&repo={repo}"
    ))
    .call()
    .unwrap()
    .body_mut()
    .read_to_string()
    .unwrap();
    let body = format!(
        r#"{{"hits":[{{"repo":"{repo}","path":"src/lib.rs","start_line":1,"end_line":9}}]}}"#
    );
    let h = ureq::post(format!("http://{addr}/symbols/for_hits"))
        .header("content-type", "application/json")
        .send(body)
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    let (v, h): (Value, Value) = (
        serde_json::from_str(&v).unwrap(),
        serde_json::from_str(&h).unwrap(),
    );

    let def = &v["definitions"][0];
    assert_eq!(
        def["code"], "line 10\nline 11\nline 12\nline 13\nline 14",
        "{v}"
    );
    assert_eq!(def["stale"], true);
    assert_eq!(def["code_rev"], sha.as_str());
    assert_eq!(def["callers"]["top"][0]["name"], "outer");
    let next = &def["next"][0];
    assert_eq!(next["tool"], "read_file");
    assert_eq!(
        next["args"]["branch"],
        sha.as_str(),
        "stale index reads at its commit"
    );
    assert_eq!(h["hits"][0]["name"], "outer");
    assert_eq!(h["hits"][0]["callees"], 1);

    store.delete_symbols(&repo).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
