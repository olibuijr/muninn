use muninn_indexd::{App, graph, router};

#[tokio::test]
async fn graph_without_store_is_503() {
    let app = App::new(std::env::temp_dir(), None);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, router(app)).into_future());
    let code = tokio::task::spawn_blocking(move || {
        ureq::get(format!("http://{addr}/graph?scope=all"))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap()
            .status()
            .as_u16()
    })
    .await
    .unwrap();
    assert_eq!(code, 503);
}

// Live graph test against the real store; skipped unless MUNINN_PG_URL is set.
#[test]
fn graph_payload_shape() {
    let Ok(url) = std::env::var("MUNINN_PG_URL") else {
        return;
    };
    let store = muninn_core::PgStore::connect(&url).unwrap();
    let v = graph::build(&store, "all", 5000, 4, &store.graph_version());
    let n = v["n"]["x"].as_array().unwrap().len();
    assert_eq!(v["n"]["y"].as_array().unwrap().len(), n);
    assert_eq!(v["n"]["label"].as_array().unwrap().len(), n);
    let (s, t) = (
        v["e"]["s"].as_array().unwrap(),
        v["e"]["t"].as_array().unwrap(),
    );
    assert_eq!(s.len(), t.len());
    assert!(
        s.iter()
            .chain(t)
            .all(|i| (i.as_u64().unwrap() as usize) < n)
    );
}
