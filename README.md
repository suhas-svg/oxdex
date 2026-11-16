# Oxdex

A full-text search engine written from scratch in Rust: tokenizer, inverted index, BM25 ranking. The goal is to index all of English Wikipedia and answer keyword queries in under 50 ms at p95.

**Status: M5 (speed and quality) in progress.** Relevance evaluation, ranking tuning, exact-title lookup, WAND, criterion benchmarks and the tantivy comparison are done on Simple English Wikipedia. MS MARCO MRR@10 and the oha load test are done too. Still to come: every number on full English (the 27 GB dump is still downloading).

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

# M5: relevance, micro-benchmarks, tantivy comparison
cargo run --release -p oxdex-search-api -- eval --verbose            # success@k and MRR@10 on benches/eval-simple.tsv
cargo run --release -p oxdex-search-api -- eval --body-b 0.75 --title-b 0.75   # try other ranking settings
cargo bench -p oxdex-query --bench hot_paths
cargo run --release -p oxdex-tantivy-bench -- build --store data/out/simple --out data/tantivy/simple
cargo run --release -p oxdex-tantivy-bench -- bench --index data/tantivy/simple --queries benches/queries-simple.txt

# M5: MS MARCO passage ranking (collection.tsv, queries.dev.tsv, qrels.dev.small.tsv from msmarco.z22.web.core.windows.net/msmarcoranking/)
cargo run --release -p oxdex-ingest  -- tsv --input data/dumps/msmarco/collection.tsv --out data/out/msmarco
cargo run --release -p oxdex-indexer -- build --store data/out/msmarco --out data/index/msmarco
cargo run --release -p oxdex-search-api -- --index data/index/msmarco --store data/out/msmarco msmarco --or --k1 0.9 --body-b 0.4

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
| Title index (titles + 115,575 redirects) | 169,556 terms, 8 MB; exact-title keys 381,925 entries, 4.6 MB |
| Index build, both fields + title keys (format v2) | 37 s, peak RAM 373 MB; dictionary 33 MB (32-byte entries now carry `max_tf` and `min_doc_len` for WAND) |
| Search, in-process, warm, 1,000 redirect-title queries | p50 2.6 ms, p95 6.2 ms, p99 10.2 ms (340 q/s on one thread); ranking alone p50 0.09 ms, p95 0.9 ms |
| Search, in-process, warm, 40 broad common-word queries | p50 4.7 ms, p95 10.2 ms (measured in M4, before the snippet speed-up) |
| Startup (launch → `/health` OK) | 125 ms warm, 658 ms cold (target < 2 s) |
| oha, 1 client, 1,000 queries replayed, warm | p50 3.1 ms, p95 7.5 ms, p99 11.6 ms (277 req/s) |
| oha, 12 clients (one per core) | 1,379 req/s, p50 7.5 ms, p95 18.7 ms |
| oha, 50 clients, 30 s | **1,179 req/s** (target > 500), p50 41 ms, p95 59 ms, p99 75 ms, 0 errors |
| Search, in-process, cold page cache (first pass, HDD) | p50 49 ms, p95 389 ms |
| Search over HTTP, 1,040 queries replayed sequentially | server `took_ms` p50 5.8 ms, p95 19.9 ms; client round trip p50 6.7 ms, p95 26.7 ms; 0 errors |

Latency includes parsing, ranking, fetching 10 docs from the zstd store and building snippets. Ranking is about 5% of it: fetching the docs takes about 1.6 ms, and snippets took most of the rest until M5. They now check a word's first letter before stemming it, which took p95 from 13.2 ms to 6.2 ms. On Windows, point clients at `127.0.0.1`, not `localhost`: curl tries `::1` first and adds about 210 ms per request.

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
| `benches` | Query sets: 1,000 random redirect titles, 40 broad queries; `eval-simple.tsv` relevance set |
| `services/tantivy-bench` | tantivy comparison (not a dependency of anything else) |
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

## Relevance (M5)

`benches/eval-simple.tsv` holds 50 queries, each with the article that should come back: 25 named lookups ("albert einstein") and 25 descriptions ("organ that pumps blood" → *Heart*). Every expected title is checked against the store first, so a typo can't count as a miss.

