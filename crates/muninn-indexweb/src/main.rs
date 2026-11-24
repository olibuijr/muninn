use muninn_indexweb::{Cfg, router, valid_token};
#[tokio::main]
async fn main() {
    let token = std::env::var("MUNINN_INDEXWEB_TOKEN").unwrap_or_default();
    assert!(
        valid_token(&token),
        "MUNINN_INDEXWEB_TOKEN (16+ chars) required"
    );
    let cfg = Cfg {
        indexd: std::env::var("MUNINN_INDEXD_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7410".into())
            .trim_end_matches('/')
            .to_string(),
        indexd_token: std::env::var("MUNINN_INDEXD_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        token,
    };
    let bind = std::env::var("MUNINN_INDEXWEB_BIND").unwrap_or_else(|_| "0.0.0.0:7415".into());
    let l = tokio::net::TcpListener::bind(&bind).await.unwrap();
    eprintln!("{{\"event\":\"listening\",\"addr\":\"{bind}\"}}");
    axum::serve(l, router(cfg.into())).await.unwrap();
}
