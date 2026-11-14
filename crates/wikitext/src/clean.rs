//! Wikitext → plain text.
//!
//! A handful of passes, each a linear scan with no regexes:
//!
//! 1. HTML comments and tags. Content of `<ref>`, `<math>`, `<gallery>` and
//!    similar is dropped; other known tags are removed but their text is kept.
//! 2. Templates `{{…}}` and tables `{|…|}`, removed by counting nesting depth.
//! 3. Wiki links `[[Target|label]]` → `label` (target saved as a link), file and
//!    category links removed, external links `[url label]` → `label`.
//! 4. Per line: headings, list markers, rules, magic words, bold/italic quotes.
//! 5. HTML entities, then whitespace and leftover-junk lines.
//!
//! Scanning only ever cuts the string at ASCII pattern bytes, which are always
//! UTF-8 character boundaries, so slicing cannot panic on non-ASCII text.

/// Cleaned article text plus the outgoing article links found in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    /// Normalized link targets, in order of appearance, duplicates removed.
    pub links: Vec<String>,
}

pub fn clean(wikitext: &str) -> Cleaned {
    let s = strip_tags_and_comments(wikitext);
    let s = strip_templates_and_tables(&s);
    let mut links = Vec::new();
    let s = replace_links(&s, &mut links);
    let s = clean_lines(&s);
    let s = decode_entities(&s);
    let text = normalize_whitespace(&s);

    let mut seen = std::collections::HashSet::new();
    links.retain(|l| seen.insert(l.clone()));
    Cleaned { text, links }
}

/// If the wikitext is a `#REDIRECT [[Target]]` page, returns the target.
pub fn redirect_target(wikitext: &str) -> Option<String> {
    let t = wikitext.trim_start();
    let head = t.get(..9)?;
    if !head.eq_ignore_ascii_case("#redirect") {
        return None;
    }
    let open = t.find("[[")?;
    let rest = &t[open + 2..];
    let close = rest.find("]]")?;
    let target = rest[..close].split('|').next()?;
    let norm = normalize_title(target);
    (!norm.is_empty()).then_some(norm)
}