| Ranking | success@1 | success@3 | success@10 | MRR@10 |
| --- | --- | --- | --- | --- |
| M4: textbook BM25 (k1 1.2, b 0.75 both fields), title × 2 | 30% | 40% | 62% | 0.384 |
| No length normalization on the title field (b 0) | 38% | 56% | 70% | 0.484 |
| + body b 0.3 | 42% | 62% | 76% | 0.525 |
| + exact-title lookup (current default) | 50% | **64%** | 76% | 0.576 |
| tantivy 0.22, same fields, title × 2, same AND → OR fallback | 40% | 46% | 66% | 0.461 |

Why these changes:

- **Title b = 0.** The title field also holds every redirect, so popular articles have long title fields. Normalizing for length punished exactly the articles people search for.
- **Body b = 0.3.** The central article on a topic is usually long. With b = 0.75, *Albert Einstein* lost to *Albert Einstein Square*.
- **Exact-title lookup.** `title_keys.bin` maps a hash of each analyzed title and redirect to its doc. A query that *is* an article's name or redirect gets that article first: "gravity" now returns *Gravity* rather than *Gravity (2013 movie)*.

The target is 80% success@3, and lexical search falls short of it. Named lookups get 24 of 25 into the top 3. Descriptions get 8 of 25. "Largest planet in the solar system" puts *Solar System* first because its title matches. *Heart* never uses the words "pumps blood". Closing that gap takes the semantic or link-based signals listed as M6 stretch goals. Simple English is a hard corpus for this: its articles are short. The same set will be rerun on full English.

## Speed work (M5)

- **WAND** for OR queries, as in the spec. Each term's dictionary entry stores its largest tf and its shortest document, which bounds its BM25 contribution for any k1 and b. Documents whose summed bounds can't reach the current 10th-place score are skipped. A proptest checks that WAND returns bit-identical top-k results to scoring everything, on random skewed corpora in memory and on disk. When documents are skipped, `total_hits` becomes a lower bound and the API sets `"total_exact": false`; the page then says "at least N". On Simple English it changes nothing measurable, because ranking takes under 1 ms. It's there for full English, where lists are 25× longer.
- **Exact-title docs** get a +1000 bonus, so WAND scores them in a prepass instead of letting the bonus loosen every bound.

Criterion micro-benchmarks (`cargo bench -p oxdex-query`), one thread:

| Benchmark | Result |
| --- | --- |
| Tokenize (Unicode words, lowercase, Porter2) | 14.5 MiB/s |
| VByte decode | 162 M integers/s |
| Block-cursor walk (doc + tf) | 209 M postings/s |
| Intersect 1k × 1M postings (leapfrog with skip pointers) | 0.42 ms |
| Intersect 50k × 1M / 500k × 1M | 4.5 ms / 12.1 ms |
| BM25 term score | 448 M/s |
| Snippet over a 40 KB article | 0.47 ms |

## MS MARCO passage ranking (M5)

8,841,823 passages (3.1 GB of text), scored on the 6,980 dev-small queries with the official relevance labels. Each query has one or a few known answer passages, and MRR@10 averages 1 / (rank of the first answer) over the top 10.

| Setup | MRR@10 | recall@10 | Ranking p50 / p95 |
| --- | --- | --- | --- |
| Oxdex, Anserini's BM25 settings (k1 0.9, b 0.4), all-OR | **0.188** | 40.0% | 2.95 / 18.2 ms |
| Oxdex, Anserini's tuned settings (k1 0.82, b 0.68), all-OR | 0.191 | 40.9% | 2.93 / 18.3 ms |
| Oxdex defaults (tuned for Wikipedia: k1 1.2, b 0.3), all-OR | 0.178 | 38.3% | 3.5 / 21.9 ms |
| Oxdex defaults, AND with OR fallback | 0.180 | 39.0% | 2.2 / 20.8 ms |
| Anserini (Lucene) BM25, published: default / tuned | 0.184 / 0.187 | | |
| Same as row 1 with WAND off | 0.188 | 40.0% | 17.95 / 72.1 ms |

