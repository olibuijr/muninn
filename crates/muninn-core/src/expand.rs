//! Query expansion: turn a natural-language code question ("where is the webhook signature
//! verified") into the identifiers and keywords BM25 can match, plus an optional HyDE snippet
//! for the dense side.
//!
//! Tiers, cheapest first:
//! - [`NoopExpander`]; nothing.
//! - [`HeuristicExpander`] / [`heuristic_expand`]; no model: stems words and joins adjacent
//!   content words into `snake_case` / `camelCase` identifiers (`webhookSignature`,
//!   `verify_signature`). Microseconds.
//! - [`HttpChatExpander`]; asks the chat model on the model server for identifiers and a HyDE line. Several
//!   hundred ms (decode is ~11 ms/token), so it is opt-in; results are cached (LRU, 256).
//!
//! Callers combine an [`Expansion`] with the original query through [`lexical_query`] (BM25
//! text) and [`dense_text`] (what to embed).
use crate::http::HttpChat;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansion {
    /// Plain words (stems, synonyms) to add to the lexical query.
    pub keywords: Vec<String>,
    /// Code identifiers as they would be written (`verify_signature`, `webhookSignature`).
    pub identifiers: Vec<String>,
    /// A 1-3 line hypothetical code / doc snippet that would answer the query (HyDE).
    pub hyde: Option<String>,
}

impl Expansion {
    pub fn is_empty(&self) -> bool {
        self.keywords.is_empty() && self.identifiers.is_empty() && self.hyde.is_none()
    }

    /// Union of two expansions, `self` first, case-insensitively deduplicated; `self.hyde` wins.
    pub fn merge(mut self, other: Expansion) -> Expansion {
        self.identifiers = dedup_ci(self.identifiers.into_iter().chain(other.identifiers));
        self.keywords = dedup_ci(self.keywords.into_iter().chain(other.keywords));
        self.hyde = self.hyde.or(other.hyde);
        self
    }
}

pub trait Expander: Send + Sync {
    fn expand(&self, q: &str) -> Expansion;
}

/// Expansion level, as accepted on the command line and (later) by `/search?expand=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    None,
    Heuristic,
    Llm,
    /// Heuristic merged with the LLM expansion.
    Both,
}

impl std::str::FromStr for ExpandMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" | "off" | "" => Ok(ExpandMode::None),
            "heuristic" | "h" => Ok(ExpandMode::Heuristic),
            "llm" => Ok(ExpandMode::Llm),
            "both" => Ok(ExpandMode::Both),
            o => Err(format!("unknown expand mode {o} (none|heuristic|llm|both)")),
        }
    }
}

/// The expander for a mode; LLM modes use [`HttpChatExpander::from_env`].
pub fn expander(mode: ExpandMode) -> Box<dyn Expander> {
    match mode {
        ExpandMode::None => Box::new(NoopExpander),
        ExpandMode::Heuristic => Box::new(HeuristicExpander),
        ExpandMode::Llm => Box::new(HttpChatExpander::from_env()),
        ExpandMode::Both => Box::new(BothExpander(HttpChatExpander::from_env())),
    }
}

pub struct NoopExpander;

impl Expander for NoopExpander {
    fn expand(&self, _q: &str) -> Expansion {
        Expansion::default()
    }
}

pub struct HeuristicExpander;

impl Expander for HeuristicExpander {
    fn expand(&self, q: &str) -> Expansion {
        heuristic_expand(q)
    }
}

struct BothExpander(HttpChatExpander);

impl Expander for BothExpander {
    fn expand(&self, q: &str) -> Expansion {
        heuristic_expand(q).merge(self.0.expand(q))
    }
}

/// Question words, glue and generic code-talk that never help BM25.
#[rustfmt::skip]
const STOP: &[&str] = &[
    "a", "about", "after", "all", "an", "and", "any", "are", "as", "at", "be", "before", "being",
    "by", "can", "code", "decide", "decides", "do", "does", "doing", "done", "each", "find", "for",
    "from", "function", "get", "gets", "handle", "handled", "handles", "has", "have", "how", "i",
    "if", "implementation", "implemented", "in", "into", "is", "it", "its", "logic", "me", "of",
    "on", "or", "part", "place", "should", "show", "so", "some", "that", "the", "their", "them",
    "then", "there", "these", "this", "those", "to", "up", "used", "uses", "via", "was", "we",
    "what", "when", "where", "whether", "which", "who", "why", "will", "with", "would", "you",
];

