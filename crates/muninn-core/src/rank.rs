use std::collections::HashMap;

pub const RRF_K: f64 = 60.0;

/// Reciprocal rank fusion over ranked id lists.
pub fn rrf(lists: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    for l in lists {
        for (i, id) in l.iter().enumerate() {
            *score.entry(id).or_default() += 1.0 / (RRF_K + i as f64 + 1.0);
        }
    }
    let mut v: Vec<_> = score.into_iter().map(|(k, s)| (k.to_string(), s)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

pub const RERANK_WINDOW: usize = 5;
const AGREE_TOP: usize = 1;

/// True when both ranked lists put the same id first: reranking cannot add signal.
pub fn top_agree(dense: &[String], fts: &[String]) -> bool {
    if dense.len() < AGREE_TOP || fts.len() < AGREE_TOP {
        return false;
    }
    let mut a: Vec<&String> = dense[..AGREE_TOP].iter().collect();
    let mut b: Vec<&String> = fts[..AGREE_TOP].iter().collect();
    a.sort();
    b.sort();
    a == b
}

pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let lower = word.to_lowercase();
        let mut cur = String::new();
        let chars: Vec<char> = word.chars().collect();
        let mut parts = Vec::new();
        for (i, &c) in chars.iter().enumerate() {
            if i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
            {
                parts.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
        parts.push(cur);
        if parts.len() > 1 {
            out.push(lower);
        }
        out.extend(
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .map(|p| p.to_lowercase()),
        );
    }
    out
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        d / (na.sqrt() * nb.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rrf_prefers_agreement() {
        let r = rrf(&[
            vec!["a".into(), "b".into(), "c".into()],
            vec!["b".into(), "a".into(), "d".into()],
        ]);
        assert_eq!(r[0].0, "a");
        assert_eq!(r[1].0, "b");
        assert_eq!(r.len(), 4);
    }
    #[test]
    fn tokenizer_splits_identifiers() {
        assert_eq!(
            tokenize("parseHttpRequest"),
            vec!["parsehttprequest", "parse", "http", "request"]
        );
        assert_eq!(tokenize("snake_case"), vec!["snake", "case"]);
    }
    #[test]
    fn rerank_skipped_when_both_lists_agree_on_top_hit() {
        let d: Vec<String> = ["a", "b"].map(String::from).to_vec();
        let f: Vec<String> = ["a", "z"].map(String::from).to_vec();
        assert!(top_agree(&d, &f));
    }
    #[test]
    fn rerank_kept_when_top_hit_differs_or_a_list_is_empty() {
        let d: Vec<String> = ["a", "b"].map(String::from).to_vec();
        let f: Vec<String> = ["b", "a"].map(String::from).to_vec();
        assert!(!top_agree(&d, &f));
        assert!(!top_agree(&d, &[]));
        assert!(!top_agree(&[], &[]));
    }
}
