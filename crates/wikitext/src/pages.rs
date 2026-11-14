//! Streaming parse of `<page>` elements with quick-xml: one tag at a time,
//! never the whole document in memory.

use std::io::BufRead;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::Result;

/// One `<page>` from the dump, before any cleaning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub ns: i32,
    /// Wikipedia's own page ID (not the dense Oxdex doc ID).
    pub id: u64,
    /// Target title when the page is a redirect.
    pub redirect: Option<String>,
    /// Raw wikitext of the latest revision.
    pub text: String,
}

/// What we need from the `<siteinfo>` header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteInfo {
    pub sitename: String,
    /// e.g. `https://simple.wikipedia.org/wiki/`
    pub article_url_prefix: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    None,
    Title,
    Ns,
    Id,
    Text,
}

/// Parses every complete `<page>` in `input`, calling `on_page` for each.
///
/// Works on a bare sequence of pages (a middle stream), and tolerates the
/// unmatched `</mediawiki>` that ends the last stream.
pub fn parse_pages(input: impl BufRead, mut on_page: impl FnMut(Page)) -> Result<()> {
    let mut reader = Reader::from_reader(input);
    let cfg = reader.config_mut();
    cfg.check_end_names = false;
    cfg.allow_unmatched_ends = true; // the last stream ends with a lone </mediawiki>
    cfg.trim_text(false);

    let mut buf = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut page: Option<Page> = None;
    let mut field = Field::None;
    let mut scratch = String::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_vec();
                if name == b"page" {
                    page = Some(Page::default());
                } else if let Some(p) = page.as_mut() {
                    field = field_for(&stack, &name);
                    scratch.clear();
                    if name == b"redirect" {
                        p.redirect = redirect_attr(&e)?;
                    }
                }
                stack.push(name);
            }
            Event::Empty(e) => {
                if let Some(p) = page.as_mut()
                    && e.local_name().as_ref() == b"redirect"
                    && stack.last().is_some_and(|n| n == b"page")
                {
                    p.redirect = redirect_attr(&e)?;
                }
            }
            Event::Text(t) => {
                if field != Field::None {
                    scratch.push_str(&t.unescape()?);
                }
            }
            Event::CData(t) => {
                if field != Field::None {
                    scratch.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                if let Some(p) = page.as_mut() {
                    match field {
                        Field::Title => p.title = std::mem::take(&mut scratch),
                        Field::Ns => p.ns = scratch.trim().parse().unwrap_or(i32::MIN),
                        Field::Id => p.id = scratch.trim().parse().unwrap_or(0),
                        Field::Text => p.text = std::mem::take(&mut scratch),
                        Field::None => {}
                    }
                    field = Field::None;
                    if name.as_ref() == b"page"
                        && let Some(done) = page.take()
                    {
                        on_page(done);
                    }
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Which page field a start tag opens, judged by its parent element so that
/// `<id>` inside `<revision>` or `<contributor>` is ignored.
fn field_for(stack: &[Vec<u8>], name: &[u8]) -> Field {
    let parent = stack.last().map(Vec::as_slice);
    match (parent, name) {
        (Some(b"page"), b"title") => Field::Title,
        (Some(b"page"), b"ns") => Field::Ns,
        (Some(b"page"), b"id") => Field::Id,
        (Some(b"revision"), b"text") => Field::Text,
        _ => Field::None,
    }
}

fn redirect_attr(e: &BytesStart) -> Result<Option<String>> {
    Ok(match e.try_get_attribute("title")? {
        Some(a) => Some(a.unescape_value()?.into_owned()),
        None => None,
    })
}

/// Parses the dump header (`<mediawiki><siteinfo>…`).
pub fn parse_siteinfo(input: impl BufRead) -> Result<SiteInfo> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut info = SiteInfo::default();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => current = e.local_name().as_ref().to_vec(),
            Event::Text(t) => {
                let text = t.unescape()?;
                match current.as_slice() {
                    b"sitename" => info.sitename = text.trim().to_owned(),
                    b"base" => {
                        // "https://simple.wikipedia.org/wiki/Main_Page" → ".../wiki/"
                        let base = text.trim();
                        info.article_url_prefix = match base.rfind('/') {
                            Some(i) => base[..=i].to_owned(),
                            None => base.to_owned(),
                        };
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"siteinfo" {
                    break;
                }
                current.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = r#"
  <page>
    <title>Alan Turing</title>
    <ns>0</ns>
    <id>13</id>
    <revision>
      <id>9001</id>
      <contributor><username>X</username><id>77</id></contributor>
      <text bytes="40" xml:space="preserve">'''Alan Turing''' was a &lt;b&gt;mathematician&lt;/b&gt; &amp;c.</text>
    </revision>
  </page>
  <page>
    <title>Turing</title>
    <ns>0</ns>
    <id>14</id>
    <redirect title="Alan Turing" />
    <revision><id>9002</id><text xml:space="preserve">#REDIRECT [[Alan Turing]]</text></revision>
  </page>
  <page>
    <title>Talk:Alan Turing</title>
    <ns>1</ns>
    <id>15</id>
    <revision><id>9003</id><text xml:space="preserve" /></revision>
  </page>
</mediawiki>
"#;

    #[test]
    fn parses_fields_redirects_and_trailing_root_close() {
        let mut pages = Vec::new();
        parse_pages(STREAM.as_bytes(), |p| pages.push(p)).unwrap();
        assert_eq!(pages.len(), 3);

        assert_eq!(pages[0].title, "Alan Turing");
        assert_eq!(pages[0].ns, 0);
        assert_eq!(pages[0].id, 13, "page id, not revision or contributor id");
        assert_eq!(pages[0].redirect, None);
        assert_eq!(pages[0].text, "'''Alan Turing''' was a <b>mathematician</b> &c.");

        assert_eq!(pages[1].redirect.as_deref(), Some("Alan Turing"));
        assert_eq!(pages[2].ns, 1);
        assert_eq!(pages[2].text, "");
    }

    #[test]
    fn truncated_stream_drops_partial_page() {
        let cut = &STREAM[..STREAM.find("<title>Turing").unwrap()];
        let mut pages = Vec::new();
        let _ = parse_pages(cut.as_bytes(), |p| pages.push(p));
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn siteinfo_url_prefix() {
        let head = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11">
  <siteinfo>
    <sitename>Wikipedia</sitename>
    <base>https://simple.wikipedia.org/wiki/Main_Page</base>
    <namespaces><namespace key="0" case="first-letter" /></namespaces>
  </siteinfo>
"#;
        let info = parse_siteinfo(head.as_bytes()).unwrap();
        assert_eq!(info.sitename, "Wikipedia");
        assert_eq!(info.article_url_prefix, "https://simple.wikipedia.org/wiki/");
    }
}
