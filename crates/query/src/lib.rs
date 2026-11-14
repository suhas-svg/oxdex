//! Query evaluation and BM25 ranking.
//!
//! M1 scores every document that contains at least one query term (OR
//! semantics). AND / OR / -exclude parsing arrives in M4.

mod bm25;
mod topk;

pub use bm25::Bm25;
pub use topk::TopK;

use oxdex_index::{DocId, InMemoryIndex};
use oxdex_text::Analyzer;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Best hits first, at most `k` of them.
    pub hits: Vec<Hit>,
    /// Number of documents that matched at least one term.
    pub total_hits: usize,
}

/// Turns a query string into its distinct terms, in first-seen order.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for t in Analyzer::global().tokens(query) {
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    terms
}

/// Scores all matching documents with BM25 and returns the best `k`.
///
/// Ties are broken by lower doc ID so results are deterministic.
pub fn search(index: &InMemoryIndex, query: &str, k: usize, bm25: &Bm25) -> SearchResult {
    let n = index.num_docs();
    let avgdl = index.avgdl();

    // Dense doc IDs let us accumulate scores in a plain array.
    let mut scores = vec![0.0f64; n as usize];
    let mut touched: Vec<DocId> = Vec::new();

    for term in query_terms(query) {
        let postings = index.postings(&term);
        if postings.is_empty() {
            continue;
        }
        let idf = Bm25::idf(n, postings.len() as u32);
        for p in postings {
            let slot = &mut scores[p.doc as usize];
            if *slot == 0.0 {
                touched.push(p.doc);
            }
            *slot += bm25.term_score(idf, p.tf, index.doc_len(p.doc), avgdl);
        }
    }

    let mut top = TopK::new(k);
    for &doc in &touched {
        top.push(Hit {
            doc,
            score: scores[doc as usize],
        });
    }
    SearchResult {
        hits: top.into_sorted_vec(),
        total_hits: touched.len(),
    }
}
