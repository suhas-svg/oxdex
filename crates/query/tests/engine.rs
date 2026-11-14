//! Matching and field scoring checked against simple set-based versions.

use std::collections::BTreeSet;

use oxdex_index::{DiskIndex, InMemoryIndex, IndexBuilder, count_terms};
use oxdex_query::{Fields, MatchMode, Mode, Params, ParsedQuery, parse, search_fields};
use proptest::prelude::*;

fn terms_of(text: &str) -> BTreeSet<String> {
    count_terms("", text).0.into_iter().map(|(t, _)| t).collect()
}

fn q(terms: &[&str], excluded: &[&str], mode: Mode) -> ParsedQuery {
    ParsedQuery {
        terms: terms.iter().map(|s| s.to_string()).collect(),
        excluded: excluded.iter().map(|s| s.to_string()).collect(),
        mode,
        dropped_stopwords: Vec::new(),
    }
}

/// Params with the AND → OR fallback switched off, to test pure AND.
fn strict() -> Params {
    Params { min_and_hits: 0, ..Params::default() }
}

const VOCAB: &[&str] = &["alpha", "beta", "gamma", "delta", "omega", "sigma"];

fn corpus() -> impl Strategy<Value = Vec<(String, String)>> {
    let doc = (
        prop::collection::vec(prop::sample::select(VOCAB), 0..3),
        prop::collection::vec(prop::sample::select(VOCAB), 0..12),
    )
        .prop_map(|(t, b)| (t.join(" "), b.join(" ")));
    // Enough docs that some lists span several 128-posting blocks on disk.
    prop::collection::vec(doc, 1..700)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn matching_equals_set_semantics(
        docs in corpus(),
        terms in prop::collection::vec(prop::sample::select(VOCAB), 1..4),
        excluded in prop::collection::vec(prop::sample::select(VOCAB), 0..2),
        any in any::<bool>(),
    ) {
        let mut body = InMemoryIndex::new();
        let mut title = InMemoryIndex::new();
        let mut body_sets = Vec::new();
        let mut title_sets = Vec::new();
        for (t, b) in &docs {
            body.add(t, b);
            title.add(t, "");
            body_sets.push(terms_of(&format!("{t} {b}")));
            title_sets.push(terms_of(t));
        }
        let mut terms: Vec<&str> = terms;
        terms.dedup();
        let excluded: Vec<&str> = excluded.into_iter().filter(|e| !terms.contains(e)).collect();
        let mode = if any { Mode::Any } else { Mode::All };
        let parsed = q(&terms, &excluded, mode);

        let expected: BTreeSet<u32> = (0..docs.len())
            .filter(|&d| {
                let has = |t: &&str| body_sets[d].contains(*t) || title_sets[d].contains(*t);
                let hit = if any { terms.iter().any(has) } else { terms.iter().all(has) };
                hit && !excluded.iter().any(|e| body_sets[d].contains(*e))
            })
            .map(|d| d as u32)
            .collect();

        let fields = Fields { body: &body, title: Some(&title) };
        let r = search_fields(&fields, &parsed, docs.len() + 1, 0, &strict());
        let got: BTreeSet<u32> = r.hits.iter().map(|h| h.doc).collect();
        prop_assert_eq!(&got, &expected);
        prop_assert_eq!(r.total_hits, expected.len());
        prop_assert!(r.hits.windows(2).all(|w| w[0].score >= w[1].score));

        // Paging returns the same ranking, sliced.
        let page = search_fields(&fields, &parsed, 3, 2, &strict());
        let want: Vec<u32> = r.hits.iter().skip(2).take(3).map(|h| h.doc).collect();
        prop_assert_eq!(page.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), want);

        // The on-disk index (block cursors, skip pointers) agrees exactly.
        let dir = tempfile::tempdir().unwrap();
        let (bdir, tdir) = (dir.path().join("body"), dir.path().join("title"));
        let mut bb = IndexBuilder::create(&bdir, 4096).unwrap();
        let mut tb = IndexBuilder::create(&tdir, 4096).unwrap();
        for (t, b) in &docs {
            let (terms, len) = count_terms(t, b);
            bb.add_doc(&terms, len).unwrap();
            let (terms, len) = count_terms(t, "");
            tb.add_doc(&terms, len).unwrap();
        }
        bb.finish().unwrap();
        tb.finish().unwrap();
        let (db, dt) = (DiskIndex::open(&bdir).unwrap(), DiskIndex::open(&tdir).unwrap());
        let d = search_fields(&Fields { body: &db, title: Some(&dt) }, &parsed, docs.len() + 1, 0, &strict());
        prop_assert_eq!(d.total_hits, r.total_hits);
        prop_assert_eq!(
            d.hits.iter().map(|h| (h.doc, h.score.to_bits())).collect::<Vec<_>>(),
            r.hits.iter().map(|h| (h.doc, h.score.to_bits())).collect::<Vec<_>>()
        );
    }
}

