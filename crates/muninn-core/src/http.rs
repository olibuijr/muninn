use crate::gate::{EMBED_GATE, Gate, RERANK_GATE, current_lane, timeout_for};
use crate::index::{Embedder, Reranker};
use serde_json::{Value, json};
use std::time::Duration;

pub const EMBED_BATCH: usize = 16;
const MAX_CHARS: usize = 6000;
const RERANK_CHARS: usize = 1500;

pub struct HttpEmbedder {
    pub url: String,
}
pub struct HttpReranker {
    pub url: String,
}

fn with_path(url: &str, path: &str) -> String {
    let u = url.trim_end_matches('/');
    if u.matches('/').count() <= 2 {
        format!("{u}{path}")
    } else {
        u.to_string()
    }
}

const API_KEY_ENV: &str = "MUNINN_LLAMA_API_KEY";

fn api_key() -> Option<String> {
    std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty())
}

fn post(url: &str, body: Value) -> Option<Value> {
    post_with_key(url, body, api_key().as_deref())
}

fn post_with_key(url: &str, body: Value, key: Option<&str>) -> Option<Value> {
    let mut req = ureq::post(url);
    if let Some(k) = key {
        req = req.header("Authorization", &format!("Bearer {k}"));
    }
    req.send_json(body).ok()?.body_mut().read_json().ok()
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_CHARS).collect()
}

impl Embedder for HttpEmbedder {
    fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
        let mut out = Vec::new();
        for batch in texts.chunks(EMBED_BATCH) {
            let lane = current_lane();
            let Some(_p) = EMBED_GATE.acquire(lane, timeout_for(lane)) else {
                out.extend(batch.iter().map(|_| Vec::new()));
                continue;
            };
            let inputs: Vec<String> = batch.iter().map(|t| clip(t)).collect();
            let v = post(
                &with_path(&self.url, "/v1/embeddings"),
                json!({"input": inputs}),
            );
            let data = v.as_ref().and_then(|v| v["data"].as_array());
            for i in 0..batch.len() {
                let emb = data
                    .and_then(|d| d.get(i))
                    .and_then(|e| e["embedding"].as_array())
                    .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
                    .unwrap_or_default();
                out.push(emb);
            }
        }
        out
    }
}

impl Reranker for HttpReranker {
    fn rerank(&self, query: &str, docs: &[String]) -> Vec<f32> {
        let docs: Vec<String> = docs
            .iter()
            .map(|d| d.chars().take(RERANK_CHARS).collect())
            .collect();
        let mut scores = vec![f32::MIN; docs.len()];
        let lane = current_lane();
        let Some(_p) = RERANK_GATE.acquire(lane, timeout_for(lane)) else {
            return scores;
        };
        if let Some(v) = post(
            &with_path(&self.url, "/v1/rerank"),
            json!({"query": query, "documents": docs}),
        ) && let Some(r) = v["results"].as_array()
        {
            for e in r {
                if let (Some(i), Some(s)) = (e["index"].as_u64(), e["relevance_score"].as_f64())
                    && let Some(slot) = scores.get_mut(i as usize)
                {
                    *slot = s as f32;
                }
            }
        }
        scores
    }
}

/// Chat completions share the model server with embeddings and rerank: two concurrent requests at most,
/// query-lane callers first (see `gate`).
pub static CHAT_GATE: Gate = Gate::new(2);

/// Non-streaming client for an OpenAI-compatible `/v1/chat/completions` (the model server's resident chat
/// model). Every request has a hard wall-clock timeout, so a slow model cannot stall a search.
pub struct HttpChat {
    pub url: String,
    pub model: String,
    pub timeout: Duration,
    agent: ureq::Agent,
}

impl HttpChat {
    /// `url` is a base (`http://host:8084`) or the full endpoint URL.
    pub fn new(url: &str, model: &str, timeout: Duration) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        HttpChat {
            url: with_path(url, "/v1/chat/completions"),
            model: model.into(),
            timeout,
            agent,
        }
    }

    /// The assistant message for one system + user turn at temperature 0, or None on timeout,
    /// HTTP error, a full gate or an unexpected body.
    pub fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Option<String> {
        let lane = current_lane();
        let _p = CHAT_GATE.acquire(lane, timeout_for(lane).min(self.timeout))?;
        let body = json!({
            "model": self.model, "temperature": 0, "max_tokens": max_tokens, "stream": false,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let mut req = self.agent.post(&self.url);
        if let Some(k) = api_key() {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        let v: Value = req.send_json(body).ok()?.body_mut().read_json().ok()?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(String::from)
    }
}

#[cfg(test)]
mod tests {
    use super::post_with_key;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn request_seen_by_server(key: Option<&str>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let body = "{}";
            write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });
        post_with_key(&url, json!({"input": ["x"]}), key);
        h.join().unwrap()
    }

    #[test]
    fn sends_bearer_token_when_configured() {
        assert!(request_seen_by_server(Some("s3cret")).contains("authorization: bearer s3cret"));
    }

    #[test]
    fn sends_no_authorization_without_token() {
        assert!(!request_seen_by_server(None).contains("authorization"));
    }
}