/// MediaWiki title normalization: underscores → spaces, collapse spaces,
/// drop `#section`, uppercase the first letter.
pub fn normalize_title(raw: &str) -> String {
    let no_section = raw.split('#').next().unwrap_or("");
    let spaced = no_section.replace('_', " ");
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Pass 1: comments and tags
// ---------------------------------------------------------------------------

/// Tags whose whole content is dropped.
const DROP_CONTENT_TAGS: &[&str] = &[
    "ref", "references", "math", "chem", "ce", "gallery", "timeline", "score", "imagemap",
    "mapframe", "maplink", "graph", "hiero", "syntaxhighlight", "source", "includeonly",
    "templatestyles", "inputbox", "categorytree", "charinsert", "indicator", "nowiki", "pre",
];

/// Tags removed while their inner text is kept.
const KEEP_CONTENT_TAGS: &[&str] = &[
    "br", "small", "big", "sup", "sub", "span", "div", "center", "blockquote", "code", "tt",
    "s", "u", "i", "b", "em", "strong", "p", "font", "abbr", "var", "del", "ins", "table",
    "tr", "td", "th", "caption", "ul", "ol", "li", "dl", "dt", "dd", "hr", "wbr", "section",
    "poem", "noinclude", "onlyinclude", "kbd", "samp", "q", "cite", "mark", "bdi", "ruby",
    "rt", "rp", "strike", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// Block-level tags that should leave a line break behind.
const BREAK_TAGS: &[&str] = &["br", "p", "div", "li", "tr", "hr", "blockquote", "poem"];

fn strip_tags_and_comments(s: &str) -> String {
    let lower = s.to_ascii_lowercase(); // same byte offsets as `s`
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copy_from = 0;
    let mut i = 0;

    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if lower[i..].starts_with("<!--") {
            out.push_str(&s[copy_from..i]);
            i = match lower[i + 4..].find("-->") {
                Some(end) => i + 4 + end + 3,
                None => b.len(), // unclosed comment hides the rest, as MediaWiki does
            };
            copy_from = i;
            continue;
        }
        let Some(tag) = parse_tag(&lower, i) else {
            i += 1;
            continue;
        };
        out.push_str(&s[copy_from..i]);
        let mut next = tag.end;
        // An unclosed drop-content tag loses just the tag, not the rest of the page.
        if DROP_CONTENT_TAGS.contains(&tag.name)
            && !tag.closing
            && !tag.self_closing
            && let Some(after_close) = find_closing_tag(&lower, tag.end, tag.name)
        {
            next = after_close;
        }
        if BREAK_TAGS.contains(&tag.name) {
            out.push('\n');
        }
        i = next;
        copy_from = i;
    }
    out.push_str(&s[copy_from..]);
    out
}

struct Tag {
    name: &'static str,
    closing: bool,
    self_closing: bool,
    /// Byte offset just past the tag's `>`.
    end: usize,
}

/// Recognizes `<name …>`, `</name>` or `<name …/>` at `i` for known tag names.
fn parse_tag(lower: &str, i: usize) -> Option<Tag> {
    let b = lower.as_bytes();
    let mut j = i + 1;
    let closing = b.get(j) == Some(&b'/');
    if closing {
        j += 1;
    }
    let name_start = j;
    while j < b.len() && (b[j].is_ascii_alphanumeric()) {
        j += 1;
    }
    let name = &lower[name_start..j];
    let known = DROP_CONTENT_TAGS
        .iter()
        .chain(KEEP_CONTENT_TAGS)
        .find(|t| **t == name)?;
    // Name must end at whitespace, '/', or '>' ("<bra>" is not "<br>").
    match b.get(j) {
        Some(b' ' | b'\t' | b'\n' | b'/' | b'>') => {}
        _ => return None,
    }
    // Attributes rarely contain '>', and never a newline-separated tag end.
    let gt = j + lower[j..].find('>')?;
    if lower[j..gt].contains('<') {
        return None;
    }
    let self_closing = gt > 0 && b[gt - 1] == b'/';
    Some(Tag {
        name: known,
        closing,
        self_closing,
        end: gt + 1,
    })
}

/// Finds `</name …>` from `from`, returning the offset just past it.
fn find_closing_tag(lower: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let mut search = from;
    loop {
        let at = search + lower[search..].find(&needle)?;
        let after = at + needle.len();
        match lower.as_bytes().get(after) {
            Some(b'>') => return Some(after + 1),
            Some(b' ' | b'\t' | b'\n') => return Some(after + lower[after..].find('>')? + 1),
            _ => search = after, // e.g. "</reference" while looking for "</ref"
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 2: templates and tables
// ---------------------------------------------------------------------------

fn strip_templates_and_tables(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut template_depth: u32 = 0;
    let mut table_depth: u32 = 0;
    let mut copy_from = 0;
    let mut i = 0;

    let at_line_start = |i: usize| {
        let line_begin = s[..i].rfind('\n').map_or(0, |n| n + 1);
        s[line_begin..i].bytes().all(|c| c == b' ' || c == b'\t')
    };

    while i < b.len() {
        let skipping = template_depth > 0 || table_depth > 0;
        let two = &b[i..(i + 2).min(b.len())];

        if two == b"{{" {
            if !skipping {
                out.push_str(&s[copy_from..i]);
            }
            template_depth += 1;
            i += 2;
        } else if two == b"}}" && template_depth > 0 {
            template_depth -= 1;
            i += 2;
            copy_from = i;
        } else if two == b"{|" && template_depth == 0 && at_line_start(i) {
            if !skipping {
                out.push_str(&s[copy_from..i]);
            }
            table_depth += 1;
            i += 2;
        } else if two == b"|}" && template_depth == 0 && table_depth > 0 && at_line_start(i) {
            table_depth -= 1;
            i += 2;
            copy_from = i;
        } else {
            i += 1;
        }
    }
    if template_depth == 0 && table_depth == 0 {
        out.push_str(&s[copy_from..]);
    }
    out
}

// ---------------------------------------------------------------------------
// Pass 3: links
// ---------------------------------------------------------------------------

/// Namespaces whose links are removed entirely (media, categories).
const DROP_NAMESPACES: &[&str] = &["file", "image", "media", "category"];

/// Other namespaces: the link text is kept but it is not an article link.
const OTHER_NAMESPACES: &[&str] = &[
    "wikipedia", "wp", "wikt", "wiktionary", "template", "help", "portal", "user", "special",
    "talk", "draft", "module", "mediawiki", "project", "wikibooks", "wikiquote", "wikisource",
    "wikinews", "wikiversity", "wikivoyage", "commons", "meta", "species", "s", "q", "n", "b",
    "v", "voy", "d", "w", "simple", "en",
];

fn replace_links(s: &str, links: &mut Vec<String>) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copy_from = 0;
    let mut i = 0;

    while i < b.len() {
        if b[i] != b'[' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'[') {
            let Some(close) = matching_double_bracket(b, i) else {
                i += 2;
                continue;
            };
            out.push_str(&s[copy_from..i]);
            let inner = &s[i + 2..close];
            out.push_str(&render_wiki_link(inner, links));
            i = close + 2;
            copy_from = i;
        } else if is_external_link_start(&s[i + 1..]) {
            let Some(close_rel) = s[i + 1..].find(']') else {
                i += 1;
                continue;
            };
            let inner = &s[i + 1..i + 1 + close_rel];
            if inner.contains('\n') {
                i += 1;
                continue;
            }
            out.push_str(&s[copy_from..i]);
            if let Some((_, label)) = inner.split_once(' ') {
                out.push_str(label.trim());
            }
            i = i + 1 + close_rel + 1;
            copy_from = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[copy_from..]);
    out
}

/// Index of the `]]` matching the `[[` at `open`, honoring nesting.
fn matching_double_bracket(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0u32;
    let mut i = open;
    while i + 1 < b.len() {
        match (b[i], b[i + 1]) {
            (b'[', b'[') => {
                depth += 1;
                i += 2;
            }
            (b']', b']') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    None
}

fn render_wiki_link(inner: &str, links: &mut Vec<String>) -> String {
    let (target_raw, label) = match inner.split_once('|') {
        Some((t, l)) => (t, Some(l)),
        None => (inner, None),
    };
    let mut target = target_raw.trim();
    let leading_colon = target.starts_with(':');
    if leading_colon {
        target = target[1..].trim_start();
    }

    let namespace = target
        .split_once(':')
        .map(|(ns, _)| ns.trim())
        .filter(|ns| !ns.is_empty() && !ns.contains(' ') || ns.eq_ignore_ascii_case("category"));

    if let Some(ns) = namespace {
        let ns_lower = ns.to_ascii_lowercase();
        let is_lang = is_language_code(ns);
        if !leading_colon && (DROP_NAMESPACES.contains(&ns_lower.as_str()) || is_lang) {
            return String::new();
        }
        if DROP_NAMESPACES.contains(&ns_lower.as_str())
            || OTHER_NAMESPACES.contains(&ns_lower.as_str())
            || is_lang
        {
            // Visible but not an article link: show the label, else the page name.
            let shown = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
                target.split_once(':').map_or(target, |(_, rest)| rest)
            });
            return replace_links(shown, &mut Vec::new());
        }
    }

    let norm = normalize_title(target);
    if !norm.is_empty() {
        links.push(norm);
    }
    match label {
        Some(l) if !l.trim().is_empty() => replace_links(l, links),
        // Pipe trick "[[Paris (band)|]]" shows "Paris"; plain "[[Paris]]" shows "Paris".
        Some(_) => strip_parenthetical(target).to_owned(),
        None => target.trim_start_matches('#').replace('#', " ").to_owned(),
    }
}

fn strip_parenthetical(t: &str) -> &str {
    match t.find(" (") {
        Some(i) if t.ends_with(')') => &t[..i],
        _ => t,
    }
}

/// Interlanguage prefixes look like "de", "fr", "zh-yue", "simple" is handled
/// as a namespace. All lowercase in practice.
fn is_language_code(ns: &str) -> bool {
    let mut parts = ns.split('-');
    let first = parts.next().unwrap_or("");
    (2..=3).contains(&first.len())
        && first.bytes().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_lowercase()))
}

fn is_external_link_start(rest: &str) -> bool {
    let b = rest.as_bytes();
    ["http://", "https://", "//", "ftp://", "mailto:"]
        .iter()
        .any(|p| b.len() >= p.len() && b[..p.len()].eq_ignore_ascii_case(p.as_bytes()))
}

// ---------------------------------------------------------------------------
// Pass 4: per-line markup
// ---------------------------------------------------------------------------

const MAGIC_WORDS: &[&str] = &[
    "__NOTOC__", "__TOC__", "__FORCETOC__", "__NOEDITSECTION__", "__NEWSECTIONLINK__",
    "__NOGALLERY__", "__NOINDEX__", "__INDEX__", "__STATICREDIRECT__", "__DISAMBIG__",
    "__HIDDENCAT__", "__NOTITLECONVERT__", "__NOCONTENTCONVERT__",
];

fn clean_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let mut l = line.trim();

        if l.starts_with("----") && l.bytes().all(|c| c == b'-') {
            continue;
        }
        if l.len() >= 2 && l.starts_with('=') && l.ends_with('=') {
            l = l.trim_matches('=').trim();
        } else {
            l = l.trim_start_matches(['*', '#', ':', ';']).trim_start();
        }

        let mut owned = strip_quote_runs(l);
        for w in MAGIC_WORDS {
            if owned.contains(w) {
                owned = owned.replace(w, "");
            }
        }
        out.push_str(&owned);
        out.push('\n');
    }
    out
}