/// Lowercase alphanumeric words of a query, minus stop words.
fn content_words(q: &str) -> Vec<String> {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Crude English stemmer for code words: `verified` -> `verify`, `retrying` -> `retry`,
/// `windows` -> `window`, `running` -> `run`, `authenticated` -> `authenticate`. Returns None
/// when the word has no recognised suffix. Also reports whether the word looked like a verb
/// form (`-ed`/`-ing`), which drives the `verify_signature` reordering.
fn stem(w: &str) -> Option<(String, bool)> {
    let n = w.len();
    if n <= 4 || !w.is_ascii() {
        return None;
    }
    let undouble = |s: &str| -> String {
        let b = s.as_bytes();
        let l = b.len();
        if l >= 3
            && b[l - 1] == b[l - 2]
            && !is_vowel(b[l - 1] as char)
            && !b"lsz".contains(&b[l - 1])
        {
            s[..l - 1].into()
        } else {
            s.into()
        }
    };
    // -ate/-ize/-ise/-ify verbs keep their e: `authenticated` -> `authenticate`.
    let e_verb = |s: &str| {
        ["at", "iz", "is", "rs", "rg", "dl", "v", "od"]
            .iter()
            .any(|x| s.ends_with(x))
    };
    if let Some(s) = w.strip_suffix("ied").or_else(|| w.strip_suffix("ies")) {
        return Some((format!("{s}y"), w.ends_with("ied")));
    }
    if let Some(s) = w.strip_suffix("ing").filter(|s| s.len() >= 3) {
        let s = undouble(s);
        return Some((if e_verb(&s) { format!("{s}e") } else { s }, true));
    }
    if let Some(s) = w.strip_suffix("ed").filter(|s| s.len() >= 3) {
        if s.ends_with('e') {
            return Some((s.into(), true));
        }
        let s = undouble(s);
        return Some((if e_verb(&s) { format!("{s}e") } else { s }, true));
    }
    if ["xes", "ches", "shes", "sses"]
        .iter()
        .any(|x| w.ends_with(x))
    {
        return Some((w[..n - 2].into(), false));
    }
    if w.ends_with('s')
        && !["ss", "us", "is", "as", "os"]
            .iter()
            .any(|x| w.ends_with(x))
        && !KEEP_S.contains(&w)
    {
        return Some((w[..n - 1].into(), false));
    }
    None
}

/// Words that end in `s` but are not plurals.
const KEEP_S: &[&str] = &[
    "always",
    "postgres",
    "series",
    "species",
    "kubernetes",
    "news",
    "sometimes",
];

fn camel(a: &str, b: &str) -> String {
    let mut c = b.chars();
    match c.next() {
        Some(f) => format!("{a}{}{}", f.to_uppercase(), c.as_str()),
        None => a.into(),
    }
}

/// The zero-cost tier: no model. Keywords are the stems of inflected content words
/// (`verified` -> `verify`); identifiers are adjacent content-word pairs joined as
/// `snake_case` and `camelCase` (on their base forms), plus `verb_object` when the second word
/// was a verb form (`signature verified` -> `verify_signature`). BM25 splits identifiers into
/// words again, but a camelCase join also adds the joined token (`webhooksignature`) that a
/// `webhookSignature` identifier is indexed under.
pub fn heuristic_expand(q: &str) -> Expansion {
    let words = content_words(q);
    let mut keywords = Vec::new();
    let mut base: Vec<(String, bool)> = Vec::new();
    for w in &words {
        match stem(w) {
            Some((s, verb)) if s != *w => {
                keywords.push(s.clone());
                base.push((s, verb));
            }
            _ => base.push((w.clone(), false)),
        }
    }
    let mut identifiers = Vec::new();
    for p in base.windows(2) {
        let ((a, _), (b, b_verb)) = (&p[0], &p[1]);
        if a == b || a.chars().all(|c| c.is_numeric()) || b.chars().all(|c| c.is_numeric()) {
            continue;
        }
        identifiers.push(format!("{a}_{b}"));
        identifiers.push(camel(a, b));
        if *b_verb {
            identifiers.push(format!("{b}_{a}"));
            identifiers.push(camel(b, a));
        }
    }
    let orig: HashSet<String> = words.into_iter().collect();
    Expansion {
        keywords: dedup_ci(keywords.into_iter().filter(|k| !orig.contains(k))),
        identifiers: dedup_ci(identifiers),
        hyde: None,
    }
}

fn dedup_ci(it: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    it.into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
        .collect()
}

/// Most terms [`lexical_query`] appends after the original words.
pub const MAX_EXTRA_TERMS: usize = 24;

/// Text for BM25: the original query verbatim, then identifiers, then keywords, skipping any
/// term already present (case-insensitive, by word), at most [`MAX_EXTRA_TERMS`] extras.
pub fn lexical_query(original: &str, e: &Expansion) -> String {
    let mut seen: HashSet<String> = original
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut out = original.trim().to_string();
    let extras = e
        .identifiers
        .iter()
        .chain(&e.keywords)
        .filter(|t| seen.insert(t.to_lowercase()))
        .take(MAX_EXTRA_TERMS);
    for t in extras {
        out.push(' ');
        out.push_str(t);
    }
    out
}

/// Text to embed (before the query prompt): the original query, plus the HyDE snippet when
/// the expansion has one.
pub fn dense_text(original: &str, e: &Expansion) -> String {
    match e.hyde.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => format!("{}\n{h}", original.trim()),
        None => original.trim().to_string(),
    }
}

