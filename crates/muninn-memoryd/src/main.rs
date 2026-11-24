use muninn_core::{HttpEmbedder, HttpReranker, auth::Tokens, memory::MemStore};
use muninn_memoryd::{App, router};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let app = tokio::task::spawn_blocking(|| {
        let url = std::env::var("MUNINN_PG_URL").expect("MUNINN_PG_URL");
        let emb = HttpEmbedder {
            url: std::env::var("MUNINN_EMBED_URL").expect("MUNINN_EMBED_URL"),
        };
        let rr = HttpReranker {
            url: std::env::var("MUNINN_RERANK_URL").expect("MUNINN_RERANK_URL"),
        };
        let store = MemStore::connect(&url, Box::new(emb), Box::new(rr)).expect("memory store");
        let tokens =
            Tokens::load(&std::env::var("MUNINN_TOKENS_FILE").expect("MUNINN_TOKENS_FILE"))
                .expect("tokens");
        Arc::new(App { store, tokens })
    })
    .await
    .unwrap();
    if std::env::args().nth(1).as_deref() == Some("lifecycle") {
        let job = std::env::args().nth(2).unwrap_or_else(|| "all".into());
        let params = muninn_core::lifecycle::Params {
            dry_run: std::env::args().any(|a| a == "--dry-run"),
            ..Default::default()
        };
        let a = app.clone();
        let r = tokio::task::spawn_blocking(move || a.store.run_lifecycle(&job, &params))
            .await
            .unwrap();
        match r {
            Ok(v) => {
                println!("{v}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("lifecycle failed: {e}");
                std::process::exit(1);
            }
        }
    }
    let bind = std::env::var("MUNINN_MEMORYD_BIND").unwrap_or("127.0.0.1:7412".into());
    let l = tokio::net::TcpListener::bind(&bind).await.unwrap();
    eprintln!("{{\"event\":\"listening\",\"addr\":\"{bind}\"}}");
    axum::serve(l, router(app)).await.unwrap();
}
