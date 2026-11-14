//! Rolling record of the last N search latencies for /stats.

use std::collections::VecDeque;

pub struct Latencies {
    window: VecDeque<f64>,
    cap: usize,
    pub count: u64,
}

impl Latencies {
    pub fn new(cap: usize) -> Self {
        Self { window: VecDeque::with_capacity(cap), cap, count: 0 }
    }

    pub fn record(&mut self, ms: f64) {
        if self.window.len() == self.cap {
            self.window.pop_front();
        }
        self.window.push_back(ms);
        self.count += 1;
    }

    /// Nearest-rank percentile (0–100) over the window.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.window.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
        Some(v[rank.clamp(1, v.len()) - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let mut l = Latencies::new(1000);
        assert_eq!(l.percentile(50.0), None);
        for ms in 1..=100 {
            l.record(f64::from(ms));
        }
        assert_eq!(l.percentile(50.0), Some(50.0));
        assert_eq!(l.percentile(95.0), Some(95.0));
        assert_eq!(l.percentile(100.0), Some(100.0));
    }

    #[test]
    fn window_drops_oldest() {
        let mut l = Latencies::new(3);
        for ms in [100.0, 1.0, 2.0, 3.0] {
            l.record(ms);
        }
        assert_eq!(l.percentile(100.0), Some(3.0));
        assert_eq!(l.count, 4);
    }
}
