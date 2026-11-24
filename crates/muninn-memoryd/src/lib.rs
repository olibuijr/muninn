use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use muninn_core::{
    auth::Tokens,
    memory::{MemStore, NewMem},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct App {
    pub store: MemStore,
    pub tokens: Tokens,
}

type Res = (StatusCode, Json<Value>);

fn s(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().map(String::from)
}

fn fin(r: Result<Value, String>) -> Res {
    match r {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => {
            let c = if e.starts_with("forbidden") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            };
            (c, Json(json!({"error": e})))
        }
    }
}

pub fn new_mem(b: &Value) -> Result<NewMem, String> {
    Ok(NewMem {
        text: s(b, "text").ok_or("text required")?,
        typ: s(b, "type").unwrap_or("semantic".into()),
        scope: s(b, "scope").unwrap_or("agent".into()),
        project_id: s(b, "project_id"),
        source: s(b, "source"),
        confidence: b["confidence"].as_f64().unwrap_or(0.8).clamp(0.0, 1.0),
        importance: b["importance"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0),
        supersedes: s(b, "supersedes"),
    })
}

async fn dispatch(app: Arc<App>, h: HeaderMap, path: &'static str, b: Value) -> Res {
    let hv = h.get("authorization").and_then(|v| v.to_str().ok());
    let run = h
        .get("x-muninn-run-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let Some(ctx) = app.tokens.resolve(hv, run) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        );
    };
    tokio::task::spawn_blocking(move || {
        let st = &app.store;
        fin(match path {
            "remember" => new_mem(&b).and_then(|m| st.remember(&ctx, &m)),
            "recall" => match s(&b, "query") {
                Some(q) => st.recall(
                    &ctx,
                    &q,
                    b["scope"].as_str(),
                    b["k"].as_u64().unwrap_or(8) as usize,
                ),
                None => Err("query required".into()),
            },
            "forget" => match s(&b, "id") {
                Some(i) => st.forget(&ctx, &i),
                None => Err("id required".into()),
            },
            "review" => st.review_list(&ctx),
            "review/count" => st.review_count(&ctx),
            "stats" => st.stats(&ctx),
            "review/decide" => match s(&b, "id") {
                Some(i) => st.review_decide(
                    &ctx,
                    &i,
                    b["approve"].as_bool().unwrap_or(false),
                    b["supersede"].as_bool().unwrap_or(false),
                ),
                None => Err("id required".into()),
            },
            _ => Err("unknown".into()),
        })
    })
    .await
    .unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal"})),
        )
    })
}

macro_rules! route {
    ($r:expr, $p:literal) => {
        $r.route(
            concat!("/", $p),
            post(
                |State(a): State<Arc<App>>, h: HeaderMap, Json(b): Json<Value>| {
                    dispatch(a, h, $p, b)
                },
            ),
        )
    };
}

pub fn router(app: Arc<App>) -> Router {
    let mut r = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(|State(a): State<Arc<App>>| async move {
                let ok = tokio::task::spawn_blocking(move || a.store.ping())
                    .await
                    .unwrap_or(false);
                (if ok {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                })
                .into_response()
            }),
        );
    r = route!(r, "remember");
    r = route!(r, "recall");
    r = route!(r, "forget");
    r = route!(r, "review");
    r = route!(r, "review/count");
    r = route!(r, "stats");
    r = route!(r, "review/decide");
    r.with_state(app)
}
