use std::collections::VecDeque;

const P50: f64 = 0.50;
const P95: f64 = 0.95;

#[derive(Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub n: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

/// Fixed-size window of the most recent recall durations (milliseconds).
pub struct LatencyWindow {
    cap: usize,
    samples: VecDeque<u64>,
}

impl LatencyWindow {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            samples: VecDeque::with_capacity(cap),
        }
    }

    pub fn record(&mut self, ms: u64) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    pub fn summary(&self) -> Option<LatencySummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let rank =
            |q: f64| sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1];
        Some(LatencySummary {
            n: sorted.len(),
            p50_ms: rank(P50),
            p95_ms: rank(P95),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::LatencyWindow;

    #[test]
    fn empty_window_has_no_percentiles() {
        let w = LatencyWindow::new(4);
        assert_eq!(w.summary(), None);
    }

    #[test]
    fn p95_uses_nearest_rank() {
        let mut w = LatencyWindow::new(100);
        for ms in 1..=100 {
            w.record(ms);
        }
        let s = w.summary().unwrap();
        assert_eq!((s.n, s.p50_ms, s.p95_ms), (100, 50, 95));
    }

    #[test]
    fn window_keeps_only_most_recent_samples() {
        let mut w = LatencyWindow::new(3);
        for ms in [900, 1, 2, 3] {
            w.record(ms);
        }
        let s = w.summary().unwrap();
        assert_eq!((s.n, s.p95_ms), (3, 3));
    }
}