pub const EXPAND_SYSTEM: &str = "You turn a question about a code base into search terms. \
Reply with one JSON object and nothing else: {\"ids\":[...],\"hyde\":\"...\"}. \
ids: 5 to 12 identifiers or keywords that likely appear in the code that answers the question, \
written as in Rust, Swift or TypeScript source (snake_case functions and fields, CamelCase \
types, camelCase in Swift/TS). hyde: one or two short lines of plausible code or a doc comment \
from that code. No markdown, no explanation.";

/// Token budget for the reply; 10 identifiers plus a one-line HyDE fit in ~90.
pub const EXPAND_MAX_TOKENS: u32 = 110;
const CACHE_CAP: usize = 256;

/// Small LRU keyed by the normalised query.
struct Lru {
    map: HashMap<String, Expansion>,
    order: VecDeque<String>,
    cap: usize,
}

impl Lru {
    fn get(&mut self, k: &str) -> Option<Expansion> {
        let v = self.map.get(k)?.clone();
        if let Some(i) = self.order.iter().position(|x| x == k) {
            let key = self.order.remove(i).unwrap();
            self.order.push_back(key);
        }
        Some(v)
    }
    fn put(&mut self, k: String, v: Expansion) {
        if self.map.insert(k.clone(), v).is_some() {
            self.order.retain(|x| *x != k);
        } else if self.map.len() > self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
        self.order.push_back(k);
    }
}

/// LLM expansion through the model server's chat model. Failures (timeout, junk) give an empty expansion
/// and are not cached, so callers can always fall back to the plain query.
pub struct HttpChatExpander {
    chat: HttpChat,
    cache: Mutex<Lru>,
}

pub const DEFAULT_CHAT_MODEL: &str = "gemma-3-4b-it";
pub const DEFAULT_EXPAND_TIMEOUT_MS: u64 = 1500;

impl HttpChatExpander {
    pub fn new(url: &str, model: &str, timeout: Duration) -> Self {
        HttpChatExpander {
            chat: HttpChat::new(url, model, timeout),
            cache: Mutex::new(Lru {
                map: HashMap::new(),
                order: VecDeque::new(),
                cap: CACHE_CAP,
            }),
        }
    }

    /// `MUNINN_CHAT_URL` (else `MUNINN_EMBED_URL`, else `http://127.0.0.1:8084`),
    /// `MUNINN_CHAT_MODEL` (default [`DEFAULT_CHAT_MODEL`]) and `MUNINN_EXPAND_TIMEOUT_MS`
    /// (default [`DEFAULT_EXPAND_TIMEOUT_MS`]); the bearer is `MUNINN_LLAMA_API_KEY`.
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let url = env("MUNINN_CHAT_URL")
            .or_else(|| env("MUNINN_EMBED_URL"))
            .unwrap_or_else(|| "http://127.0.0.1:8084".into());
        let model = env("MUNINN_CHAT_MODEL").unwrap_or_else(|| DEFAULT_CHAT_MODEL.into());
        let ms = env("MUNINN_EXPAND_TIMEOUT_MS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_EXPAND_TIMEOUT_MS);
        Self::new(&url, &model, Duration::from_millis(ms))
    }

    /// Cached expansion without calling the model.
    pub fn cached(&self, q: &str) -> Option<Expansion> {
        self.cache.lock().unwrap().get(&cache_key(q))
    }
}

