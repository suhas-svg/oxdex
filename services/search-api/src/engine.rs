//! Everything one search needs, opened once and shared across requests.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use oxdex_docstore::DocStore;
use oxdex_index::titlekeys::TitleKeys;
use oxdex_index::{DiskIndex, FORMAT_VERSION};
use oxdex_query::{Fields, Params, parse, search_fields, snippet};
use serde::Serialize;

pub const SNIPPET_CHARS: usize = 160;

pub struct Engine {
    pub body: DiskIndex,
    pub title: Option<DiskIndex>,
    pub store: DocStore,
    pub title_keys: Option<TitleKeys>,
    pub params: Params,
    pub index_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub took_ms: f64,
    /// Part of `took_ms` spent matching and ranking; the rest is fetching
    /// the page's documents and building snippets.
    pub rank_ms: f64,
    /// Part of `took_ms` spent reading documents from the store.
    pub fetch_ms: f64,
    pub total_hits: usize,
    /// False when the engine skipped documents it could prove would not make
    /// the page (WAND); `total_hits` is then a lower bound.
    pub total_exact: bool,
    /// "and", "or", or "and_fell_back_to_or".
    pub mode: &'static str,
    pub k: usize,
    pub offset: usize,
    pub results: Vec<ResultItem>,
}

#[derive(Debug, Serialize)]
pub struct ResultItem {
    pub doc_id: u32,
    pub title: String,
    pub url: String,
    pub score: f64,
    /// HTML-escaped text with matches wrapped in `<mark>`.
    pub snippet: String,
}

impl Engine {
    /// Opens `<index>` (body field), `<index>/title` if present, and the store.
    pub fn open(index: &Path, store: &Path) -> Result<Self> {
        let body = DiskIndex::open(index).with_context(|| format!("opening index {}", index.display()))?;
        let title_dir = index.join("title");
        let title = if title_dir.join("meta.json").exists() {
            let t = DiskIndex::open(&title_dir).with_context(|| format!("opening {}", title_dir.display()))?;
            if t.meta().num_docs != body.meta().num_docs {
                bail!("title index has {} docs, body has {}", t.meta().num_docs, body.meta().num_docs);
            }
            Some(t)
        } else {
            None
        };
        let title_keys = TitleKeys::open(index)?;
        let store = DocStore::open(store).with_context(|| format!("opening store {}", store.display()))?;
        if store.num_docs() != body.meta().num_docs {
            bail!("index has {} docs but the store has {}", body.meta().num_docs, store.num_docs());
        }
        let size = |i: &DiskIndex| i.meta().postings_bytes + i.meta().dict_bytes + i.meta().doclens_bytes;
        let keys_bytes = std::fs::metadata(index.join(oxdex_index::titlekeys::TITLE_KEYS_FILE)).map_or(0, |m| m.len());
        let index_bytes = size(&body) + title.as_ref().map_or(0, size) + keys_bytes;
        Ok(Self { body, title, store, title_keys, params: Params::default(), index_bytes })
    }

    pub fn index_version(&self) -> u32 {
        FORMAT_VERSION
    }

    /// Ranked doc IDs only (no fetch, no snippets), for evaluation runs.
    /// `force_or` scores every query as OR, the usual setup for MS MARCO.
    pub fn rank(&self, query: &str, k: usize, force_or: bool) -> Vec<u32> {
        let mut parsed = parse(query);
        if force_or {
            parsed.mode = oxdex_query::Mode::Any;
        }
        if let Some(tk) = &self.title_keys
            && parsed.is_plain()
        {
            parsed.exact_title_docs = tk.lookup(query);
        }
        let fields = Fields { body: &self.body, title: self.title.as_ref() };
        search_fields(&fields, &parsed, k, 0, &self.params).hits.iter().map(|h| h.doc).collect()
    }

    pub fn search(&self, query: &str, k: usize, offset: usize) -> Result<SearchResponse> {
        let started = Instant::now();
        let mut parsed = parse(query);
        if let Some(tk) = &self.title_keys
            && parsed.is_plain()
        {
            parsed.exact_title_docs = tk.lookup(query);
        }
        let fields = Fields { body: &self.body, title: self.title.as_ref() };
        let r = search_fields(&fields, &parsed, k, offset, &self.params);
        let rank_ms = started.elapsed().as_secs_f64() * 1e3;

        let mut results = Vec::with_capacity(r.hits.len());
        let mut fetch = std::time::Duration::ZERO;
        for hit in &r.hits {
            let t = Instant::now();
            let d = self.store.get(hit.doc)?;
            fetch += t.elapsed();
            results.push(ResultItem {
                doc_id: d.id,
                snippet: snippet(&d.text, &parsed.terms, SNIPPET_CHARS),
                title: d.title,
                url: d.url,
                score: (hit.score * 1000.0).round() / 1000.0,
            });
        }
        Ok(SearchResponse {
            query: query.to_owned(),
            took_ms: (started.elapsed().as_secs_f64() * 1e6).round() / 1e3,
            rank_ms: (rank_ms * 1e3).round() / 1e3,
            fetch_ms: (fetch.as_secs_f64() * 1e6).round() / 1e3,
            total_hits: r.total_hits,
            total_exact: r.total_exact,
            mode: r.mode.as_str(),
            k,
            offset,
            results,
        })
    }
}
