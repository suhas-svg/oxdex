//! Document store: articles packed into zstd-compressed blocks.
//!
//! Files in the store directory (all integers little-endian):
//!
//! - `docs.bin`: 16-byte header (`OXDXDOCS`, u32 version, u32 reserved), then
//!   zstd blocks of ~64 KiB uncompressed. A block holds consecutive records:
//!   `u32 doc_id, u32 len + title, u32 len + url, u32 len + text`.
//! - `docs.idx`: 24-byte header (`OXDXDIDX`, u32 version, u32 num_docs,
//!   u32 num_blocks, u32 reserved), then per block `u32 first_doc, u32
//!   compressed_len, u64 offset`. A doc's block is found by binary search.
//! - `links.bin`: 16-byte header (`OXDXLINK`, u32 version, u32 reserved), then
//!   one zstd stream of `u32 doc_id, u32 count, count × (u16 len + title)`.
//!   Read sequentially (for PageRank later), not by random access.
//!
//! Doc IDs are dense: 0, 1, 2, … in the order documents are added.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub const FORMAT_VERSION: u32 = 1;
pub const DEFAULT_BLOCK_BYTES: usize = 64 * 1024;

const DOCS_MAGIC: &[u8; 8] = b"OXDXDOCS";
const IDX_MAGIC: &[u8; 8] = b"OXDXDIDX";
const LINKS_MAGIC: &[u8; 8] = b"OXDXLINK";
const DOCS_HEADER_LEN: u64 = 16;
const IDX_HEADER_LEN: usize = 24;
const IDX_ENTRY_LEN: usize = 16;

pub type DocId = u32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not an Oxdex {what} file (bad magic bytes)")]
    BadMagic { path: PathBuf, what: &'static str },
    #[error("{path} has format version {found}, this build reads version {FORMAT_VERSION}")]
    BadVersion { path: PathBuf, found: u32 },
    #[error("corrupt document store: {0}")]
    Corrupt(String),
    #[error("doc {0} is out of range")]
    NotFound(DocId),
    #[error("too many documents for u32 doc IDs")]
    TooManyDocs,
}

pub type Result<T> = std::result::Result<T, Error>;

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub id: DocId,
    pub title: String,
    pub url: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockEntry {
    first_doc: DocId,
    compressed_len: u32,
    offset: u64,
}

/// What a finished store holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub num_docs: u32,
    pub num_blocks: u32,
    pub num_links: u64,
    /// Uncompressed title + url + text bytes.
    pub raw_bytes: u64,
    /// Size of docs.bin.
    pub docs_bytes: u64,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub struct DocStoreWriter {
    dir: PathBuf,
    docs: BufWriter<File>,
    docs_pos: u64,
    block: Vec<u8>,
    block_first: DocId,
    block_bytes: usize,
    blocks: Vec<BlockEntry>,
    links: zstd::Encoder<'static, BufWriter<File>>,
    level: i32,
    next_id: u64,
    summary: Summary,
}

impl DocStoreWriter {
    /// Creates (or overwrites) a store in `dir`.
    pub fn create(dir: impl AsRef<Path>) -> Result<Self> {
        Self::with_options(dir, DEFAULT_BLOCK_BYTES, 3)
    }

    pub fn with_options(dir: impl AsRef<Path>, block_bytes: usize, level: i32) -> Result<Self> {
        let dir = dir.as_ref().to_owned();
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;

        let docs_path = dir.join("docs.bin");
        let mut docs = BufWriter::new(File::create(&docs_path).map_err(io_err(&docs_path))?);
        write_header(&mut docs, DOCS_MAGIC, &[FORMAT_VERSION, 0]).map_err(io_err(&docs_path))?;

        let links_path = dir.join("links.bin");
        let mut links_file =
            BufWriter::new(File::create(&links_path).map_err(io_err(&links_path))?);
        write_header(&mut links_file, LINKS_MAGIC, &[FORMAT_VERSION, 0])
            .map_err(io_err(&links_path))?;
        let links = zstd::Encoder::new(links_file, level).map_err(io_err(&links_path))?;

        Ok(Self {
            dir,
            docs,
            docs_pos: DOCS_HEADER_LEN,
            block: Vec::with_capacity(block_bytes + block_bytes / 4),
            block_first: 0,
            block_bytes,
            blocks: Vec::new(),
            links,
            level,
            next_id: 0,
            summary: Summary::default(),
        })
    }

