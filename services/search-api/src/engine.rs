//! Everything one search needs, opened once and shared across requests.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use oxdex_docstore::DocStore;
use oxdex_index::{DiskIndex, FORMAT_VERSION};
use oxdex_query::{Fields, Params, parse, search_fields, snippet};
use serde::Serialize;

pub const SNIPPET_CHARS: usize = 160;

pub struct Engine {
    pub body: DiskIndex,
    pub title: Option<DiskIndex>,
    pub store: DocStore,
    pub params: Params,
    pub index_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub took_ms: f64,
    pub total_hits: usize,
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
        let store = DocStore::open(store).with_context(|| format!("opening store {}", store.display()))?;
        if store.num_docs() != body.meta().num_docs {
            bail!("index has {} docs but the store has {}", body.meta().num_docs, store.num_docs());
        }
        let size = |i: &DiskIndex| i.meta().postings_bytes + i.meta().dict_bytes + i.meta().doclens_bytes;
        let index_bytes = size(&body) + title.as_ref().map_or(0, size);
        Ok(Self { body, title, store, params: Params::default(), index_bytes })
    }

    pub fn index_version(&self) -> u32 {
        FORMAT_VERSION
    }

    pub fn search(&self, query: &str, k: usize, offset: usize) -> Result<SearchResponse> {
        let started = Instant::now();
        let parsed = parse(query);
        let fields = Fields { body: &self.body, title: self.title.as_ref() };
        let r = search_fields(&fields, &parsed, k, offset, &self.params);

        let mut results = Vec::with_capacity(r.hits.len());
        for hit in &r.hits {
            let d = self.store.get(hit.doc)?;
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
            total_hits: r.total_hits,
            mode: r.mode.as_str(),
            k,
            offset,
            results,
        })
    }
}
