//! Query parsing, matching, BM25 ranking and snippets.
//!
//! - [`parse`]: `fpga timing -xilinx`, `verilog OR vhdl`, stopword handling.
//! - [`search_fields`]: AND (skip-pointer leapfrog) / OR (doc-at-a-time) over
//!   body and title fields, `-exclude`, title boost, AND → OR fallback.
//! - [`snippet`]: best ~160-character window with `<mark>` highlights.
//! - [`search`]: the plain M1–M3 entry point, OR over a single field.

mod bm25;
mod engine;
mod parse;
mod snippet;
mod topk;

pub use bm25::Bm25;
pub use engine::{FieldResult, Fields, MatchMode, Params, search_fields};
pub use parse::{Mode, ParsedQuery, is_stopword, parse};
pub use snippet::snippet;
pub use topk::TopK;

use oxdex_index::{DocId, SearchIndex};
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

/// Scores every document containing any query term with BM25 over a single
/// field (no stopword dropping, no title field) and returns the best `k`.
/// Ties are broken by lower doc ID so results are deterministic.
pub fn search<I: SearchIndex>(index: &I, query: &str, k: usize, bm25: &Bm25) -> SearchResult {
    let parsed = ParsedQuery {
        terms: query_terms(query),
        excluded: Vec::new(),
        mode: Mode::Any,
        dropped_stopwords: Vec::new(),
    };
    let params = Params { bm25: *bm25, ..Params::default() };
    let r = search_fields(&Fields { body: index, title: None }, &parsed, k, 0, &params);
    SearchResult {
        hits: r.hits,
        total_hits: r.total_hits,
    }
}