fn build(docs: &[(&str, &str, &str)]) -> (InMemoryIndex, InMemoryIndex) {
    // (title, title-field text incl. redirects, body)
    let mut body = InMemoryIndex::new();
    let mut title = InMemoryIndex::new();
    for (t, tf, b) in docs {
        body.add(t, b);
        title.add(tf, "");
    }
    (body, title)
}

#[test]
fn title_boost_ranks_the_named_article_first() {
    let (body, title) = build(&[
        ("Logic block", "Logic block", "An fpga is built from many logic blocks. Each fpga logic block fpga fpga."),
        ("Field-programmable gate array", "Field-programmable gate array FPGA", "A programmable chip. An fpga can be rewired."),
    ]);
    let p = parse("fpga");
    let no_title = search_fields(&Fields { body: &body, title: None }, &p, 10, 0, &Params::default());
    assert_eq!(no_title.hits[0].doc, 0, "body alone favors the doc that repeats the word");

    let with_title = search_fields(&Fields { body: &body, title: Some(&title) }, &p, 10, 0, &Params::default());
    assert_eq!(with_title.hits[0].doc, 1, "the redirect title FPGA lifts the real article");
}

#[test]
fn redirect_title_matches_even_when_body_lacks_the_word() {
    let (body, title) = build(&[
        ("United States", "United States USA US America", "A country in North America."),
        ("Canada", "Canada", "A country north of the United States."),
    ]);
    let r = search_fields(&Fields { body: &body, title: Some(&title) }, &parse("usa"), 10, 0, &Params::default());
    assert_eq!(r.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), [0]);
}

#[test]
fn and_falls_back_to_or_when_too_specific() {
    let docs: Vec<(String, String)> =
        (0..30).map(|i| (format!("d{i}"), if i < 3 { "rust compiler".into() } else { "rust".into() })).collect();
    let mut body = InMemoryIndex::new();
    for (t, b) in &docs {
        body.add(t, b);
    }
    let fields = Fields { body: &body, title: None };

    let strict_and = search_fields(&fields, &parse("rust compiler"), 10, 0, &strict());
    assert_eq!((strict_and.total_hits, strict_and.mode), (3, MatchMode::And));

    let fallback = search_fields(&fields, &parse("rust compiler"), 10, 0, &Params::default());
    assert_eq!(fallback.mode, MatchMode::OrFallback);
    assert_eq!(fallback.total_hits, 30);
    let top3: BTreeSet<u32> = fallback.hits[..3].iter().map(|h| h.doc).collect();
    assert_eq!(top3, BTreeSet::from([0, 1, 2]), "docs with both terms still rank first");

    let missing = search_fields(&fields, &parse("rust nonexistent"), 10, 0, &Params::default());
    assert_eq!(missing.mode, MatchMode::OrFallback);
    assert_eq!(missing.total_hits, 30);
}

#[test]
fn exclusion_and_or_keyword() {
    let mut body = InMemoryIndex::new();
    for b in ["fpga timing xilinx", "fpga timing altera", "verilog", "vhdl", "fpga"] {
        body.add("", b);
    }
    let f = Fields { body: &body, title: None };
    let r = search_fields(&f, &parse("fpga timing -xilinx"), 10, 0, &strict());
    assert_eq!(r.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), [1]);

    let r = search_fields(&f, &parse("verilog OR vhdl"), 10, 0, &Params::default());
    assert_eq!(r.mode, MatchMode::Or);
    assert_eq!(r.total_hits, 2);
}
