//! The one tokenizer Oxdex uses everywhere.
//!
//! The indexer and the query service both call [`Analyzer::tokens`], so a word is
//! always split, lowercased and stemmed the same way on both sides. If you change
//! anything here, every index built before the change must be rebuilt.

use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};
use unicode_segmentation::UnicodeSegmentation;

/// Tokens longer than this many characters are dropped (base64 blobs, long URLs).
pub const MAX_TOKEN_CHARS: usize = 64;

/// Splits text on Unicode word boundaries, lowercases, and stems with Porter2.
pub struct Analyzer {
    stemmer: Stemmer,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            stemmer: Stemmer::create(Algorithm::English),
        }
    }

    /// Returns the shared, lazily built analyzer.
    pub fn global() -> &'static Analyzer {
        static ANALYZER: OnceLock<Analyzer> = OnceLock::new();
        ANALYZER.get_or_init(Analyzer::new)
    }

    /// Yields the index terms for `text`, in order, duplicates included.
    pub fn tokens<'a>(&'a self, text: &'a str) -> impl Iterator<Item = String> + 'a {
        text.unicode_words()
            .filter(|w| w.chars().count() <= MAX_TOKEN_CHARS)
            .map(move |w| self.term(w))
    }

    /// Like [`tokens`](Self::tokens), but also yields each word's byte offset
    /// and original spelling, for highlighting matches in snippets.
    pub fn tokens_with_offsets<'a>(
        &'a self,
        text: &'a str,
    ) -> impl Iterator<Item = (usize, &'a str, String)> + 'a {
        text.unicode_word_indices()
            .filter(|(_, w)| w.chars().count() <= MAX_TOKEN_CHARS)
            .map(move |(i, w)| (i, w, self.term(w)))
    }

    /// The words [`tokens`](Self::tokens) would analyze, with byte offsets,
    /// before lowercasing and stemming. Lets callers skip words cheaply.
    pub fn words_with_offsets<'a>(&self, text: &'a str) -> impl Iterator<Item = (usize, &'a str)> + 'a {
        text.unicode_word_indices().filter(|(_, w)| w.chars().count() <= MAX_TOKEN_CHARS)
    }

    /// The index term for one word: lowercased, then stemmed.
    pub fn term(&self, word: &str) -> String {
        let lower = word.to_lowercase();
        self.stemmer.stem(&lower).into_owned()
    }
}

/// Convenience wrapper around [`Analyzer::global`].
pub fn tokenize(text: &str) -> Vec<String> {
    Analyzer::global().tokens(text).collect()
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn lowercases_and_stems() {
        assert_eq!(tokenize("Running runs RUN"), ["run", "run", "run"]);
        assert_eq!(tokenize("Indexes indexing indexed"), ["index", "index", "index"]);
    }

    #[test]
    fn splits_hyphens_and_drops_punctuation() {
        assert_eq!(
            tokenize("Field-programmable gate array!"),
            ["field", "programm", "gate", "array"]
        );
    }

    #[test]
    fn keeps_decimal_numbers_together() {
        assert_eq!(tokenize("IEEE 802.3 at 2.5 Gbit/s"), ["ieee", "802.3", "at", "2.5", "gbit", "s"]);
    }

    #[test]
    fn cpp_loses_its_plus_signs() {
        // Known limitation: "C++" and "C" are the same term. Fine for v1.
        assert_eq!(tokenize("C++ and C"), ["c", "and", "c"]);
    }

    #[test]
    fn keeps_accents() {
        assert_eq!(tokenize("Café Zürich"), ["café", "zürich"]);
    }

    #[test]
    fn keeps_stopwords() {
        assert_eq!(tokenize("the art of the deal"), ["the", "art", "of", "the", "deal"]);
    }

    #[test]
    fn drops_overlong_tokens() {
        let junk = "a".repeat(65);
        let ok = "b".repeat(64);
        let text = format!("x {junk} {ok} y");
        assert_eq!(tokenize(&text), vec!["x".to_string(), ok, "y".to_string()]);
    }

    #[test]
    fn offsets_point_at_original_words() {
        let text = "Café owners RUN cafés.";
        let got: Vec<(usize, &str, String)> = super::Analyzer::global().tokens_with_offsets(text).collect();
        assert_eq!(got.len(), 4);
        for (i, w, _) in &got {
            assert_eq!(&text[*i..*i + w.len()], *w);
        }
        assert_eq!(got[2], (13, "RUN", "run".to_string()));
        let plain: Vec<String> = got.into_iter().map(|t| t.2).collect();
        assert_eq!(plain, tokenize(text), "same terms as tokens()");
    }

    #[test]
    fn empty_and_whitespace() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("  \n\t ").is_empty());
        assert!(tokenize("!!! ... ---").is_empty());
    }
}