/// Removes runs of two or more apostrophes ('' italic, ''' bold, ''''' both).
fn strip_quote_runs(l: &str) -> String {
    let mut out = String::with_capacity(l.len());
    let mut run = 0usize;
    for c in l.chars() {
        if c == '\'' {
            run += 1;
            continue;
        }
        if run == 1 {
            out.push('\'');
        }
        run = 0;
        out.push(c);
    }
    if run == 1 {
        out.push('\'');
    }
    out
}

// ---------------------------------------------------------------------------
// Pass 5: entities and whitespace
// ---------------------------------------------------------------------------

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let semi = after.find(';').filter(|&n| n > 0 && n <= 10);
        let decoded = semi.and_then(|n| decode_entity(&after[..n]).map(|c| (c, n)));
        match decoded {
            Some((c, n)) => {
                out.push(c);
                rest = &after[n + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" | "ensp" | "emsp" | "thinsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "minus" => '−',
        "times" => '×',
        "deg" => '°',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "euro" => '€',
        "pound" => '£',
        "frac12" => '½',
        "plusmn" => '±',
        "zwj" | "zwnj" | "shy" | "lrm" | "rlm" => '\u{200B}',
        _ => return None,
    })
}

/// Trims lines, collapses inner whitespace, and drops lines with no letters
/// or digits (leftover `|`, `}`, `)` and the like).
fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '\u{200B}')
            .filter(|w| !w.is_empty())
            .collect();
        if !words.iter().any(|w| w.chars().any(char::is_alphanumeric)) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&words.join(" "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(w: &str) -> String {
        clean(w).text
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(text("Hello world."), "Hello world.");
    }

    #[test]
    fn nested_templates_are_removed() {
        let w = "A {{Infobox|name={{lang|fr|x}}|y={{a|{{b}}}}}} B {{c}}C";
        assert_eq!(text(w), "A B C");
    }

    #[test]
    fn unclosed_template_swallows_rest_without_panic() {
        assert_eq!(text("Before {{broken|"), "Before");
        assert_eq!(text("x }} y"), "x }} y");
    }

    #[test]
    fn tables_are_removed_including_nested() {
        let w = "Intro\n{| class=\"wikitable\"\n|-\n| a || b\n{|\n| inner\n|}\n| c\n|}\nAfter";
        assert_eq!(text(w), "Intro\nAfter");
    }

    #[test]
    fn template_with_pipe_brace_inside_table() {
        let w = "{|\n| {{x|}} cell\n|}\nKept";
        assert_eq!(text(w), "Kept");
    }

    #[test]
    fn refs_and_comments() {
        let w = "Fact.<ref name=\"a\">{{cite web|url=x}}</ref> More<ref name=b/>.<!-- hidden --> End";
        assert_eq!(text(w), "Fact. More. End");
    }

    #[test]
    fn unclosed_ref_drops_only_the_tag() {
        assert_eq!(text("A <ref>dangling B"), "A dangling B");
    }

    #[test]
    fn unclosed_comment_hides_rest() {
        assert_eq!(text("A <!-- never closed\nB"), "A");
    }

    #[test]
    fn formatting_tags_keep_text_and_unknown_angles_survive() {
        assert_eq!(text("x<sup>2</sup> and <small>tiny</small>"), "x2 and tiny");
        assert_eq!(text("3 < 5 and 7 > 2"), "3 < 5 and 7 > 2");
        assert_eq!(text("a<brand>b"), "a<brand>b");
        assert_eq!(text("line<br/>break"), "line\nbreak");
    }

    #[test]
    fn links_become_labels_and_are_collected() {
        let c = clean("The [[field-programmable gate array|FPGA]] and [[ASIC]]s, [[Verilog#History]].");
        assert_eq!(c.text, "The FPGA and ASICs, Verilog History.");
        assert_eq!(c.links, ["Field-programmable gate array", "ASIC", "Verilog"]);
    }

    #[test]
    fn duplicate_links_are_kept_once() {
        let c = clean("[[Rust]] and [[rust|Rust again]] and [[Rust_]]");
        assert_eq!(c.links, ["Rust"]);
    }

    #[test]
    fn files_categories_and_interwiki_are_removed() {
        let w = "A [[File:X.jpg|thumb|A [[cat]] in [[Paris]]]] B [[Category:Cats]] [[de:Katze]] C";
        let c = clean(w);
        assert_eq!(c.text, "A B C");
        assert!(c.links.is_empty());
    }

    #[test]
    fn other_namespaces_show_text_but_are_not_links() {
        let c = clean("See [[wikt:dog|dog]] and [[:Category:Cats]].");
        assert_eq!(c.text, "See dog and Cats.");
        assert!(c.links.is_empty());
    }

    #[test]
    fn pipe_trick() {
        let c = clean("[[Paris (band)|]] played.");
        assert_eq!(c.text, "Paris played.");
        assert_eq!(c.links, ["Paris (band)"]);
    }

    #[test]
    fn external_links() {
        assert_eq!(text("See [https://example.com the site] or [http://x.org]."), "See the site or .");
        assert_eq!(text("[not a link] stays"), "[not a link] stays");
    }

    #[test]
    fn headings_lists_bold_italic() {
        let w = "== History ==\n'''Bold''' and ''italic'' and '''''both'''''. It's fine.\n* item one\n# item two\n: indented\n----";
        assert_eq!(
            text(w),
            "History\nBold and italic and both. It's fine.\nitem one\nitem two\nindented"
        );
    }

    #[test]
    fn magic_words_and_entities() {
        assert_eq!(text("__NOTOC__A&nbsp;B &ndash; C &amp; D &#169; &#x41; &bogus; &"), "A B – C & D © A &bogus; &");
    }

    #[test]
    fn junk_lines_dropped() {
        assert_eq!(text("Real line\n|\n}}\n)\n  \nSecond"), "Real line\nSecond");
    }

    #[test]
    fn empty_and_markup_only_pages() {
        assert_eq!(clean(""), Cleaned::default());
        assert_eq!(text("{{stub}}\n[[Category:X]]"), "");
    }

    #[test]
    fn non_ascii_is_safe() {
        assert_eq!(text("Zürich {{ü|é}} café ''naïve'' [[Ñandú|ñandú]]"), "Zürich café naïve ñandú");
    }

    #[test]
    fn redirects() {
        assert_eq!(redirect_target("#REDIRECT [[United States]]").as_deref(), Some("United States"));
        assert_eq!(redirect_target("#redirect[[foo_bar#Sec|x]]").as_deref(), Some("Foo bar"));
        assert_eq!(redirect_target("Normal text"), None);
        assert_eq!(redirect_target("#REDIRECT"), None);
    }

    #[test]
    fn title_normalization() {
        assert_eq!(normalize_title("  united_states  of  america#x"), "United states of america");
        assert_eq!(normalize_title("éclair"), "Éclair");
        assert_eq!(normalize_title("#only"), "");
    }
}

