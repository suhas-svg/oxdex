//! Memory-mapped reader over a finished index.
//!
//! Opening maps the files and checks headers; nothing is copied, so startup is
//! near-instant and the OS keeps hot pages cached.

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use crate::builder::{DICT_ENTRY_LEN, DICT_HEADER_LEN, DICT_MAGIC, DLEN_MAGIC, POST_MAGIC, SMALL_HEADER_LEN};
use crate::postings::BlockCursor;
use crate::{DocId, Error, FORMAT_VERSION, IndexMeta, Result, SearchIndex, io_err, u32_at, u64_at};

pub struct DiskIndex {
    dict: Mmap,
    post: Mmap,
    doclens: Mmap,
    meta: IndexMeta,
    num_terms: usize,
    heap_off: usize,
}

/// One dictionary entry with its encoded posting list.
#[derive(Debug, Clone, Copy)]
pub struct TermEntry<'a> {
    pub term: &'a [u8],
    pub df: u32,
    pub max_tf: u32,
    pub min_doc_len: u32,
    pub postings: &'a [u8],
}

pub(crate) fn map(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(io_err(path))?;
    // SAFETY: index files are written once and never modified in place; a
    // rebuild goes to a new directory (Windows cannot overwrite mapped files).
    unsafe { Mmap::map(&file) }.map_err(io_err(path))
}

pub(crate) fn check_magic(data: &[u8], magic: &[u8; 8], path: &Path, min_len: usize) -> Result<()> {
    if data.len() < min_len || &data[..8] != magic {
        return Err(Error::BadMagic { path: path.to_owned() });
    }
    let found = u32_at(data, 8);
    if found != FORMAT_VERSION {
        return Err(Error::BadVersion { path: path.to_owned(), found });
    }
    Ok(())
}

impl DiskIndex {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let meta_path = dir.join("meta.json");
        if !meta_path.exists() {
            return Err(Error::Incomplete(dir.to_owned()));
        }
        let meta: IndexMeta =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).map_err(io_err(&meta_path))?)?;
        if meta.format_version != FORMAT_VERSION {
            return Err(Error::BadVersion { path: meta_path, found: meta.format_version });
        }

        let dict_path = dir.join("terms.dict");
        let dict = map(&dict_path)?;
        check_magic(&dict, DICT_MAGIC, &dict_path, DICT_HEADER_LEN)?;
        let num_terms = u64_at(&dict, 16);
        let heap_off = u64_at(&dict, 24);
        let entries_end = DICT_HEADER_LEN as u64 + num_terms * DICT_ENTRY_LEN as u64;
        if heap_off != entries_end || heap_off > dict.len() as u64 || num_terms != meta.num_terms {
            return Err(Error::Corrupt(format!(
                "terms.dict header: {num_terms} terms, heap at {heap_off}, file {} bytes, meta says {} terms",
                dict.len(),
                meta.num_terms
            )));
        }

        let post_path = dir.join("postings.bin");
        let post = map(&post_path)?;
        check_magic(&post, POST_MAGIC, &post_path, SMALL_HEADER_LEN)?;

        let dl_path = dir.join("doclens.bin");
        let doclens = map(&dl_path)?;
        check_magic(&doclens, DLEN_MAGIC, &dl_path, SMALL_HEADER_LEN)?;
        if doclens.len() != SMALL_HEADER_LEN + 4 * meta.num_docs as usize {
            return Err(Error::Corrupt(format!(
                "doclens.bin is {} bytes, expected {} for {} docs",
                doclens.len(),
                SMALL_HEADER_LEN + 4 * meta.num_docs as usize,
                meta.num_docs
            )));
        }

        Ok(Self {
            dict,
            post,
            doclens,
            meta,
            num_terms: num_terms as usize,
            heap_off: heap_off as usize,
        })
    }

    pub fn meta(&self) -> &IndexMeta {
        &self.meta
    }

    pub fn num_terms(&self) -> usize {
        self.num_terms
    }

    fn entry_at(&self, i: usize) -> (u64, u64, u32, u32) {
        let at = DICT_HEADER_LEN + i * DICT_ENTRY_LEN;
        let e = &self.dict[at..at + DICT_ENTRY_LEN];
        (u64_at(e, 0), u64_at(e, 8), u32_at(e, 16), u32_at(e, 20))
    }

    fn term_bytes(&self, i: usize) -> &[u8] {
        let (str_off, _, _, str_len) = self.entry_at(i);
        let start = self.heap_off.saturating_add(str_off as usize);
        self.dict.get(start..start.saturating_add(str_len as usize)).unwrap_or(&[])
    }

    /// The `i`-th term in byte order (0 ≤ i < num_terms).
    pub fn term_entry(&self, i: usize) -> TermEntry<'_> {
        let (_, post_off, df, _) = self.entry_at(i);
        let end = if i + 1 < self.num_terms { self.entry_at(i + 1).1 } else { self.post.len() as u64 };
        let postings = self.post.get(post_off as usize..end as usize).unwrap_or(&[]);
        let at = DICT_HEADER_LEN + i * DICT_ENTRY_LEN;
        let max_tf = u32_at(&self.dict[at..], 24);
        let min_doc_len = u32_at(&self.dict[at..], 28);
        TermEntry { term: self.term_bytes(i), df, max_tf, min_doc_len, postings }
    }

    /// Binary search over the sorted dictionary.
    pub fn find(&self, term: &str) -> Option<usize> {
        let key = term.as_bytes();
        let (mut lo, mut hi) = (0usize, self.num_terms);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.term_bytes(mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    pub fn doc_freq(&self, term: &str) -> u32 {
        self.find(term).map_or(0, |i| self.term_entry(i).df)
    }
}

impl SearchIndex for DiskIndex {
    type Cursor<'a> = BlockCursor<'a>;

    fn num_docs(&self) -> u32 {
        self.meta.num_docs
    }

    fn avgdl(&self) -> f64 {
        // Recomputed from the exact integers: the float in meta.json can come
        // back one ULP off after a JSON round trip, which would shift scores.
        if self.meta.num_docs == 0 {
            0.0
        } else {
            self.meta.total_tokens as f64 / f64::from(self.meta.num_docs)
        }
    }

    fn doc_len(&self, doc: DocId) -> u32 {
        let at = SMALL_HEADER_LEN + 4 * doc as usize;
        self.doclens.get(at..at + 4).map_or(0, |b| u32_at(b, 0))
    }

    fn cursor(&self, term: &str) -> Option<BlockCursor<'_>> {
        let e = self.term_entry(self.find(term)?);
        Some(BlockCursor::new(e.postings, e.df).with_bounds(e.max_tf, e.min_doc_len))
    }
}