fn cache_key(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl Expander for HttpChatExpander {
    fn expand(&self, q: &str) -> Expansion {
        let key = cache_key(q);
        if key.is_empty() {
            return Expansion::default();
        }
        if let Some(e) = self.cache.lock().unwrap().get(&key) {
            return e;
        }
        let Some(reply) = self
            .chat
            .complete(EXPAND_SYSTEM, q.trim(), EXPAND_MAX_TOKENS)
        else {
            return Expansion::default();
        };
        let e = parse_expansion(&reply);
        if !e.is_empty() {
            self.cache.lock().unwrap().put(key, e.clone());
        }
        e
    }
}

/// True for a plausible code identifier: ASCII letters, digits and `_`, starting with a
/// letter or `_`, 2-48 chars.
fn is_ident(s: &str) -> bool {
    (2..=48).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

/// Language keywords and type noise a model likes to emit that only add common BM25 terms.
#[rustfmt::skip]
const NOISE: &[&str] = &[
    "fn", "let", "mut", "pub", "impl", "struct", "enum", "while", "loop", "match", "self",
    "return", "func", "var", "const", "class", "function", "string", "str", "int", "bool",
    "result", "option", "some", "none", "ok", "err", "true", "false", "async", "await", "vec",
];

/// Sort model terms into identifiers (contain `_` or an inner capital) and keywords; split
/// anything that is not a bare identifier (`Result<T, E>`, `while let`) into its words.
fn classify(terms: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let (mut ids, mut kws) = (Vec::new(), Vec::new());
    for t in terms {
        let t = t
            .trim()
            .trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
        let parts: Vec<&str> = if is_ident(t) {
            vec![t]
        } else {
            t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|p| is_ident(p) && p.len() >= 3)
                .collect()
        };
        for p in parts {
            if NOISE.contains(&p.to_lowercase().as_str()) {
                continue;
            }
            let inner_cap = p.chars().skip(1).any(|c| c.is_ascii_uppercase());
            if p.contains('_') || inner_cap {
                ids.push(p.to_string());
            } else {
                kws.push(p.to_lowercase());
            }
        }
    }
    (dedup_ci(ids), dedup_ci(kws))
}

/// Strip a markdown fence (```json ... ```) if present.
fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.split_once('\n').map(|x| x.1).unwrap_or("");
    rest.rsplit_once("```").map(|x| x.0).unwrap_or(rest).trim()
}

fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        Value::String(s) => s
            .split([',', ' ', '\n'])
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

