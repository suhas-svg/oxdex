# Oxdex

A full-text search engine written from scratch in Rust: tokenizer, inverted index, BM25 ranking. The goal is to index all of English Wikipedia and answer keyword queries in under 50 ms at p95.

**Status: M2 (Wikipedia ingestion) done.** Simple English Wikipedia ingests into a compressed document store. Search still runs on the M1 in-memory engine.

## Try it

```bash
# M1: search a folder of text files
cargo run -p oxdex-mini -- data/mini fpga timing

# M2: ingest a multistream dump (download both files from dumps.wikimedia.org/simplewiki/latest/)
cargo run --release -p oxdex-ingest -- build \
  --dump data/dumps/simplewiki-latest-pages-articles-multistream.xml.bz2 --out data/out/simple
cargo run --release -p oxdex-ingest -- sample --store data/out/simple -n 20
cargo run --release -p oxdex-ingest -- audit  --store data/out/simple

cargo test --workspace
```

## Numbers so far

Simple English Wikipedia, dump of 2026-11-02, Zephyrus G16 (12 threads), release build.

| Metric | Result |
| --- | --- |
| Dump size (bz2) | 390 MB, 5,636 streams, 563,515 pages |
| Ingest time (dump → doc store) | 19.0 s |
| Articles stored | 286,043 |
| Skipped: other namespaces / redirects / empty after cleaning | 161,807 / 115,575 / 90 |
| Cleaner panics / bad streams | 0 / 0 |
| Clean text → `docs.bin` | 332 MB → 124 MB (zstd, 64 KiB blocks) |
| Outgoing links saved | 3.9 M |
| Articles with no leftover markup (`audit`) | 99.23% |

## Layout

| Path | What it is |
| --- | --- |
| `crates/text` | Shared tokenizer: Unicode word split, lowercase, Porter2 stem, drop tokens > 64 chars |
| `crates/wikitext` | Multistream index + stream reader, streaming `<page>` parser, wikitext cleaner |
| `crates/docstore` | `docs.bin` / `docs.idx` / `links.bin`: dense doc IDs, zstd blocks, mmap reader |
| `crates/index` | Inverted index (in-memory for now; on-disk in M3) |
| `crates/query` | BM25 scoring (k1 = 1.2, b = 0.75), top-k min-heap, golden ranking tests |
| `services/ingest` | `oxdex-ingest build / sample / audit / show` |
| `services/mini` | M1 CLI over a folder of `.txt` files (first line = title) |
| `data/mini` | 20 sample articles |

## Known cleaner gaps

Accepted under the one-week time box:

- Templates that render text, like `{{convert|4|C}}`, are dropped, so their values go missing.
- Section headings that only held links or templates ("References", "Other websites") remain as bare lines.
- Bare URLs in prose are kept (0.8% of articles).

## Milestones

- [x] M1: mini engine, golden ranking tests pass
- [x] M2: Wikipedia ingestion: zero panics, skip counts logged, 20 random articles read cleanly
- [ ] M3: on-disk index (segments, k-way merge, VByte + skip pointers, mmap)
- [ ] M4: query service and UI (axum, AND/OR/-exclude, snippets, title boost)
- [ ] M5: speed and quality (WAND, benchmarks, success@3, MRR@10, tantivy comparison)

No search library is used in the core crates.
