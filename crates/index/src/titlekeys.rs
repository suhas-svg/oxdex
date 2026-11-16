//! Exact-title lookup: `title_keys.bin` maps a hash of each analyzed title and
//! redirect title to its doc ID, so "gravity" can find the article named
//! *Gravity* ahead of *Gravity (2013 movie)*.
//!
//! Layout: 16-byte header (`OXDXTKEY`, version, reserved), u64 entry count,
//! then entries of `u64 key, u32 doc` sorted by (key, doc), 12 bytes each.
//! Keys are FNV-1a over the analyzed terms, so they are stable across builds
//! and platforms. A 64-bit collision across ~20M titles is unlikely enough to
//! ignore; the cost of one would be a single wrongly boosted result.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;
use oxdex_text::Analyzer;

use crate::builder::{SMALL_HEADER_LEN, write_small_header};
use crate::disk::{check_magic, map};
use crate::{DocId, Error, Result, io_err};

pub const TITLE_KEYS_FILE: &str = "title_keys.bin";
const MAGIC: &[u8; 8] = b"OXDXTKEY";
const ENTRY: usize = 12;
const DATA_START: usize = SMALL_HEADER_LEN + 8;

/// Hash of the analyzed terms of `text`, or `None` if it has no terms.
pub fn title_key(text: &str) -> Option<u64> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut any = false;
    for term in Analyzer::global().tokens(text) {
        if any {
            h = fnv(h, &[0x1f]);
        }
        h = fnv(h, term.as_bytes());
        any = true;
    }
    any.then_some(h)
}

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Default)]
pub struct TitleKeysWriter {
    entries: Vec<(u64, DocId)>,
}

impl TitleKeysWriter {
    pub fn add(&mut self, doc: DocId, title: &str) {
        if let Some(k) = title_key(title) {
            self.entries.push((k, doc));
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Writes `<dir>/title_keys.bin`; returns the number of distinct entries.
    pub fn write(mut self, dir: &Path) -> Result<usize> {
        self.entries.sort_unstable();
        self.entries.dedup();
        let path = dir.join(TITLE_KEYS_FILE);
        let f = File::create(&path).map_err(io_err(&path))?;
        let mut w = BufWriter::new(f);
        (|| {
            write_small_header(&mut w, MAGIC)?;
            w.write_all(&(self.entries.len() as u64).to_le_bytes())?;
            for (k, d) in &self.entries {
                w.write_all(&k.to_le_bytes())?;
                w.write_all(&d.to_le_bytes())?;
            }
            w.flush()
        })()
        .map_err(io_err(&path))?;
        Ok(self.entries.len())
    }
}

pub struct TitleKeys {
    data: Mmap,
    len: usize,
}

impl TitleKeys {
    /// Opens `<dir>/title_keys.bin`, or returns `Ok(None)` if the index has none.
    pub fn open(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(TITLE_KEYS_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let data = map(&path)?;
        check_magic(&data, MAGIC, &path, DATA_START)?;
        let len = u64::from_le_bytes(data[SMALL_HEADER_LEN..DATA_START].try_into().unwrap()) as usize;
        if data.len() != DATA_START + len * ENTRY {
            return Err(Error::Corrupt(format!("{}: {len} entries but {} bytes", path.display(), data.len())));
        }
        Ok(Some(Self { data, len }))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn key(&self, i: usize) -> u64 {
        let at = DATA_START + i * ENTRY;
        u64::from_le_bytes(self.data[at..at + 8].try_into().unwrap())
    }

    fn doc(&self, i: usize) -> DocId {
        let at = DATA_START + i * ENTRY + 8;
        u32::from_le_bytes(self.data[at..at + 4].try_into().unwrap())
    }

    /// Docs whose title or a redirect to them analyzes to the same terms as `text`.
    pub fn lookup(&self, text: &str) -> Vec<DocId> {
        let Some(k) = title_key(text) else { return Vec::new() };
        // First index whose key is >= k.
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) < k { lo = mid + 1 } else { hi = mid }
        }
        let mut out = Vec::new();
        while lo < self.len && self.key(lo) == k {
            out.push(self.doc(lo));
            lo += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_case_punctuation_and_inflection() {
        assert_eq!(title_key("The Beatles"), title_key("the beatles"));
        assert_eq!(title_key("Field-programmable gate array"), title_key("field programmable gate arrays"));
        assert_ne!(title_key("Gravity"), title_key("Gravity (2013 movie)"));
        assert_eq!(title_key("  ...  "), None);
    }

    #[test]
    fn round_trip_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TitleKeysWriter::default();
        w.add(7, "Gravity");
        w.add(9, "Gravity (2013 movie)");
        w.add(3, "Gravitation"); // a redirect, say
        w.add(7, "gravity"); // duplicate after analysis
        w.add(2, "Gravity");
        assert_eq!(w.write(dir.path()).unwrap(), 4);
        let t = TitleKeys::open(dir.path()).unwrap().unwrap();
        assert_eq!(t.lookup("GRAVITY"), vec![2, 7]);
        assert_eq!(t.lookup("gravity 2013 movie"), vec![9]);
        assert!(t.lookup("levity").is_empty());
        assert!(TitleKeys::open(&dir.path().join("nope")).unwrap().is_none());
    }
}
