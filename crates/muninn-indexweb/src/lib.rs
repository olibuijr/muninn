use axum::{
    Router,
    extract::{Path, RawQuery, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;

pub struct Cfg {
    pub indexd: String,
    pub indexd_token: Option<String>,
    pub token: String,
}

const PAGE: &str = include_str!("index.html");
const ROUTES: [&str; 6] = ["repos", "search", "symbol", "graph", "file", "readyz"];

pub fn valid_token(t: &str) -> bool {
    t.len() >= 16
}

pub fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn auth(State(cfg): State<Arc<Cfg>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|g| token_matches(g, &cfg.token));
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn proxy(
    State(cfg): State<Arc<Cfg>>,
    Path(name): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    if !ROUTES.contains(&name.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut url = format!("{}/{name}", cfg.indexd);
    if let Some(q) = q {
        url = format!("{url}?{q}");
    }
    let tok = cfg.indexd_token.clone();
    let r = tokio::task::spawn_blocking(move || {
        let mut req = ureq::get(&url).config().http_status_as_error(false).build();
        if let Some(t) = tok {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let mut resp = req.call().map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((status, ct, body))
    })
    .await;
    match r {
        Ok(Ok((status, ct, body))) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            [
                (header::CONTENT_TYPE, ct),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
            body,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, e).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn router(cfg: Arc<Cfg>) -> Router {
    let api = Router::new()
        .route("/api/{name}", get(proxy))
        .layer(middleware::from_fn_with_state(cfg.clone(), auth))
        .with_state(cfg);
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/healthz", get(|| async { "ok" }))
        .merge(api)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_compare() {
        assert!(token_matches("abcdefghijklmnop", "abcdefghijklmnop"));
        assert!(!token_matches("abcdefghijklmnoq", "abcdefghijklmnop"));
        assert!(!token_matches("short", "abcdefghijklmnop"));
        assert!(!token_matches("", "abcdefghijklmnop"));
    }
    #[test]
    fn token_length_rule() {
        assert!(!valid_token("short"));
        assert!(valid_token("0123456789abcdef"));
    }
}
