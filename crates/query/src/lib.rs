//! Query evaluation and BM25 ranking.
//!
//! Search walks all query terms' posting lists together, document at a time
//! (always the smallest current doc ID), so memory stays O(terms + k) no matter
//! how large the corpus is. M3 uses OR semantics; AND / -exclude parsing and
//! skip-pointer intersection arrive in M4.

mod bm25;
mod topk;

pub use bm25::Bm25;
pub use topk::TopK;

use oxdex_index::{DocId, NO_MORE_DOCS, PostingCursor, SearchIndex};
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

/// Scores every document containing any query term with BM25 and returns the
/// best `k`. Ties are broken by lower doc ID so results are deterministic.
pub fn search<I: SearchIndex>(index: &I, query: &str, k: usize, bm25: &Bm25) -> SearchResult {
    let n = index.num_docs();
    let avgdl = index.avgdl();

    // Per term, in query order: its IDF and a cursor over its postings.
    let mut lists: Vec<(f64, I::Cursor<'_>)> = query_terms(query)
        .iter()
        .filter_map(|t| index.cursor(t))
        .map(|c| (Bm25::idf(n, c.df()), c))
        .collect();

    let mut top = TopK::new(k);
    let mut total_hits = 0usize;
    loop {
        let doc = lists.iter().map(|(_, c)| c.doc()).min().unwrap_or(NO_MORE_DOCS);
        if doc == NO_MORE_DOCS {
            break;
        }
        let doc_len = index.doc_len(doc);
        let mut score = 0.0;
        for (idf, c) in lists.iter_mut() {
            if c.doc() == doc {
                score += bm25.term_score(*idf, c.tf(), doc_len, avgdl);
                c.next();
            }
        }
        total_hits += 1;
        top.push(Hit { doc, score });
    }

    SearchResult {
        hits: top.into_sorted_vec(),
        total_hits,
    }
}