/// Parse the model reply. Accepts the asked-for `{"ids":[..],"hyde":".."}`, the long key
/// names (`identifiers`, `keywords`), fenced JSON, JSON wrapped in prose, and as a last resort
/// pulls quoted identifier-like strings out of whatever came back (truncated JSON included).
pub fn parse_expansion(reply: &str) -> Expansion {
    let body = strip_fences(reply);
    let json = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if b > a => serde_json::from_str::<Value>(&body[a..=b]).ok(),
        _ => None,
    };
    if let Some(v) = json.filter(Value::is_object) {
        let mut terms = str_list(&v["ids"]);
        terms.extend(str_list(&v["identifiers"]));
        terms.extend(str_list(&v["keywords"]));
        let (identifiers, keywords) = classify(terms);
        let hyde = ["hyde", "snippet", "code"]
            .iter()
            .find_map(|k| v[*k].as_str())
            .map(|h| h.lines().take(3).collect::<Vec<_>>().join("\n"))
            .map(|h| h.trim().chars().take(400).collect::<String>())
            .filter(|h| !h.is_empty());
        return Expansion {
            keywords,
            identifiers,
            hyde,
        };
    }
    // Junk or truncated: take every "quoted" string that is not a key name, and a hyde value
    // if one got far enough to have its closing quote.
    let mut terms = Vec::new();
    let mut hyde = None;
    let parts: Vec<&str> = body.split('"').collect();
    for (i, p) in parts.iter().enumerate().skip(1).step_by(2) {
        if i + 1 >= parts.len() {
            break; // unterminated
        }
        if ["ids", "identifiers", "keywords", "hyde"].contains(p) {
            continue;
        }
        let prev = parts.get(i.wrapping_sub(2)).copied().unwrap_or("");
        if prev == "hyde" {
            hyde = Some(p.trim().to_string()).filter(|h| !h.is_empty());
        } else {
            terms.push(p.to_string());
        }
    }
    let (identifiers, keywords) = classify(terms);
    Expansion {
        keywords,
        identifiers,
        hyde,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_joins_pairs_and_stems() {
        let e = heuristic_expand("where is the webhook signature verified");
        assert!(e.identifiers.contains(&"webhook_signature".into()));
        assert!(e.identifiers.contains(&"webhookSignature".into()));
        assert!(e.identifiers.contains(&"verify_signature".into()));
        assert!(e.identifiers.contains(&"verifySignature".into()));
        assert_eq!(e.keywords, vec!["verify".to_string()]);
        assert!(e.hyde.is_none());
        assert!(!e.identifiers.iter().any(|i| i.contains("where")));
    }

    #[test]
    fn stems() {
        let s = |w: &str| stem(w).map(|x| x.0);
        assert_eq!(s("retrying").as_deref(), Some("retry"));
        assert_eq!(s("windows").as_deref(), Some("window"));
        assert_eq!(s("running").as_deref(), Some("run"));
        assert_eq!(s("authenticated").as_deref(), Some("authenticate"));
        assert_eq!(s("parsed").as_deref(), Some("parse"));
        assert_eq!(s("parsing").as_deref(), Some("parse"));
        assert_eq!(s("entries").as_deref(), Some("entry"));
        assert_eq!(s("matches").as_deref(), Some("match"));
        assert_eq!(s("signed").as_deref(), Some("sign"));
        assert_eq!(s("moving").as_deref(), Some("move"));
        assert_eq!(s("focus"), None);
        assert_eq!(s("class"), None);
        assert_eq!(s("key"), None);
        assert_eq!(s("postgres"), None);
        assert_eq!(s("always"), None);
        assert_eq!(s("stores").as_deref(), Some("store"));
    }

    #[test]
    fn heuristic_on_empty_and_stopword_queries() {
        assert!(heuristic_expand("").is_empty());
        assert!(heuristic_expand("where is the code that").is_empty());
    }

    #[test]
    fn parses_clean_fenced_and_wrapped_json() {
        let clean = r#"{"ids":["verifySignature","webhook_secret","signature","HmacSha256","fn"],"hyde":"fn verify_signature(body: &[u8], sig: &str) -> bool"}"#;
        let e = parse_expansion(clean);
        assert_eq!(
            e.identifiers,
            vec!["verifySignature", "webhook_secret", "HmacSha256"]
        );
        assert_eq!(e.keywords, vec!["signature"]);
        assert_eq!(
            e.hyde.as_deref(),
            Some("fn verify_signature(body: &[u8], sig: &str) -> bool")
        );
        let fenced = format!("```json\n{clean}\n```");
        assert_eq!(parse_expansion(&fenced), e);
        let wrapped = format!("Sure! Here you go:\n{clean}\nHope that helps.");
        assert_eq!(parse_expansion(&wrapped), e);
    }

    #[test]
    fn parses_long_keys_and_junk_terms() {
        let r = r#"{"identifiers":["Result<T, E>","while let","try_connect"],"keywords":"retry, backoff"}"#;
        let e = parse_expansion(r);
        assert_eq!(e.identifiers, vec!["try_connect"]);
        assert_eq!(e.keywords, vec!["retry", "backoff"]);
        assert!(e.hyde.is_none());
    }

    #[test]
    fn parses_truncated_and_garbage_replies() {
        let r = r#"```json
{"ids":["focus_window","keyboardFocus","seat","#;
        let e = parse_expansion(r);
        assert_eq!(e.identifiers, vec!["focus_window", "keyboardFocus"]);
        assert_eq!(e.keywords, vec!["seat"]);
        assert!(parse_expansion("I cannot help with that.").is_empty());
        assert!(parse_expansion("").is_empty());
        let r = r#"{"ids":["a_b"],"hyde":"// picks the focused window" "#;
        let e = parse_expansion(r);
        assert_eq!(e.identifiers, vec!["a_b"]);
        assert_eq!(e.hyde.as_deref(), Some("// picks the focused window"));
    }

    #[test]
    fn lexical_query_keeps_original_first_and_dedups() {
        let e = Expansion {
            identifiers: vec![
                "verify_signature".into(),
                "Webhook".into(),
                "VERIFY_signature".into(),
            ],
            keywords: vec!["verify".into(), "hmac".into()],
            hyde: Some("x".into()),
        };
        assert_eq!(
            lexical_query("  where is the webhook signature verified ", &e),
            "where is the webhook signature verified verify_signature verify hmac"
        );
        assert_eq!(lexical_query("q", &Expansion::default()), "q");
    }

    #[test]
    fn lexical_query_caps_extras() {
        let e = Expansion {
            keywords: (0..100).map(|i| format!("k{i}")).collect(),
            ..Default::default()
        };
        let q = lexical_query("q", &e);
        assert_eq!(q.split(' ').count(), 1 + MAX_EXTRA_TERMS);
    }

    #[test]
    fn dense_text_adds_hyde_only_when_present() {
        let mut e = Expansion::default();
        assert_eq!(dense_text(" find focus ", &e), "find focus");
        e.hyde = Some("  ".into());
        assert_eq!(dense_text("find focus", &e), "find focus");
        e.hyde = Some("fn focus_window()".into());
        assert_eq!(
            dense_text("find focus", &e),
            "find focus\nfn focus_window()"
        );
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut l = Lru {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: 2,
        };
        let e = |k: &str| Expansion {
            keywords: vec![k.into()],
            ..Default::default()
        };
        l.put("a".into(), e("a"));
        l.put("b".into(), e("b"));
        assert!(l.get("a").is_some());
        l.put("c".into(), e("c"));
        assert!(l.get("b").is_none());
        assert!(l.get("a").is_some() && l.get("c").is_some());
        l.put("c".into(), e("c2"));
        assert_eq!(l.order.len(), 2);
        assert_eq!(l.get("c").unwrap().keywords, vec!["c2"]);
    }

    #[test]
    fn merge_and_modes() {
        let a = Expansion {
            identifiers: vec!["a_b".into()],
            keywords: vec![],
            hyde: None,
        };
        let b = Expansion {
            identifiers: vec!["A_B".into(), "c".into()],
            keywords: vec!["k".into()],
            hyde: Some("h".into()),
        };
        let m = a.merge(b);
        assert_eq!(m.identifiers, vec!["a_b", "c"]);
        assert_eq!(m.hyde.as_deref(), Some("h"));
        assert_eq!("heuristic".parse::<ExpandMode>(), Ok(ExpandMode::Heuristic));
        assert!("bogus".parse::<ExpandMode>().is_err());
        assert!(NoopExpander.expand("anything").is_empty());
    }

    #[test]
    fn chat_expander_parses_server_reply_and_caches() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            // Exactly one connection: a second (uncached) call would hang until the timeout.
            let (mut s, _) = l.accept().unwrap();
            let mut buf = vec![0u8; 16384];
            let mut n = 0;
            // Headers and body may arrive in separate reads; stop once the JSON body closed.
            while n < buf.len() {
                let m = s.read(&mut buf[n..]).unwrap();
                n += m;
                if m == 0 || buf[..n].ends_with(b"}") {
                    break;
                }
            }
            let content = r#"{\"ids\":[\"focus_window\"],\"hyde\":\"// focus\"}"#;
            let body = format!(r#"{{"choices":[{{"message":{{"content":"{content}"}}}}]}}"#);
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let x = HttpChatExpander::new(&url, "m", Duration::from_millis(2000));
        let e = x.expand("Where is focus?");
        assert_eq!(e.identifiers, vec!["focus_window"]);
        assert_eq!(e.hyde.as_deref(), Some("// focus"));
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions"));
        assert!(req.contains("\"temperature\": 0"));
        assert_eq!(x.cached("where  is FOCUS?"), Some(e.clone()));
        assert_eq!(x.expand("where is focus?"), e);
    }

    #[test]
    fn chat_expander_times_out_to_empty() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let x = HttpChatExpander::new(&url, "m", Duration::from_millis(150));
        let t = std::time::Instant::now();
        assert!(x.expand("slow").is_empty());
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(l);
    }

    /// Live latency against the model server: `cargo test -p muninn-core live_chat_expansion -- --ignored
    /// --nocapture` with `~/.config/muninn/index.env` sourced.
    #[test]
    #[ignore]
    fn live_chat_expansion() {
        let x = HttpChatExpander::from_env();
        for q in [
            "where is the webhook signature verified",
            "how does the compositor decide which window gets keyboard focus",
            "code that retries a failed postgres connection",
        ] {
            let t = std::time::Instant::now();
            let e = x.expand(q);
            println!(
                "{:>6.0} ms  {q}\n  {e:?}",
                t.elapsed().as_secs_f64() * 1000.0
            );
            assert!(!e.is_empty());
        }
    }
}