#[cfg(test)]
mod fuzz {
    use super::clean;
    use proptest::prelude::*;

    const PIECES: &[&str] = &[
        "{{", "}}", "{|", "|}", "[[", "]]", "[", "]", "|", "\n", "<ref>", "</ref>", "<ref name=x/>",
        "<!--", "-->", "'''", "''", "==", "*", "#", ":", "<br>", "<math>", "</math>", "&amp;",
        "&#", ";", "&", "<", ">", "http://x ", "File:", "Category:", "de:", "é", "ß", "日本",
        "word", " ", "(", ")", "#REDIRECT", "__TOC__", "{{{", "}}}", "-", "_",
    ];

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4000))]

        /// Arbitrary mixes of markup fragments never panic, and the output
        /// never contains a template opener. (Links are rendered after
        /// templates, so a nonsense target like `[[{|]]` can re-form `{{`;
        /// real titles cannot contain braces, so such inputs are excluded.)
        #[test]
        fn never_panics(parts in prop::collection::vec(prop::sample::select(PIECES), 0..60)) {
            let input: String = parts.concat();
            let out = clean(&input);
            if !input.contains("[[") {
                prop_assert!(!out.text.contains("{{"));
            }
        }

        #[test]
        fn never_panics_on_any_string(s in "\\PC{0,200}") {
            let _ = clean(&s);
        }
    }
}