    /// Appends a document and returns its doc ID.
    pub fn add(&mut self, title: &str, url: &str, text: &str, links: &[String]) -> Result<DocId> {
        let id = DocId::try_from(self.next_id).map_err(|_| Error::TooManyDocs)?;
        self.next_id += 1;

        if self.block.is_empty() {
            self.block_first = id;
        }
        self.block.extend_from_slice(&id.to_le_bytes());
        for field in [title, url, text] {
            put_bytes32(&mut self.block, field.as_bytes())?;
        }
        self.summary.raw_bytes += (title.len() + url.len() + text.len()) as u64;

        let links_path = self.dir.join("links.bin");
        let mut rec = Vec::with_capacity(8 + links.len() * 16);
        rec.extend_from_slice(&id.to_le_bytes());
        let kept: Vec<&String> = links.iter().filter(|l| l.len() <= u16::MAX as usize).collect();
        rec.extend_from_slice(&(kept.len() as u32).to_le_bytes());
        for l in &kept {
            rec.extend_from_slice(&(l.len() as u16).to_le_bytes());
            rec.extend_from_slice(l.as_bytes());
        }
        self.links.write_all(&rec).map_err(io_err(&links_path))?;
        self.summary.num_links += kept.len() as u64;

        if self.block.len() >= self.block_bytes {
            self.flush_block()?;
        }
        Ok(id)
    }

    fn flush_block(&mut self) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let path = self.dir.join("docs.bin");
        let compressed = zstd::bulk::compress(&self.block, self.level).map_err(io_err(&path))?;
        let compressed_len = u32::try_from(compressed.len())
            .map_err(|_| Error::Corrupt("block over 4 GiB".into()))?;
        self.docs.write_all(&compressed).map_err(io_err(&path))?;
        self.blocks.push(BlockEntry {
            first_doc: self.block_first,
            compressed_len,
            offset: self.docs_pos,
        });
        self.docs_pos += u64::from(compressed_len);
        self.block.clear();
        Ok(())
    }

    /// Flushes the last block and writes the index. The store is unreadable
    /// until this returns.
    pub fn finish(mut self) -> Result<Summary> {
        self.flush_block()?;

        let docs_path = self.dir.join("docs.bin");
        self.docs.flush().map_err(io_err(&docs_path))?;

        let links_path = self.dir.join("links.bin");
        self.links
            .finish()
            .and_then(|mut w| w.flush())
            .map_err(io_err(&links_path))?;

        let num_docs = DocId::try_from(self.next_id).map_err(|_| Error::TooManyDocs)?;
        let num_blocks = self.blocks.len() as u32;
        let idx_path = self.dir.join("docs.idx");
        let mut idx = BufWriter::new(File::create(&idx_path).map_err(io_err(&idx_path))?);
        write_header(&mut idx, IDX_MAGIC, &[FORMAT_VERSION, num_docs, num_blocks, 0])
            .map_err(io_err(&idx_path))?;
        for b in &self.blocks {
            idx.write_all(&b.first_doc.to_le_bytes())
                .and_then(|_| idx.write_all(&b.compressed_len.to_le_bytes()))
                .and_then(|_| idx.write_all(&b.offset.to_le_bytes()))
                .map_err(io_err(&idx_path))?;
        }
        idx.flush().map_err(io_err(&idx_path))?;

        Ok(Summary {
            num_docs,
            num_blocks,
            docs_bytes: self.docs_pos,
            ..self.summary
        })
    }
}

fn write_header(w: &mut impl Write, magic: &[u8; 8], fields: &[u32]) -> io::Result<()> {
    w.write_all(magic)?;
    for f in fields {
        w.write_all(&f.to_le_bytes())?;
    }
    Ok(())
}

fn put_bytes32(buf: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| Error::Corrupt("field over 4 GiB".into()))?;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(bytes);
    Ok(())
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Read-only view of a store. `docs.bin` is memory-mapped; `docs.idx` is
/// loaded into memory (16 bytes per block).
pub struct DocStore {
    mmap: Mmap,
    blocks: Vec<BlockEntry>,
    num_docs: u32,
}

impl DocStore {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();

