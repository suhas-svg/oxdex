use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::Hit;

/// Keeps the best `k` hits seen so far in a min-heap of size `k`.
///
/// Pushing is O(log k) and only happens when a hit beats the current worst, so
/// scanning millions of candidates never sorts them all.
pub struct TopK {
    k: usize,
    heap: BinaryHeap<Ranked>,
}

/// Ordered so the heap's top is the *worst* kept hit: lowest score, then
/// highest doc ID (higher doc IDs lose ties).
#[derive(Debug, Clone, Copy)]
struct Ranked(Hit);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        // "Greater" = worse, so BinaryHeap (a max-heap) exposes the worst hit.
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then(self.0.doc.cmp(&other.0.doc))
    }
}

impl TopK {
    pub fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k + 1),
        }
    }

    pub fn push(&mut self, hit: Hit) {
        if self.k == 0 {
            return;
        }
        let candidate = Ranked(hit);
        if self.heap.len() < self.k {
            self.heap.push(candidate);
        } else if let Some(mut worst) = self.heap.peek_mut()
            && candidate < *worst
        {
            *worst = candidate;
        }
    }

    /// Score a new hit must reach to get in, once `k` hits are held.
    pub fn threshold(&self) -> Option<f64> {
        if self.heap.len() < self.k { None } else { self.heap.peek().map(|r| r.0.score) }
    }

    /// Best hit first.
    pub fn into_sorted_vec(self) -> Vec<Hit> {
        // into_sorted_vec is ascending by Ord, i.e. best (least "worse") first.
        self.heap.into_sorted_vec().into_iter().map(|r| r.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(doc: u32, score: f64) -> Hit {
        Hit { doc, score }
    }

    #[test]
    fn keeps_best_k_in_order() {
        let mut top = TopK::new(3);
        for (doc, score) in [(0, 1.0), (1, 5.0), (2, 3.0), (3, 4.0), (4, 0.5), (5, 2.0)] {
            top.push(hit(doc, score));
        }
        let docs: Vec<u32> = top.into_sorted_vec().iter().map(|h| h.doc).collect();
        assert_eq!(docs, [1, 3, 2]);
    }

    #[test]
    fn ties_prefer_lower_doc_id() {
        let mut top = TopK::new(2);
        for doc in [7, 3, 9, 1] {
            top.push(hit(doc, 1.0));
        }
        let docs: Vec<u32> = top.into_sorted_vec().iter().map(|h| h.doc).collect();
        assert_eq!(docs, [1, 3]);
    }

    #[test]
    fn matches_full_sort() {
        // Deterministic pseudo-random scores, compared with sorting everything.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let hits: Vec<Hit> = (0..1000)
            .map(|doc| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                hit(doc, (x % 50) as f64)
            })
            .collect();

        let mut top = TopK::new(10);
        hits.iter().for_each(|h| top.push(*h));

        let mut all = hits.clone();
        all.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
        all.truncate(10);
        assert_eq!(top.into_sorted_vec(), all);
    }

    #[test]
    fn k_zero_and_fewer_than_k() {
        let mut none = TopK::new(0);
        none.push(hit(0, 1.0));
        assert!(none.into_sorted_vec().is_empty());

        let mut few = TopK::new(10);
        few.push(hit(4, 2.0));
        few.push(hit(2, 3.0));
        assert_eq!(few.into_sorted_vec(), [hit(2, 3.0), hit(4, 2.0)]);
    }
}
