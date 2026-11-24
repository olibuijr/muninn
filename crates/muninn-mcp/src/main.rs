use muninn_mcp::{Cfg, router};
#[tokio::main]
async fn main() {
    let cfg = Cfg {
        indexd: std::env::var("MUNINN_INDEXD_URL").unwrap_or("http://127.0.0.1:7410".into()),
        memoryd: std::env::var("MUNINN_MEMORYD_URL").unwrap_or("http://127.0.0.1:7412".into()),
        tokens: std::env::var("MUNINN_TOKENS_FILE")
            .map(|p| muninn_core::auth::Tokens::load(&p).expect("tokens"))
            .unwrap_or_default(),
        token: std::env::var("MUNINN_MCP_TOKEN").ok(),
    };
    let bind = std::env::var("MUNINN_MCP_BIND").unwrap_or("127.0.0.1:7411".into());
    let l = tokio::net::TcpListener::bind(&bind).await.unwrap();
    eprintln!("{{\"event\":\"listening\",\"addr\":\"{bind}\"}}");
    axum::serve(l, router(cfg.into())).await.unwrap();
}
