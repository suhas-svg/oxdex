//! Query parsing: `fpga timing -xilinx` means "fpga AND timing, NOT xilinx";
//! a standalone uppercase `OR` switches the whole query to any-term matching.
//!
//! Stopwords stay in the index (so phrase search stays possible later), but the
//! query side drops them whenever the query has at least one other term:
//! requiring "that" or "can" would only shrink AND results and cost time.

use std::collections::HashSet;
use std::sync::OnceLock;

use oxdex_text::Analyzer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every term must match (the default).
    All,
    /// Any term may match (`OR` keyword).
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Analyzed terms to match and score, distinct, in query order.
    pub terms: Vec<String>,
    /// Analyzed terms whose documents are removed from the results.
    pub excluded: Vec<String>,
    pub mode: Mode,
    /// Stopwords that were dropped from `terms`.
    pub dropped_stopwords: Vec<String>,
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "he", "her", "his", "how", "i", "if", "in",
    "into", "is", "it", "its", "no", "not", "of", "on", "or", "she", "should", "so", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "to",
    "was", "we", "were", "what", "when", "where", "which", "who", "will", "with", "would", "you",
    "after", "about",
];

/// Stopwords in their analyzed (stemmed) form, e.g. "does" → "doe".
fn stopwords() -> &'static HashSet<String> {
    static SET: OnceLock<HashSet<String>> = OnceLock::new();
    SET.get_or_init(|| STOPWORDS.iter().map(|w| Analyzer::global().term(w)).collect())
}

pub fn is_stopword(term: &str) -> bool {
    stopwords().contains(term)
}

pub fn parse(query: &str) -> ParsedQuery {
    let analyzer = Analyzer::global();
    let mut terms: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut mode = Mode::All;

    for word in query.split_whitespace() {
        if word == "OR" {
            mode = Mode::Any;
            continue;
        }
        let (target, w) = match word.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (&mut excluded, rest),
            _ => (&mut terms, word),
        };
        for t in analyzer.tokens(w) {
            if !target.contains(&t) {
                target.push(t);
            }
        }
    }
    // A term both wanted and excluded is excluded.
    terms.retain(|t| !excluded.contains(t));

    let mut dropped_stopwords = Vec::new();
    if terms.iter().any(|t| !is_stopword(t)) {
        terms.retain(|t| {
            let stop = is_stopword(t);
            if stop {
                dropped_stopwords.push(t.clone());
            }
            !stop
        });
    }

    ParsedQuery {
        terms,
        excluded,
        mode,
        dropped_stopwords,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn and_by_default_with_exclusion() {
        let p = parse("fpga timing -xilinx");
        assert_eq!(p.terms, s(&["fpga", "time"]));
        assert_eq!(p.excluded, s(&["xilinx"]));
        assert_eq!(p.mode, Mode::All);
    }

    #[test]
    fn or_keyword_switches_mode() {
        let p = parse("verilog OR vhdl");
        assert_eq!(p.terms, s(&["verilog", "vhdl"]));
        assert_eq!(p.mode, Mode::Any);
        // Lowercase "or" is just a stopword.
        assert_eq!(parse("verilog or vhdl").mode, Mode::All);
    }

    #[test]
    fn stopwords_dropped_only_when_other_terms_exist() {
        let p = parse("chip that can be rewired after manufacturing");
        assert_eq!(p.terms, s(&["chip", "rewir", "manufactur"]));
        assert_eq!(p.dropped_stopwords, s(&["that", "can", "be", "after"]));

        let only = parse("the who");
        assert_eq!(only.terms, s(&["the", "who"]));
        assert!(only.dropped_stopwords.is_empty());
    }

    #[test]
    fn hyphens_dashes_and_duplicates() {
        let p = parse("field-programmable Gate gate - -");
        assert_eq!(p.terms, s(&["field", "programm", "gate"]));
        assert!(p.excluded.is_empty());

        let both = parse("rust -rust compiler");
        assert_eq!(both.terms, s(&["compil"]));
        assert_eq!(both.excluded, s(&["rust"]));
    }

    #[test]
    fn empty_and_punctuation() {
        assert!(parse("").terms.is_empty());
        assert!(parse("!!! ???").terms.is_empty());
        assert!(parse("OR").terms.is_empty());
    }
}
