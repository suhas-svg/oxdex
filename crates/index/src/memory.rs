//! In-memory inverted index (milestone M1).
//!
//! Doc IDs are dense: 0, 1, 2, … in insertion order. Posting lists are sorted
//! by doc ID because documents are only ever appended. Used for small corpora
//! and as the reference the on-disk index is tested against.

use std::collections::HashMap;

use crate::{DocId, NO_MORE_DOCS, PostingCursor, SearchIndex, count_terms};

/// One entry of a posting list: a document and how often the term appears in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    pub doc: DocId,
    pub tf: u32,
}

#[derive(Debug, Clone)]
pub struct StoredDoc {
    pub title: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct InMemoryIndex {
    postings: HashMap<String, Vec<Posting>>,
    doc_lens: Vec<u32>,
    /// u64 so corpus-wide totals cannot overflow on full Wikipedia.
    total_len: u64,
    docs: Vec<StoredDoc>,
}

impl InMemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a document and returns its doc ID.
    ///
    /// The title is indexed together with the body for now; M4 splits it into
    /// its own field for the title boost.
    pub fn add(&mut self, title: &str, text: &str) -> DocId {
        let doc = DocId::try_from(self.docs.len()).expect("more than u32::MAX documents");
        let (terms, len) = count_terms(title, text);
        for (term, tf) in terms {
            self.postings.entry(term).or_default().push(Posting { doc, tf });
        }
        self.doc_lens.push(len);
        self.total_len += u64::from(len);
        self.docs.push(StoredDoc {
            title: title.to_owned(),
            text: text.to_owned(),
        });
        doc
    }

    pub fn num_docs(&self) -> u32 {
        self.docs.len() as u32
    }

    pub fn num_terms(&self) -> usize {
        self.postings.len()
    }

    /// All indexed terms, in no particular order.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.postings.keys().map(String::as_str)
    }

    /// Average document length in tokens (0.0 for an empty index).
    pub fn avgdl(&self) -> f64 {
        if self.docs.is_empty() {
            0.0
        } else {
            self.total_len as f64 / self.docs.len() as f64
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_len
    }

    /// The posting list for an already-analyzed term, sorted by doc ID.
    pub fn postings(&self, term: &str) -> &[Posting] {
        self.postings.get(term).map_or(&[], Vec::as_slice)
    }

    /// Number of documents containing `term`.
    pub fn doc_freq(&self, term: &str) -> u32 {
        self.postings(term).len() as u32
    }

    pub fn doc_len(&self, doc: DocId) -> u32 {
        self.doc_lens[doc as usize]
    }

    pub fn doc(&self, doc: DocId) -> &StoredDoc {
        &self.docs[doc as usize]
    }
}

/// Cursor over an in-memory posting list.
pub struct SliceCursor<'a> {
    postings: &'a [Posting],
    i: usize,
    max_tf: u32,
    min_doc_len: u32,
}

impl<'a> SliceCursor<'a> {
    /// A cursor with always-safe score bounds; see [`Self::with_bounds`].
    pub fn new(postings: &'a [Posting]) -> Self {
        Self { postings, i: 0, max_tf: u32::MAX, min_doc_len: 0 }
    }

    pub fn with_bounds(mut self, max_tf: u32, min_doc_len: u32) -> Self {
        self.max_tf = max_tf;
        self.min_doc_len = min_doc_len;
        self
    }
}

impl PostingCursor for SliceCursor<'_> {
    fn doc(&self) -> DocId {
        self.postings.get(self.i).map_or(NO_MORE_DOCS, |p| p.doc)
    }

    fn tf(&self) -> u32 {
        self.postings.get(self.i).map_or(0, |p| p.tf)
    }

    fn next(&mut self) {
        self.i = (self.i + 1).min(self.postings.len());
    }

    fn advance(&mut self, target: DocId) {
        self.i += self.postings[self.i..].partition_point(|p| p.doc < target);
    }

    fn df(&self) -> u32 {
        self.postings.len() as u32
    }

    fn max_tf(&self) -> u32 {
        self.max_tf
    }

    fn min_doc_len(&self) -> u32 {
        self.min_doc_len
    }
}

impl SearchIndex for InMemoryIndex {
    type Cursor<'a> = SliceCursor<'a>;

    fn num_docs(&self) -> u32 {
        InMemoryIndex::num_docs(self)
    }

    fn avgdl(&self) -> f64 {
        InMemoryIndex::avgdl(self)
    }

    fn doc_len(&self, doc: DocId) -> u32 {
        self.doc_lens.get(doc as usize).copied().unwrap_or(0)
    }

    fn cursor(&self, term: &str) -> Option<SliceCursor<'_>> {
        // The reference index computes exact bounds by scanning the list.
        self.postings.get(term).map(|p| {
            let max_tf = p.iter().map(|x| x.tf).max().unwrap_or(0);
            let min_dl = p.iter().map(|x| self.doc_lens[x.doc as usize]).min().unwrap_or(0);
            SliceCursor::new(p).with_bounds(max_tf, min_dl)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_dense_and_postings_sorted() {
        let mut idx = InMemoryIndex::new();
        assert_eq!(idx.add("A", "rust rust search"), 0);
        assert_eq!(idx.add("B", "search engine"), 1);
        assert_eq!(idx.add("C", "rust engine"), 2);

        assert_eq!(idx.num_docs(), 3);
        assert_eq!(
            idx.postings("rust"),
            [Posting { doc: 0, tf: 2 }, Posting { doc: 2, tf: 1 }]
        );
        assert_eq!(idx.doc_freq("search"), 2);
        assert_eq!(idx.postings("missing"), []);
    }

    #[test]
    fn lengths_include_title() {
        let mut idx = InMemoryIndex::new();
        idx.add("Two words", "three more words");
        idx.add("One", "");
        assert_eq!(idx.doc_len(0), 5);
        assert_eq!(idx.doc_len(1), 1);
        assert_eq!(idx.avgdl(), 3.0);
        assert_eq!(idx.postings("word"), [Posting { doc: 0, tf: 2 }]);
    }

    #[test]
    fn empty_index() {
        let idx = InMemoryIndex::new();
        assert_eq!(idx.num_docs(), 0);
        assert_eq!(idx.avgdl(), 0.0);
    }

    #[test]
    fn slice_cursor_advance() {
        let mut idx = InMemoryIndex::new();
        for i in 0..10 {
            idx.add("t", if i % 3 == 0 { "hit" } else { "miss" });
        }
        let mut c = SearchIndex::cursor(&idx, "hit").unwrap();
        assert_eq!(c.df(), 4);
        assert_eq!(c.doc(), 0);
        c.advance(4);
        assert_eq!(c.doc(), 6);
        c.advance(6);
        assert_eq!(c.doc(), 6);
        c.next();
        assert_eq!(c.doc(), 9);
        c.advance(100);
        assert_eq!(c.doc(), NO_MORE_DOCS);
        c.next();
        assert_eq!(c.doc(), NO_MORE_DOCS);
    }
}
