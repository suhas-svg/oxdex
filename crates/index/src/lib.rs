//! The Oxdex inverted index.
//!
//! - [`InMemoryIndex`]: the M1 index, kept for small corpora and as a reference
//!   implementation in tests.
//! - [`IndexBuilder`]: builds the on-disk index in memory-bounded segments, then
//!   k-way merges them.
//! - [`DiskIndex`]: memory-mapped reader over the merged files.
//! - [`check`]: validates every posting list of a built index.
//!
//! Both indexes implement [`SearchIndex`], so the query engine runs unchanged on
//! either one.
//!
//! On-disk files (all integers little-endian, each file starts with 8 magic
//! bytes and a u32 format version):
//!
//! | File | Layout |
//! | --- | --- |
//! | `terms.dict` | 32-byte header (`OXDXDICT`, version, reserved, u64 num_terms, u64 heap_offset); `num_terms` × 32-byte entries sorted by term bytes (`u64 str_off, u64 post_off, u32 df, u32 str_len, u32 max_tf, u32 min_doc_len`); then the term string heap |
//! | `postings.bin` | 16-byte header (`OXDXPOST`); per term, see [`postings`] |
//! | `doclens.bin` | 16-byte header (`OXDXDLEN`); one u32 token count per doc ID |
//! | `title_keys.bin` | Optional exact-title lookup, see [`titlekeys`] |
//! | `meta.json` | [`IndexMeta`]; written last, so its presence marks a finished build |

mod builder;
mod check;
mod disk;
mod memory;
pub mod postings;
pub mod titlekeys;
pub mod vbyte;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use oxdex_text::Analyzer;
use serde::{Deserialize, Serialize};

pub use builder::{BuildStats, IndexBuilder};
pub use check::{CheckReport, check};
pub use disk::{DiskIndex, TermEntry};
pub use memory::{InMemoryIndex, Posting, SliceCursor, StoredDoc};
pub use postings::BlockCursor;

pub type DocId = u32;

/// Returned by [`PostingCursor::doc`] once a cursor is exhausted.
pub const NO_MORE_DOCS: DocId = u32::MAX;

/// On-disk format version. Bump on any layout change; readers refuse others.
pub const FORMAT_VERSION: u32 = 2;

/// A forward-only walk over one term's posting list, in doc ID order.
pub trait PostingCursor {
    /// Current doc ID, or [`NO_MORE_DOCS`] when exhausted.
    fn doc(&self) -> DocId;
    /// Term frequency in the current doc (0 when exhausted).
    fn tf(&self) -> u32;
    /// Moves to the next posting.
    fn next(&mut self);
    /// Moves to the first posting with doc ID ≥ `target` (no-op if already there).
    fn advance(&mut self, target: DocId);
    /// Document frequency: the list's total length.
    fn df(&self) -> u32;
    /// Largest tf anywhere in the list. With [`Self::min_doc_len`] it bounds
    /// the BM25 score this term can give any document, for any k1 and b.
    fn max_tf(&self) -> u32;
    /// Length of the shortest document in the list.
    fn min_doc_len(&self) -> u32;
}

/// What the query engine needs from an index.
pub trait SearchIndex {
    type Cursor<'a>: PostingCursor
    where
        Self: 'a;

    fn num_docs(&self) -> u32;
    fn avgdl(&self) -> f64;
    fn doc_len(&self, doc: DocId) -> u32;
    /// Cursor over an already-analyzed term, or `None` if it is not indexed.
    fn cursor(&self, term: &str) -> Option<Self::Cursor<'_>>;
}

/// Analyzes a document into distinct terms with their counts, plus its length
/// in tokens. Both indexes use this, so they always agree on tokenization.
pub fn count_terms(title: &str, text: &str) -> (Vec<(String, u32)>, u32) {
    let analyzer = Analyzer::global();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut len: u32 = 0;
    for token in analyzer.tokens(title).chain(analyzer.tokens(text)) {
        *counts.entry(token).or_default() += 1;
        len = len.saturating_add(1);
    }
    (counts.into_iter().collect(), len)
}

/// Header facts the query service checks at startup (`meta.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub format_version: u32,
    pub num_docs: u32,
    pub num_terms: u64,
    /// u64: the corpus-wide total overflows u32 on full Wikipedia.
    pub total_tokens: u64,
    pub avgdl: f64,
    pub num_segments: usize,
    pub postings_bytes: u64,
    pub dict_bytes: u64,
    pub doclens_bytes: u64,
    pub mem_budget_bytes: u64,
    pub build_secs: f64,
    pub built_unix_secs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not an Oxdex index file (bad magic bytes)")]
    BadMagic { path: PathBuf },
    #[error("{path} has format version {found}, this build reads version {FORMAT_VERSION}")]
    BadVersion { path: PathBuf, found: u32 },
    #[error("{0}: no meta.json, so the build did not finish")]
    Incomplete(PathBuf),
    #[error("bad meta.json: {0}")]
    Meta(#[from] serde_json::Error),
    #[error("corrupt index: {0}")]
    Corrupt(String),
    #[error("too many documents for u32 doc IDs")]
    TooManyDocs,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}
