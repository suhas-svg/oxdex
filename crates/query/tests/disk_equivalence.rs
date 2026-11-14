//! The on-disk index must rank exactly like the in-memory reference: same
//! docs, same order, bit-identical scores, same hit counts.

use std::path::PathBuf;

use oxdex_index::{DiskIndex, InMemoryIndex, IndexBuilder, count_terms};
use oxdex_query::{Bm25, search};

fn sample_docs() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/mini");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let raw = std::fs::read_to_string(p).unwrap();
            let (title, body) = raw.split_once('\n').unwrap();
            (title.trim().to_owned(), body.to_owned())
        })
        .collect()
}

#[test]
fn disk_and_memory_rank_identically() {
    let docs = sample_docs();
    assert_eq!(docs.len(), 20);

    let mut mem = InMemoryIndex::new();
    let dir = tempfile::tempdir().unwrap();
    // A tiny budget forces several segments and a real merge.
    let mut builder = IndexBuilder::create(dir.path(), 2048).unwrap();
    for (title, body) in &docs {
        mem.add(title, body);
        let (terms, len) = count_terms(title, body);
        builder.add_doc(&terms, len).unwrap();
    }
    let stats = builder.finish().unwrap();
    assert!(stats.num_segments > 1);
    let disk = DiskIndex::open(dir.path()).unwrap();

    let bm = Bm25::default();
    for q in [
        "fpga timing",
        "chip that can be rewired after manufacturing",
        "memory",
        "rust compiler garbage collection",
        "IEEE 802.3",
        "the",
        "nothing-matches-this",
        "",
    ] {
        let a = search(&mem, q, 10, &bm);
        let b = search(&disk, q, 10, &bm);
        assert_eq!(a.total_hits, b.total_hits, "query {q:?}");
        assert_eq!(a.hits.len(), b.hits.len(), "query {q:?}");
        for (x, y) in a.hits.iter().zip(&b.hits) {
            assert_eq!(x.doc, y.doc, "query {q:?}");
            assert_eq!(x.score.to_bits(), y.score.to_bits(), "query {q:?}");
        }
    }
}
