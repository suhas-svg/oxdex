//! Micro-benchmarks for the inner loops the spec names: tokenizing, VByte
//! decoding, posting-list intersection and BM25 scoring.
//!
//!     cargo bench -p oxdex-query --bench hot_paths

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxdex_index::postings::{self, BlockCursor};
use oxdex_index::{DocId, NO_MORE_DOCS, PostingCursor, vbyte};
use oxdex_query::{Bm25, snippet};
use oxdex_text::Analyzer;

const PARAGRAPH: &str = "A field-programmable gate array (FPGA) is an integrated circuit designed to be \
configured by a customer or a designer after manufacturing, hence the term field-programmable. The FPGA \
configuration is generally specified using a hardware description language (HDL), similar to that used for \
an application-specific integrated circuit (ASIC). Timing closure, clock domain crossing and placement are \
the hard parts of building large designs on modern devices from vendors such as Xilinx and Altera.";

/// Deterministic pseudo-random sorted doc list with roughly `n` entries.
fn list(n: usize, seed: u64, universe: u32) -> (Vec<DocId>, Vec<u32>) {
    let mut x = seed;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut docs: Vec<DocId> = (0..n).map(|_| (next() % u64::from(universe)) as DocId).collect();
    docs.sort_unstable();
    docs.dedup();
    let tfs = docs.iter().map(|_| 1 + (next() % 4) as u32).collect();
    (docs, tfs)
}

fn tokenize(c: &mut Criterion) {
    let text = PARAGRAPH.repeat(20); // ~9 KB, a typical article body
    let mut g = c.benchmark_group("tokenize");
    g.throughput(Throughput::Bytes(text.len() as u64));
    g.bench_function("analyze_9kb", |b| b.iter(|| Analyzer::global().tokens(black_box(&text)).count()));
    g.finish();
}

fn vbyte_decode(c: &mut Criterion) {
    let mut g = c.benchmark_group("postings");
    let (docs, tfs) = list(100_000, 7, 7_000_000);
    let mut enc = Vec::new();
    postings::encode(&docs, &tfs, &mut enc);
    let df = docs.len() as u32;

    let mut raw = Vec::new();
    for &d in &docs {
        vbyte::encode(d, &mut raw);
    }
    g.throughput(Throughput::Elements(docs.len() as u64));
    g.bench_function("vbyte_decode_100k", |b| {
        b.iter(|| {
            let (mut pos, mut sum) = (0usize, 0u64);
            while let Some(v) = vbyte::decode(black_box(&raw), &mut pos) {
                sum += u64::from(v);
            }
            sum
        })
    });
    g.bench_function("cursor_walk_100k", |b| {
        b.iter(|| {
            let mut c = BlockCursor::new(black_box(&enc), df);
            let mut sum = 0u64;
            while c.doc() != NO_MORE_DOCS {
                sum += u64::from(c.tf());
                c.next();
            }
            sum
        })
    });
    g.finish();
}

fn intersect(c: &mut Criterion) {
    let mut g = c.benchmark_group("intersect");
    // A rare list against a common one: skip pointers should make the cost
    // track the rare list, not the common one.
    for (rare, common) in [(1_000usize, 1_000_000usize), (50_000, 1_000_000), (500_000, 1_000_000)] {
        let (a, at) = list(rare, 11, 7_000_000);
        let (b, bt) = list(common, 13, 7_000_000);
        let (mut ea, mut eb) = (Vec::new(), Vec::new());
        postings::encode(&a, &at, &mut ea);
        postings::encode(&b, &bt, &mut eb);
        let (dfa, dfb) = (a.len() as u32, b.len() as u32);
        g.bench_with_input(BenchmarkId::new("leapfrog", format!("{rare}x{common}")), &(), |bch, _| {
            bch.iter(|| {
                let mut x = BlockCursor::new(&ea, dfa);
                let mut y = BlockCursor::new(&eb, dfb);
                let mut hits = 0u32;
                while x.doc() != NO_MORE_DOCS {
                    y.advance(x.doc());
                    if y.doc() == NO_MORE_DOCS {
                        break;
                    }
                    if y.doc() == x.doc() {
                        hits += 1;
                        x.next();
                    } else {
                        x.advance(y.doc());
                    }
                }
                hits
            })
        });
    }
    g.finish();
}

fn bm25(c: &mut Criterion) {
    let bm = Bm25::default();
    let (docs, tfs) = list(100_000, 17, 7_000_000);
    let lens: Vec<u32> = docs.iter().map(|d| 50 + d % 2000).collect();
    let mut g = c.benchmark_group("bm25");
    g.throughput(Throughput::Elements(docs.len() as u64));
    g.bench_function("term_score_100k", |b| {
        b.iter(|| {
            tfs.iter()
                .zip(&lens)
                .map(|(&tf, &dl)| bm.term_score(black_box(3.2), tf, dl, 420.0))
                .sum::<f64>()
        })
    });
    g.finish();
}

fn snippets(c: &mut Criterion) {
    let text = PARAGRAPH.repeat(90); // ~40 KB: the most a snippet scans
    let terms: Vec<String> = ["xilinx", "timing"].iter().map(|w| Analyzer::global().term(w)).collect();
    c.bench_function("snippet_40kb", |b| b.iter(|| snippet(black_box(&text), &terms, 160)));
}

criterion_group!(benches, tokenize, vbyte_decode, intersect, bm25, snippets);
criterion_main!(benches);
