//! In-memory inverted index (milestone M1).
//!
//! Doc IDs are dense: 0, 1, 2, … in insertion order. Posting lists are sorted by
//! doc ID because documents are only ever appended. M3 replaces this with
//! on-disk segments, but keeps the same read API shape.

use std::collections::HashMap;

use oxdex_text::Analyzer;

pub type DocId = u32;

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
        let analyzer = Analyzer::global();

        let mut tfs: HashMap<String, u32> = HashMap::new();
        let mut len: u32 = 0;
        for token in analyzer.tokens(title).chain(analyzer.tokens(text)) {
            *tfs.entry(token).or_default() += 1;
            len += 1;
        }
        for (term, tf) in tfs {
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

    /// Average document length in tokens (0.0 for an empty index).
    pub fn avgdl(&self) -> f64 {
        if self.docs.is_empty() {
            0.0
        } else {
            self.total_len as f64 / self.docs.len() as f64
        }
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
}