        let idx_path = dir.join("docs.idx");
        let idx = std::fs::read(&idx_path).map_err(io_err(&idx_path))?;
        let header = check_header(&idx, IDX_MAGIC, "index", &idx_path, IDX_HEADER_LEN)?;
        let num_docs = header[0];
        let num_blocks = header[1] as usize;
        let body = &idx[IDX_HEADER_LEN..];
        if body.len() != num_blocks * IDX_ENTRY_LEN {
            return Err(Error::Corrupt(format!(
                "docs.idx says {num_blocks} blocks but holds {} bytes of entries",
                body.len()
            )));
        }
        let blocks: Vec<BlockEntry> = body
            .as_chunks::<IDX_ENTRY_LEN>()
            .0
            .iter()
            .map(|e| BlockEntry {
                first_doc: u32::from_le_bytes(e[0..4].try_into().unwrap()),
                compressed_len: u32::from_le_bytes(e[4..8].try_into().unwrap()),
                offset: u64::from_le_bytes(e[8..16].try_into().unwrap()),
            })
            .collect();

        let docs_path = dir.join("docs.bin");
        let file = File::open(&docs_path).map_err(io_err(&docs_path))?;
        // SAFETY: the store is written once and never modified while served;
        // new builds go to a new directory.
        let mmap = unsafe { Mmap::map(&file) }.map_err(io_err(&docs_path))?;
        check_header(&mmap, DOCS_MAGIC, "document", &docs_path, DOCS_HEADER_LEN as usize)?;
        for b in &blocks {
            let end = b.offset + u64::from(b.compressed_len);
            if end > mmap.len() as u64 {
                return Err(Error::Corrupt(format!("block at {} runs past end of docs.bin", b.offset)));
            }
        }

        Ok(Self {
            mmap,
            blocks,
            num_docs,
        })
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    pub fn num_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Fetches one document (decompresses its whole ~64 KiB block).
    pub fn get(&self, id: DocId) -> Result<Doc> {
        if id >= self.num_docs {
            return Err(Error::NotFound(id));
        }
        let block_idx = self.blocks.partition_point(|b| b.first_doc <= id).saturating_sub(1);
        self.decode_block(block_idx)?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or(Error::NotFound(id))
    }

    /// All documents in doc ID order, one block at a time.
    pub fn iter(&self) -> impl Iterator<Item = Result<Doc>> + '_ {
        (0..self.blocks.len()).flat_map(move |i| match self.decode_block(i) {
            Ok(docs) => docs.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(e) => vec![Err(e)],
        })
    }

    fn decode_block(&self, i: usize) -> Result<Vec<Doc>> {
        let b = self.blocks[i];
        let start = b.offset as usize;
        let compressed = &self.mmap[start..start + b.compressed_len as usize];
        let raw = zstd::decode_all(compressed)
            .map_err(|e| Error::Corrupt(format!("block {i} does not decompress: {e}")))?;

        let mut cur = Cursor { buf: &raw, pos: 0 };
        let mut docs = Vec::new();
        while cur.pos < raw.len() {
            let id = cur.u32()?;
            let title = cur.string()?;
            let url = cur.string()?;
            let text = cur.string()?;
            docs.push(Doc { id, title, url, text });
        }
        Ok(docs)
    }
}

fn check_header(
    data: &[u8],
    magic: &[u8; 8],
    what: &'static str,
    path: &Path,
    header_len: usize,
) -> Result<Vec<u32>> {
    if data.len() < header_len || &data[..8] != magic {
        return Err(Error::BadMagic {
            path: path.to_owned(),
            what,
        });
    }
    let fields: Vec<u32> = data[8..header_len]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    if fields[0] != FORMAT_VERSION {
        return Err(Error::BadVersion {
            path: path.to_owned(),
            found: fields[0],
        });
    }
    Ok(fields[1..].to_vec())
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| Error::Corrupt(format!("record truncated at byte {}", self.pos)))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| Error::Corrupt("record is not UTF-8".into()))
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// Sequential reader over `links.bin`: yields `(doc_id, link targets)`.
pub struct LinksReader {
    stream: zstd::Decoder<'static, BufReader<File>>,
}

impl LinksReader {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let path = dir.as_ref().join("links.bin");
        let mut file = File::open(&path).map_err(io_err(&path))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header).map_err(io_err(&path))?;
        check_header(&header, LINKS_MAGIC, "links", &path, 16)?;
        let stream = zstd::Decoder::new(file).map_err(io_err(&path))?;
        Ok(Self { stream })
    }

    fn read_record(&mut self) -> Result<Option<(DocId, Vec<String>)>> {
        let mut head = [0u8; 8];
        match self.stream.read_exact(&mut head[..4]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(Error::Corrupt(format!("links.bin: {e}"))),
        }
        let corrupt = |e: io::Error| Error::Corrupt(format!("links.bin: {e}"));
        self.stream.read_exact(&mut head[4..]).map_err(corrupt)?;
        let id = u32::from_le_bytes(head[..4].try_into().unwrap());
        let count = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
        let mut links = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let mut len = [0u8; 2];
            self.stream.read_exact(&mut len).map_err(corrupt)?;
            let mut bytes = vec![0u8; u16::from_le_bytes(len) as usize];
            self.stream.read_exact(&mut bytes).map_err(corrupt)?;
            links.push(
                String::from_utf8(bytes).map_err(|_| Error::Corrupt("link is not UTF-8".into()))?,
            );
        }
        Ok(Some((id, links)))
    }
}

