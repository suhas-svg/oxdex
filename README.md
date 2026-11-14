# Oxdex

A full-text search engine written from scratch in Rust: tokenizer, inverted index, BM25 ranking. The goal is to index all of English Wikipedia and answer keyword queries in under 50 ms at p95.

**Status: M4 (query service and UI) working on Simple English Wikipedia.** An axum service serves AND / OR / `-exclude` queries with a title field, snippets and a search page. Full English (M3's 8 GB RAM exit check) is pending the 27 GB dump download.

## Try it

```bash
# M1: search a folder of text files
cargo run -p oxdex-mini -- data/mini fpga timing

# M2: ingest a multistream dump (download both files from dumps.wikimedia.org/simplewiki/latest/)
cargo run --release -p oxdex-ingest -- build \
  --dump data/dumps/simplewiki-latest-pages-articles-multistream.xml.bz2 --out data/out/simple
cargo run --release -p oxdex-ingest -- sample --store data/out/simple -n 20
cargo run --release -p oxdex-ingest -- audit  --store data/out/simple

# M3: build, check and search the on-disk index
cargo run --release -p oxdex-indexer -- build  --store data/out/simple --out data/index/simple
cargo run --release -p oxdex-indexer -- check  --index data/index/simple
cargo run --release -p oxdex-indexer -- search --index data/index/simple --store data/out/simple fpga

# M4: search service + web page at http://127.0.0.1:8080
cargo run --release -p oxdex-search-api -- --index data/index/simple --store data/out/simple
curl -G 127.0.0.1:8080/search --data-urlencode "q=fpga timing -xilinx" -d k=10 -d offset=0
cargo run --release -p oxdex-search-api -- bench --queries benches/queries-simple.txt --rounds 3

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
| Index build (1 GB segment budget) | 45 s: 20 s adding docs, 25 s segment write + merge on a laptop HDD |
| Index peak RAM (process working set) | 348–365 MB |
| Terms / postings / tokens | 864,546 / 24.2 M / 51.2 M |
| Index size on disk | 85.5 MB (postings 57 MB, dictionary 27 MB, doc lengths 1 MB) = 25.8% of clean text (target < 35%) |
| `check` | 0 errors, 0.7 s |
| Index open (mmap) | 0.7 ms |
| Title index (titles + 115,575 redirects) | 169,556 terms, 8 MB; index build with both fields 98 s, peak RAM 371 MB |
| Search, in-process, warm, 1,000 redirect-title queries | p50 3.7 ms, p95 13.1 ms, p99 23 ms, max 33 ms (~200 q/s on one thread) |
| Search, in-process, warm, 40 broad common-word queries | p50 4.7 ms, p95 10.2 ms |
| Search, in-process, cold page cache (first pass, HDD) | p50 49 ms, p95 389 ms |
| Search over HTTP, 1,040 queries replayed sequentially | server `took_ms` p50 5.8 ms, p95 19.9 ms; client round trip p50 6.7 ms, p95 26.7 ms; 0 errors |

Latency includes parsing, ranking, fetching 10 docs from the zstd store and building snippets. On Windows, point clients at `127.0.0.1`, not `localhost`: curl tries `::1` first and adds about 210 ms per request.

## Layout

| Path | What it is |
| --- | --- |
| `crates/text` | Shared tokenizer: Unicode word split, lowercase, Porter2 stem, drop tokens > 64 chars |
| `crates/wikitext` | Multistream index + stream reader, streaming `<page>` parser, wikitext cleaner |
| `crates/docstore` | `docs.bin` / `docs.idx` / `links.bin`: dense doc IDs, zstd blocks, mmap reader |
| `crates/index` | In-memory index (M1 reference), segment builder + k-way merge, delta/VByte postings in 128-doc blocks with skip pointers, mmap `DiskIndex`, checker |
| `crates/query` | Query parser (AND default, `OR`, `-exclude`, stopword drop), doc-at-a-time BM25 (k1 = 1.2, b = 0.75) over body + title fields (title × 2), leapfrog AND with OR fallback under 10 hits, top-k min-heap, snippets |
| `services/ingest` | `oxdex-ingest build / sample / audit / show` |
| `services/indexer` | `oxdex-indexer build / check / search`; builds the body index and `title/` (title + redirect titles) |
| `services/search-api` | axum service: `/search`, `/doc/{id}`, `/health`, `/stats`, the web page, and `bench` |
| `web` | Search page (HTML/CSS/JS, embedded in the binary) |
| `benches` | Query sets: 1,000 random redirect titles, 40 broad queries |
| `services/mini` | M1 CLI over a folder of `.txt` files (first line = title) |
| `data/mini` | 20 sample articles |

## Known cleaner gaps

Accepted under the one-week time box:

- Templates that render text, like `{{convert|4|C}}`, are dropped, so their values go missing.
- Section headings that only held links or templates ("References", "Other websites") remain as bare lines.
- Bare URLs in prose are kept (0.8% of articles).

## HTTP API

| Route | Returns |
| --- | --- |
| `GET /search?q=&k=10&offset=0` | `query`, `took_ms`, `total_hits`, `mode` (`and`, `or`, `and_fell_back_to_or`), `results[]` with `doc_id`, `title`, `url`, `score`, `snippet` (HTML-escaped, matches in `<mark>`) |
| `GET /doc/{id}` | Full stored text |
| `GET /health` | `status`, `index_version`, `num_docs` |
| `GET /stats` | Index sizes plus p50 / p95 / p99 of recent `took_ms` |

Empty queries, queries over 256 characters, `k` outside 1–100, `offset` over 10,000 and non-numeric parameters get a 400 with `{"error": "..."}`. Requests before the index is open get a 503. Query syntax: words are ANDed; a bare `OR` makes the query match any word; `-word` excludes documents whose body has it. If AND finds fewer than 10 documents the query reruns as OR. Stopwords are dropped unless the query is only stopwords.

## Known ranking gaps (M5)

- Paraphrases miss: "chip that can be rewired after manufacturing" doesn't surface *Field-programmable gate array*, which never says "rewired".
- With the OR fallback, a rare word and a common one score close together. "fpga timing" puts *Time Protocol* second.

## Milestones

- [x] M1: mini engine, golden ranking tests pass
- [x] M2: Wikipedia ingestion: zero panics, skip counts logged, 20 random articles read cleanly
- [x] M3: on-disk index (segments, k-way merge, VByte + skip pointers, mmap); the full English 8 GB check is pending
- [x] M4: query service and UI (axum, AND/OR/-exclude, snippets, title boost); p95 measured on Simple English, full English pending
- [ ] M5: speed and quality (WAND, benchmarks, success@3, MRR@10, tantivy comparison)

No search library is used in the core crates.
