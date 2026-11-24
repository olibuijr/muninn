use crate::memory::Ctx;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize, Clone)]
pub struct TokenEntry {
    pub sha256: String,
    pub actor: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
}

impl TokenEntry {
    fn ctx(&self, run_id: Option<String>) -> Ctx {
        Ctx {
            actor: self.actor.clone(),
            scopes: self.scopes.clone(),
            projects: self.projects.clone(),
            run_id,
        }
    }
}

#[derive(Default, Clone)]
pub struct Tokens(pub Vec<TokenEntry>);

impl Tokens {
    pub fn load(path: &str) -> Result<Self, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Ok(Self(serde_json::from_str(&s).map_err(|e| e.to_string())?))
    }

    /// Resolve an `Authorization: Bearer` header value to a request context.
    pub fn resolve(&self, header: Option<&str>, run_id: Option<String>) -> Option<Ctx> {
        let tok = header?.strip_prefix("Bearer ")?;
        let h = hex::encode(Sha256::digest(tok.as_bytes()));
        self.0.iter().find(|t| t.sha256 == h).map(|t| t.ctx(run_id))
    }
}
