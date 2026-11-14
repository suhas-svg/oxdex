//! Golden ranking tests: 10 hand-written docs, 5 queries, known correct order.
//!
//! Each expected order follows from one BM25 property (noted per query), so a
//! formula mistake — wrong log, swapped k1 and b, missing length norm — breaks
//! at least one of them. A brute-force scorer cross-checks the exact scores.

use oxdex_index::InMemoryIndex;
use oxdex_query::{Bm25, query_terms, search};
use oxdex_text::tokenize;

const DOCS: [&str; 10] = [
    /* 0 */ "verilog verilog verilog hardware design",
    /* 1 */ "verilog hardware design notes today",
    /* 2 */ "fpga timing closure",
    /* 3 */ "system system timing",
    /* 4 */ "fpga timing results for a much larger board with extra words",
    /* 5 */ "runs running runner",
    /* 6 */ "clock domain crossing synchronizer",
    /* 7 */ "clock tree design",
    /* 8 */ "system design system architecture system",
    /* 9 */ "domain names and web hosting",
];

fn build() -> InMemoryIndex {
    let mut idx = InMemoryIndex::new();
    for (i, text) in DOCS.iter().enumerate() {
        // Neutral one-token titles ("d0", "d1", …) so titles never match a query.
        idx.add(&format!("d{i}"), text);
    }
    idx
}

fn ranked(idx: &InMemoryIndex, q: &str) -> Vec<u32> {
    search(idx, q, 10, &Bm25::default()).hits.iter().map(|h| h.doc).collect()
}

#[test]
fn term_frequency_wins_at_equal_length() {
    // d0 has "verilog" 3×, d1 once; both are 6 tokens long.
    assert_eq!(ranked(&build(), "verilog"), [0, 1]);
}

#[test]
fn short_docs_and_term_coverage() {
    // d2 and d4 both match both terms, but d4 is long; d3 matches only "timing".
    assert_eq!(ranked(&build(), "fpga timing"), [2, 4, 3]);
}

#[test]
fn query_is_stemmed_like_documents() {
    // "running" → "run" matches "runs"/"running" in d5. Porter2 leaves "runner"
    // as "runner", so it is not a match on its own.
    assert_eq!(ranked(&build(), "running"), [5]);
    assert_eq!(ranked(&build(), "RUNS"), [5]);
}

#[test]
fn coverage_then_length() {
    // d6 has all three terms. d7 ("clock") and d9 ("domain") each match one
    // term with the same df; d7 is shorter, so it ranks above d9.
    assert_eq!(ranked(&build(), "clock domain crossing"), [6, 7, 9]);
}

#[test]
fn rare_terms_outweigh_common_ones() {
    // "system" (df 2) is rarer than "design" (df 4). d8 has both; d3 has only
    // the rare one and beats d7, which has only the common one at equal length.
    // d0 and d1 tie exactly and fall back to doc ID order.
    assert_eq!(ranked(&build(), "system design"), [8, 3, 7, 0, 1]);
}

#[test]
fn no_matches_and_empty_query() {
    let idx = build();
    let r = search(&idx, "nonexistentterm", 10, &Bm25::default());
    assert!(r.hits.is_empty());
    assert_eq!(r.total_hits, 0);
    assert!(search(&idx, "", 10, &Bm25::default()).hits.is_empty());
    assert!(search(&idx, "  ?! ", 10, &Bm25::default()).hits.is_empty());
}

#[test]
fn k_limits_results_but_not_total_hits() {
    let r = search(&build(), "system design", 2, &Bm25::default());
    assert_eq!(r.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), [8, 3]);
    assert_eq!(r.total_hits, 5);
}

#[test]
fn repeated_query_terms_count_once() {
    let idx = build();
    let once = search(&idx, "verilog", 10, &Bm25::default());
    let twice = search(&idx, "verilog Verilog", 10, &Bm25::default());
    assert_eq!(once.hits, twice.hits);
}

/// Recomputes BM25 straight from the formula by re-tokenizing every document,
/// with no posting lists involved, and checks `search` agrees exactly.
#[test]
fn scores_match_brute_force_reference() {
    let idx = build();
    let bm = Bm25::default();
    let doc_tokens: Vec<Vec<String>> = DOCS
        .iter()
        .enumerate()
        .map(|(i, t)| tokenize(&format!("d{i} {t}")))
        .collect();
    let n = doc_tokens.len() as f64;
    let avgdl = doc_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n;

    for q in ["verilog", "fpga timing", "running", "clock domain crossing", "system design"] {
        let terms = query_terms(q);
        let mut expected: Vec<(u32, f64)> = Vec::new();
        for (doc, toks) in doc_tokens.iter().enumerate() {
            let mut score = 0.0;
            for t in &terms {
                let tf = toks.iter().filter(|x| *x == t).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = doc_tokens.iter().filter(|d| d.contains(t)).count() as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let dl = toks.len() as f64;
                score += idf * tf * (bm.k1 + 1.0) / (tf + bm.k1 * (1.0 - bm.b + bm.b * dl / avgdl));
            }
            if score > 0.0 {
                expected.push((doc as u32, score));
            }
        }
        expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let got: Vec<(u32, f64)> = search(&idx, q, 10, &bm).hits.iter().map(|h| (h.doc, h.score)).collect();
        assert_eq!(got.len(), expected.len(), "query {q:?}");
        for ((gd, gs), (ed, es)) in got.iter().zip(&expected) {
            assert_eq!(gd, ed, "query {q:?}");
            assert!((gs - es).abs() < 1e-9, "query {q:?} doc {gd}: {gs} vs {es}");
        }
    }
}
