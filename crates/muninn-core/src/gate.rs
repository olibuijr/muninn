use std::cell::Cell;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    Query = 0,
    Ingest = 1,
    Fill = 2,
}

thread_local! { static LANE: Cell<Lane> = const { Cell::new(Lane::Fill) }; }

pub fn with_lane<R>(l: Lane, f: impl FnOnce() -> R) -> R {
    let prev = LANE.with(|c| c.replace(l));
    let r = f();
    LANE.with(|c| c.set(prev));
    r
}

pub fn current_lane() -> Lane {
    LANE.with(|c| c.get())
}

struct State {
    free: usize,
    waiting: [usize; 3],
    wait_ms: [u64; 3],
    served: [u64; 3],
}

pub struct Gate {
    st: Mutex<State>,
    cv: Condvar,
}

pub struct Permit<'a>(&'a Gate);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.st.lock().unwrap().free += 1;
        self.0.cv.notify_all();
    }
}

impl Gate {
    pub const fn new(n: usize) -> Self {
        Gate {
            st: Mutex::new(State {
                free: n,
                waiting: [0; 3],
                wait_ms: [0; 3],
                served: [0; 3],
            }),
            cv: Condvar::new(),
        }
    }

    /// A lane may take a slot only when no higher-priority lane is waiting.
    pub fn acquire(&self, lane: Lane, timeout: Duration) -> Option<Permit<'_>> {
        let l = lane as usize;
        let start = Instant::now();
        let mut s = self.st.lock().unwrap();
        s.waiting[l] += 1;
        loop {
            if s.free > 0 && s.waiting[..l].iter().all(|w| *w == 0) {
                s.waiting[l] -= 1;
                s.free -= 1;
                s.served[l] += 1;
                s.wait_ms[l] += start.elapsed().as_millis() as u64;
                return Some(Permit(self));
            }
            let left = timeout.checked_sub(start.elapsed())?;
            let (g, r) = self.cv.wait_timeout(s, left).unwrap();
            s = g;
            if r.timed_out() && start.elapsed() >= timeout {
                s.waiting[l] -= 1;
                self.cv.notify_all();
                return None;
            }
        }
    }

    pub fn stats(&self) -> serde_json::Value {
        let s = self.st.lock().unwrap();
        let n = ["query", "ingest", "fill"];
        let mut o = serde_json::Map::new();
        for (i, name) in n.iter().enumerate() {
            o.insert(
                (*name).into(),
                serde_json::json!({"waiting": s.waiting[i], "served": s.served[i],
                    "avg_wait_ms": s.wait_ms[i].checked_div(s.served[i]).unwrap_or(0)}),
            );
        }
        serde_json::Value::Object(o)
    }
}

pub static EMBED_GATE: Gate = Gate::new(2);
pub static RERANK_GATE: Gate = Gate::new(2);

pub fn timeout_for(l: Lane) -> Duration {
    match l {
        Lane::Query => Duration::from_millis(800),
        _ => Duration::from_secs(600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_beats_fill_and_times_out() {
        let g = Gate::new(1);
        let p = g.acquire(Lane::Fill, Duration::from_secs(1)).unwrap();
        assert!(g.acquire(Lane::Query, Duration::from_millis(30)).is_none());
        drop(p);
        assert!(g.acquire(Lane::Query, Duration::from_millis(30)).is_some());
    }
}
