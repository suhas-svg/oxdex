/// BM25 parameters.
///
/// score(D, Q) = Σ IDF(q) · f(q,D)·(k1+1) / (f(q,D) + k1·(1 − b + b·|D|/avgdl))
/// IDF(q)      = ln(1 + (N − n(q) + 0.5) / (n(q) + 0.5))
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25 {
    /// How fast repeated occurrences stop adding score.
    pub k1: f64,
    /// How strongly long documents are penalized (0 = not at all, 1 = fully).
    pub b: f64,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25 {
    /// Inverse document frequency. Always positive with the +1 inside the log.
    pub fn idf(num_docs: u32, doc_freq: u32) -> f64 {
        let n = f64::from(num_docs);
        let df = f64::from(doc_freq);
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// One term's contribution to one document's score.
    pub fn term_score(&self, idf: f64, tf: u32, doc_len: u32, avgdl: f64) -> f64 {
        if tf == 0 {
            return 0.0;
        }
        let tf = f64::from(tf);
        let len_ratio = if avgdl > 0.0 { f64::from(doc_len) / avgdl } else { 1.0 };
        let norm = self.k1 * (1.0 - self.b + self.b * len_ratio);
        idf * tf * (self.k1 + 1.0) / (tf + norm)
    }
}

#[cfg(test)]
mod tests {
    use super::Bm25;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn idf_matches_hand_computation() {
        // N = 10, n = 2: ln(1 + 8.5 / 2.5) = ln(4.4)
        assert!(close(Bm25::idf(10, 2), 4.4f64.ln()));
        // A term in every doc still has a small positive weight.
        assert!(Bm25::idf(10, 10) > 0.0);
        // Rarer terms weigh more.
        assert!(Bm25::idf(1000, 1) > Bm25::idf(1000, 500));
    }

    #[test]
    fn term_score_matches_hand_computation() {
        let bm = Bm25::default();
        // tf = 2, |D| = avgdl → norm = k1 = 1.2; 2·2.2 / 3.2 = 1.375
        assert!(close(bm.term_score(1.0, 2, 100, 100.0), 1.375));
        // tf = 1, |D| = 2·avgdl → norm = 1.2·(0.25 + 1.5) = 2.1; 2.2 / 3.1
        assert!(close(bm.term_score(1.0, 1, 200, 100.0), 2.2 / 3.1));
    }

    #[test]
    fn tf_saturates() {
        let bm = Bm25::default();
        let s1 = bm.term_score(1.0, 1, 100, 100.0);
        let s2 = bm.term_score(1.0, 2, 100, 100.0);
        let s3 = bm.term_score(1.0, 3, 100, 100.0);
        let s100 = bm.term_score(1.0, 100, 100, 100.0);
        assert!(s1 < s2 && s2 < s3, "more occurrences still help");
        assert!(s2 - s1 > s3 - s2, "each extra occurrence helps less");
        assert!(s100 < bm.k1 + 1.0, "bounded by k1 + 1");
    }

    #[test]
    fn longer_docs_score_lower() {
        let bm = Bm25::default();
        assert!(bm.term_score(1.0, 3, 50, 100.0) > bm.term_score(1.0, 3, 300, 100.0));
    }

    #[test]
    fn b_zero_ignores_length() {
        let bm = Bm25 { k1: 1.2, b: 0.0 };
        assert!(close(bm.term_score(1.0, 3, 10, 100.0), bm.term_score(1.0, 3, 1000, 100.0)));
    }
}
