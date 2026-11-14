# Oxdex

A full-text search engine written from scratch in Rust: tokenizer, inverted index, BM25 ranking. The goal is to index all of English Wikipedia and answer keyword queries in under 50 ms at p95.

**Status: M1 (mini engine) done.** Everything is in memory and runs over a folder of text files.

## Try it

```bash
cargo run -p oxdex-mini -- data/mini fpga timing
cargo run -p oxdex-mini -- data/mini            # interactive, one query per line
cargo test --workspace
```

## Layout

| Path | What it is |
| --- | --- |
| `crates/text` | Shared tokenizer: Unicode word split, lowercase, Porter2 stem, drop tokens > 64 chars |
| `crates/index` | Inverted index with dense `u32` doc IDs (in-memory for M1) |
| `crates/query` | BM25 scoring (k1 = 1.2, b = 0.75), top-k min-heap, golden ranking tests |
| `services/mini` | M1 CLI over a folder of `.txt` files (first line = title) |
| `data/mini` | 20 sample articles |

## Milestones

- [x] M1: mini engine, golden ranking tests pass
- [ ] M2: Wikipedia ingestion (multistream reader, wikitext cleaner, doc store)
- [ ] M3: on-disk index (segments, k-way merge, VByte + skip pointers, mmap)
- [ ] M4: query service and UI (axum, AND/OR/-exclude, snippets, title boost)
- [ ] M5: speed and quality (WAND, benchmarks, success@3, MRR@10, tantivy comparison)

No search library is used in the core crates.
