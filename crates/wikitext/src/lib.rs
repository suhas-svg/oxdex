//! Reading Wikipedia dumps and turning wikitext into plain text.
//!
//! - [`multistream`]: the dump's index file and its independent bz2 streams.
//! - [`pages`]: streaming XML parsing of `<page>` elements inside one stream.
//! - [`clean`]: wikitext markup → plain text plus outgoing links.
//!
//! Nothing here panics on malformed input: bad markup degrades to slightly
//! dirtier text, and bad streams come back as errors the caller can count.

pub mod clean;
pub mod multistream;
pub mod pages;

pub use clean::{Cleaned, clean, normalize_title, redirect_target};
pub use multistream::{StreamRange, parse_index, stream_ranges};
pub use pages::{Page, SiteInfo, parse_pages, parse_siteinfo};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("XML attribute error: {0}")]
    XmlAttr(#[from] quick_xml::events::attributes::AttrError),
    #[error("bad multistream index line {line}: {text:?}")]
    BadIndexLine { line: usize, text: String },
}

pub type Result<T> = std::result::Result<T, Error>;
