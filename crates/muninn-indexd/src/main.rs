use muninn_indexd::{App, require_token, router};
#[tokio::main]
async fn main() {
    let (app, bind) = tokio::task::spawn_blocking(setup).await.unwrap();
    let a = app.clone();
    std::thread::spawn(move || {
        a.load_repos();
        muninn_indexd::release_free_memory();
        a.heal_missing();
        // Low-priority fill of chunks without a current embedding; repeated so an embedder that
        // was down at startup, or a new EMB_VERSION, still gets every chunk covered.
        loop {
            a.embed_missing();
            muninn_indexd::release_free_memory();
            std::thread::sleep(std::time::Duration::from_secs(600));
        }
    });
    let l = tokio::net::TcpListener::bind(&bind).await.unwrap();
    eprintln!("{{\"event\":\"listening\",\"addr\":\"{bind}\"}}");
    let token = std::env::var("MUNINN_INDEXD_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .expect("MUNINN_INDEXD_TOKEN (bearer token required on every non-probe route)");
    axum::serve(l, require_token(router(app), token))
        .await
        .unwrap();
}

fn setup() -> (std::sync::Arc<App>, String) {
    let dir = std::env::var("MUNINN_REPOS_DIR")
        .expect("MUNINN_REPOS_DIR (bare/mirror clones, one dir per repo)");
    let secret = std::env::var("MUNINN_WEBHOOK_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .expect("MUNINN_WEBHOOK_SECRET (HMAC key for /webhook/push; generate with openssl rand -hex 32)");
    let bind = std::env::var("MUNINN_INDEXD_BIND").unwrap_or_else(|_| "127.0.0.1:7410".into());
    let store = std::env::var("MUNINN_PG_URL")
        .ok()
        .map(|u| std::sync::Arc::new(muninn_core::PgStore::connect(&u).expect("postgres")));
    let emb = std::env::var("MUNINN_EMBED_URL")
        .ok()
        .map(|url| Box::new(muninn_core::HttpEmbedder { url }) as Box<dyn muninn_core::Embedder>);
    let rr = std::env::var("MUNINN_RERANK_URL")
        .ok()
        .map(|url| Box::new(muninn_core::HttpReranker { url }) as Box<dyn muninn_core::Reranker>);
    let app = App::with_backends(dir.into(), Some(secret), store, emb, rr);
    if let Ok(list) = std::env::var("MUNINN_INITIAL") {
        for spec in list.split(',').filter(|s| !s.is_empty()) {
            let (repo, branch) = spec.split_once('@').unwrap_or((spec, "main"));
            let n = app.index_push(repo, branch, None, branch);
            eprintln!(
                "{{\"event\":\"initial_index\",\"repo\":\"{repo}\",\"branch\":\"{branch}\",\"files\":{n}}}"
            );
        }
    }
    (app, bind)
}
