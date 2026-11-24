use std::collections::VecDeque;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CacheKey {
    pub query: String,
    pub scope: Option<String>,
    pub project: Option<String>,
    pub k: usize,
    pub caller: String,
    pub version: u64,
}

/// Small LRU (front = most recent); capacities are tens of entries so a linear scan is fine.
pub struct RecallCache {
    cap: usize,
    entries: VecDeque<(CacheKey, String)>,
}

impl RecallCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: VecDeque::new(),
        }
    }

    pub fn get(&mut self, key: &CacheKey) -> Option<String> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let hit = self.entries.remove(pos)?;
        let value = hit.1.clone();
        self.entries.push_front(hit);
        Some(value)
    }

    pub fn put(&mut self, key: CacheKey, value: String) {
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push_front((key, value));
        self.entries.truncate(self.cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(q: &str, version: u64) -> CacheKey {
        CacheKey {
            query: q.into(),
            scope: None,
            project: None,
            k: 8,
            caller: "a".into(),
            version,
        }
    }

    #[test]
    fn hit_returns_stored_value() {
        let mut c = RecallCache::new(2);
        c.put(key("q", 1), "v".into());
        assert_eq!(c.get(&key("q", 1)).as_deref(), Some("v"));
    }
    #[test]
    fn new_corpus_version_misses() {
        let mut c = RecallCache::new(2);
        c.put(key("q", 1), "v".into());
        assert!(c.get(&key("q", 2)).is_none());
    }
    #[test]
    fn project_filter_is_part_of_the_key() {
        let mut c = RecallCache::new(4);
        c.put(key("q", 1), "all".into());
        let mut p = key("q", 1);
        p.project = Some("api".into());
        assert!(c.get(&p).is_none());
        c.put(p.clone(), "api".into());
        assert_eq!(c.get(&p).as_deref(), Some("api"));
        assert_eq!(c.get(&key("q", 1)).as_deref(), Some("all"));
    }
    #[test]
    fn evicts_least_recently_used() {
        let mut c = RecallCache::new(2);
        c.put(key("a", 1), "1".into());
        c.put(key("b", 1), "2".into());
        c.get(&key("a", 1));
        c.put(key("c", 1), "3".into());
        assert!(c.get(&key("b", 1)).is_none());
        assert!(c.get(&key("a", 1)).is_some());
    }
}
