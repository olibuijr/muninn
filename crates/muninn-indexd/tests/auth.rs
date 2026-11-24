use muninn_indexd::{App, require_token, router};

const TOKEN: &str = "test-token";

async fn serve() -> std::net::SocketAddr {
    let app = App::new(std::env::temp_dir(), None);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(axum::serve(l, require_token(router(app), TOKEN.into())).into_future());
    addr
}

async fn status(addr: std::net::SocketAddr, path: &'static str, auth: Option<&'static str>) -> u16 {
    tokio::task::spawn_blocking(move || {
        let mut req = ureq::get(format!("http://{addr}{path}"));
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        req.config()
            .http_status_as_error(false)
            .build()
            .call()
            .unwrap()
            .status()
            .as_u16()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn anonymous_search_is_401() {
    let a = serve().await;
    for path in [
        "/search?q=x",
        "/graph?scope=all",
        "/symbol?name=x",
        "/symbols/stats",
    ] {
        assert_eq!(status(a, path, None).await, 401, "{path}");
    }
}

#[tokio::test]
async fn wrong_token_is_401() {
    let a = serve().await;
    assert_eq!(status(a, "/search?q=x", Some("Bearer nope")).await, 401);
}

#[tokio::test]
async fn valid_token_is_served() {
    let a = serve().await;
    assert_eq!(
        status(a, "/search?q=x", Some("Bearer test-token")).await,
        200
    );
}

#[tokio::test]
async fn health_probes_stay_open() {
    let a = serve().await;
    assert_eq!(status(a, "/healthz", None).await, 200);
}