impl Iterator for LinksReader {
    type Item = Result<(DocId, Vec<String>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_record().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u32) -> (String, String, String, Vec<String>) {
        (
            format!("Title {i}"),
            format!("https://example.org/wiki/Title_{i}"),
            format!("Body of article {i}. ").repeat((i % 7 + 1) as usize * 40),
            (0..i % 4).map(|j| format!("Link {j} ü")).collect(),
        )
    }

    fn build(dir: &Path, n: u32, block_bytes: usize) -> Summary {
        let mut w = DocStoreWriter::with_options(dir, block_bytes, 1).unwrap();
        for i in 0..n {
            let (t, u, x, l) = sample(i);
            assert_eq!(w.add(&t, &u, &x, &l).unwrap(), i);
        }
        w.finish().unwrap()
    }

    #[test]
    fn round_trip_across_many_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let s = build(dir.path(), 500, 4096);
        assert_eq!(s.num_docs, 500);
        assert!(s.num_blocks > 50, "small blocks force many of them");

        let store = DocStore::open(dir.path()).unwrap();
        assert_eq!(store.num_docs(), 500);
        for i in [0, 1, 7, 123, 498, 499] {
            let d = store.get(i).unwrap();
            let (t, u, x, _) = sample(i);
            assert_eq!(d, Doc { id: i, title: t, url: u, text: x });
        }
        assert!(matches!(store.get(500), Err(Error::NotFound(500))));

        let all: Vec<Doc> = store.iter().collect::<Result<_>>().unwrap();
        assert_eq!(all.len(), 500);
        assert!(all.iter().enumerate().all(|(i, d)| d.id == i as u32));
    }

    #[test]
    fn links_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = build(dir.path(), 50, DEFAULT_BLOCK_BYTES);
        let recs: Vec<_> = LinksReader::open(dir.path()).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(recs.len(), 50);
        for (i, (id, links)) in recs.iter().enumerate() {
            assert_eq!(*id, i as u32);
            assert_eq!(*links, sample(i as u32).3);
        }
        assert_eq!(s.num_links, recs.iter().map(|r| r.1.len() as u64).sum::<u64>());
    }

    #[test]
    fn oversized_doc_gets_its_own_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = DocStoreWriter::with_options(dir.path(), 1024, 1).unwrap();
        w.add("small", "u", "x", &[]).unwrap();
        let big = "é".repeat(10_000);
        w.add("big", "u", &big, &[]).unwrap();
        w.add("after", "u", "y", &[]).unwrap();
        w.finish().unwrap();

        let store = DocStore::open(dir.path()).unwrap();
        assert_eq!(store.get(1).unwrap().text, big);
        assert_eq!(store.get(2).unwrap().title, "after");
    }

    #[test]
    fn empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = build(dir.path(), 0, DEFAULT_BLOCK_BYTES);
        assert_eq!(s.num_docs, 0);
        let store = DocStore::open(dir.path()).unwrap();
        assert!(store.iter().next().is_none());
        assert!(matches!(store.get(0), Err(Error::NotFound(0))));
    }

    #[test]
    fn rejects_wrong_magic_and_version() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), 3, DEFAULT_BLOCK_BYTES);
        let idx = dir.path().join("docs.idx");

        let mut bytes = std::fs::read(&idx).unwrap();
        bytes[8] = 99; // version
        std::fs::write(&idx, &bytes).unwrap();
        assert!(matches!(DocStore::open(dir.path()), Err(Error::BadVersion { found: 99, .. })));

        bytes[0] = b'X';
        std::fs::write(&idx, &bytes).unwrap();
        assert!(matches!(DocStore::open(dir.path()), Err(Error::BadMagic { .. })));
    }

    #[test]
    fn truncated_index_is_corrupt_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), 300, 2048);
        let idx = dir.path().join("docs.idx");
        let bytes = std::fs::read(&idx).unwrap();
        std::fs::write(&idx, &bytes[..bytes.len() - 5]).unwrap();
        assert!(matches!(DocStore::open(dir.path()), Err(Error::Corrupt(_))));
    }
}
