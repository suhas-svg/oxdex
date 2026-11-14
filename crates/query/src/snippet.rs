//! Snippets: the ~160-character window with the most distinct query terms,
//! HTML-escaped, with matches wrapped in `<mark>`.

use oxdex_text::Analyzer;

/// Only the first part of very long articles is scanned for a window.
const SCAN_BYTES: usize = 40_000;

struct Match {
    start: usize,
    end: usize,
    term: usize,
}

/// Builds an HTML-safe snippet of about `width` characters.
pub fn snippet(text: &str, terms: &[String], width: usize) -> String {
    let scan = &text[..text.floor_char_boundary(SCAN_BYTES.min(text.len()))];
    let matches: Vec<Match> = Analyzer::global()
        .tokens_with_offsets(scan)
        .filter_map(|(start, word, term)| {
            terms.iter().position(|t| *t == term).map(|i| Match { start, end: start + word.len(), term: i })
        })
        .collect();

    if matches.is_empty() {
        let end = cut_forward(text, 0, width);
        return wrap(text, 0, end, &[]);
    }

    // Best window: most distinct terms, then most matches, then earliest.
    let (mut best_i, mut best_j, mut best_key) = (0, 1, (0u32, 0usize));
    let mut j = 0;
    for i in 0..matches.len() {
        j = j.max(i + 1);
        while j < matches.len() && matches[j].end - matches[i].start <= width {
            j += 1;
        }
        let mut seen: u64 = 0;
        for m in &matches[i..j] {
            seen |= 1u64 << (m.term % 64);
        }
        let key = (seen.count_ones(), j - i);
        if key > best_key {
            (best_i, best_j, best_key) = (i, j, key);
        }
    }

    // Centre the matches in the window, snapped to word boundaries.
    let span = matches[best_j - 1].end - matches[best_i].start;
    let lead = width.saturating_sub(span) / 3;
    let start = cut_backward(text, matches[best_i].start, lead);
    let end = cut_forward(text, start, width).max(matches[best_j - 1].end);
    wrap(text, start, end, &matches[best_i..best_j])
}

/// A word-boundary position at most `back` bytes before `from`.
fn cut_backward(text: &str, from: usize, back: usize) -> usize {
    let lo = text.floor_char_boundary(from.saturating_sub(back));
    match text[lo..from].find(char::is_whitespace) {
        Some(ws) if lo > 0 => lo + ws + 1,
        _ => lo,
    }
}

/// A word-boundary position about `width` bytes after `from` (or the end).
fn cut_forward(text: &str, from: usize, width: usize) -> usize {
    let hi = text.floor_char_boundary((from + width).min(text.len()));
    if hi >= text.len() {
        return text.len();
    }
    text[from..hi].rfind(char::is_whitespace).map_or(hi, |ws| from + ws)
}

fn wrap(text: &str, start: usize, end: usize, marks: &[Match]) -> String {
    let mut out = String::with_capacity(end - start + 32);
    if start > 0 {
        out.push('…');
    }
    let mut pos = start;
    for m in marks.iter().filter(|m| m.start >= start && m.end <= end) {
        escape_into(&text[pos..m.start], &mut out);
        out.push_str("<mark>");
        escape_into(&text[m.start..m.end], &mut out);
        out.push_str("</mark>");
        pos = m.end;
    }
    escape_into(&text[pos..end], &mut out);
    if end < text.len() {
        out.push('…');
    }
    out
}

fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\n' | '\r' | '\t' => out.push(' '),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_window_with_most_distinct_terms() {
        let filler = "lorem ipsum dolor sit amet ".repeat(20);
        let text = format!(
            "FPGA appears here alone. {filler} Timing closure on an FPGA design needs static timing analysis. {filler}"
        );
        let s = snippet(&text, &terms(&["fpga", "time"]), 160);
        assert!(s.contains("<mark>FPGA</mark> design"), "{s}");
        assert!(s.contains("<mark>Timing</mark>"), "{s}");
        assert!(s.starts_with('…') && s.ends_with('…'));
        let visible = s.replace("<mark>", "").replace("</mark>", "");
        assert!(visible.chars().count() <= 175, "{} chars", visible.chars().count());
    }

    #[test]
    fn escapes_html_and_flattens_newlines() {
        let s = snippet("x <b>bold</b> & \"q\"\nnext rust", &terms(&["rust"]), 160);
        assert_eq!(s, "x &lt;b&gt;bold&lt;/b&gt; &amp; &quot;q&quot; next <mark>rust</mark>");
    }

    #[test]
    fn no_match_falls_back_to_the_start() {
        let text = "word ".repeat(100);
        let s = snippet(&text, &terms(&["absent"]), 50);
        assert!(!s.starts_with('…') && s.ends_with('…'));
        assert!(!s.contains("<mark>"));
    }

    #[test]
    fn short_text_and_unicode_are_safe() {
        assert_eq!(snippet("", &terms(&["x"]), 160), "");
        let s = snippet("Zürich café — naïve ünïcode Zürich", &terms(&["zürich"]), 12);
        assert!(s.contains("<mark>Zürich</mark>"), "{s}");
        for w in [1, 5, 13, 40] {
            let _ = snippet(&"é日本 ".repeat(30), &terms(&["日本"]), w);
        }
    }

    #[test]
    fn stems_match_inflections() {
        let s = snippet("He was running fast while others ran.", &terms(&["run"]), 160);
        assert!(s.contains("<mark>running</mark>"), "{s}");
    }
}
