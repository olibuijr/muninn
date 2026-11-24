use muninn_indexd::{App, require_token, router};

const TOKEN: &str = "test-token";

async fn serve() -> std::net::SocketAddr {
    serve_with(None).await
}

async fn serve_with(secret: Option<&str>) -> std::net::SocketAddr {
    let app = App::new(std::env::temp_dir(), secret.map(String::from));
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

async fn push(addr: std::net::SocketAddr, body: &'static str, sig: Option<String>) -> u16 {
    tokio::task::spawn_blocking(move || {
        let mut req = ureq::post(format!("http://{addr}/webhook/push"));
        if let Some(s) = sig {
            req = req.header("X-Gitea-Signature", &s);
        }
        req.config()
            .http_status_as_error(false)
            .build()
            .send(body)
            .unwrap()
            .status()
            .as_u16()
    })
    .await
    .unwrap()
}

fn sign(secret: &str, body: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    m.update(body.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

#[tokio::test]
async fn webhook_without_secret_refuses_every_push() {
    let a = serve_with(None).await;
    assert_eq!(push(a, "not json", None).await, 503);
    assert_eq!(push(a, "not json", Some(sign("x", "not json"))).await, 503);
}

#[tokio::test]
async fn webhook_checks_the_signature() {
    let a = serve_with(Some("s3cret")).await;
    assert_eq!(push(a, "not json", None).await, 401);
    assert_eq!(
        push(a, "not json", Some(sign("wrong", "not json"))).await,
        401
    );
    // A valid signature gets past the HMAC check to body validation.
    assert_eq!(
        push(a, "not json", Some(sign("s3cret", "not json"))).await,
        400
    );
}