- With the same parameters, Oxdex matches Lucene's published BM25. That is the check the spec asks for: this BM25 implementation is correct at 8.8M-document scale. The small edge likely comes from tokenizer and stopword-list differences.
- The Wikipedia-tuned defaults score lower here. Passages are short and similar in length, so the low b chosen for Wikipedia's long central articles doesn't fit. Ranking parameters are corpus-specific. The service keeps the Wikipedia defaults, since Wikipedia is what it serves.
- **WAND pays off at this scale**: ranking p50 drops 6× and p95 4× with identical results. Simple English was too small for WAND to show any gain.

Build at this scale, as a rehearsal for full English: loading the store took 179 s (3.1 GB → 1.16 GB zstd). The index build took 210 s with a 1 GB buffer and **peak RAM 2.0 GB**: 2.67 M terms, 346 M postings, 955 MB on disk = 31% of the text. `check` reports 0 errors in 11 s.

## Load test (M5)

`oha --urls-from-file benches/urls-simple.txt` replays the 1,000 queries over HTTP, so each request is a different search, not one cached URL. At 50 clients, latency is mostly time spent waiting in line. By Little's law, 50 requests in flight at 1,200/s means about 40 ms each, however fast one search is. The useful question is how that wait is shared.

In M4, every request went straight to the blocking thread pool, so 50 searches time-sliced 12 cores. Each search slowed down (server-side p95 69 ms) and the tail was long. The service now runs at most one search per core and queues the rest fairly (`--search-slots`, default = cores):

| Search slots, 50 clients | Throughput | p50 | p95 | p99 | Slowest |
| --- | --- | --- | --- | --- | --- |
| 12 (default) | 1,179 /s | 41 ms | 59 ms | 75 ms | 163 ms |
| 24 | 1,381 /s | 31 ms | 70 ms | 115 ms | 427 ms |
| unlimited (M4) | 1,419 /s | 23 ms | 108 ms | 193 ms | 520 ms |

The default gives up about 17% of peak throughput for a tail three times tighter. Set `--search-slots` higher if throughput matters more.

oha's Windows GNU build needs jemalloc, which doesn't compile with this toolchain. I built it from the cached oha 1.17.0 source with the jemalloc allocator compiled out, so it uses the system allocator instead. That changes nothing about what oha measures.

## Oxdex vs tantivy (Simple English)

Same 286,043 articles, same queries. tantivy is set up to match Oxdex: `en_stem` tokenizer, freqs without positions, title and body fields with title × 2, AND with an OR fallback under 10 hits, top 10. The timings cover ranking only. Neither engine fetches text or builds snippets in this test.

| | Oxdex | tantivy 0.22 |
| --- | --- | --- |
| Build time | 37 s | 23.5 s |
| Index size | 104 MB (33% of text) | 62 MB (19%) |
| Ranking, 1,000 title queries, p50 / p95 | 0.09 / 0.88 ms | 0.36 / 3.1 ms |
| Ranking, 40 broad queries, p50 / p95 | 0.74 / 2.3 ms | 1.6 / 5.3 ms |
| success@3 / MRR@10 | 64% / 0.576 | 46% / 0.461 |

Why the index is bigger: tantivy bit-packs blocks of 128 doc IDs and tfs, while Oxdex uses VByte (a byte per small gap at best). tantivy's term dictionary is an FST that shares prefixes, while Oxdex stores 32-byte fixed entries plus raw strings: 33 MB for 865k terms. Oxdex also keeps a separate title index and title keys.

Why queries are faster here: Oxdex's query path is just a dictionary lookup and a few cursors. tantivy runs a general query parser and builds a weight and scorer tree per field and clause. At this corpus size that fixed cost dominates, so expect the gap to narrow on full English, where scanning postings costs more.

Relevance differs because of Oxdex's ranking changes above, not because of the engine. tantivy uses textbook BM25 with b = 0.75.

## Milestones

- [x] M1: mini engine, golden ranking tests pass
- [x] M2: Wikipedia ingestion: zero panics, skip counts logged, 20 random articles read cleanly
- [x] M3: on-disk index (segments, k-way merge, VByte + skip pointers, mmap); the full English 8 GB check is pending
- [x] M4: query service and UI (axum, AND/OR/-exclude, snippets, title boost); p95 measured on Simple English, full English pending
- [ ] M5: speed and quality: eval set, tuning, exact-title lookup, WAND, criterion, tantivy, oha load test, MS MARCO MRR@10 done; full English numbers to go

No search library is used in the core crates.
