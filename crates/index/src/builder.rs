//! Memory-bounded index build: in-memory segments, flushed by size, then one
//! k-way merge into the final files.
//!
//! While building, each term's postings are kept already compressed: an
//! interleaved VByte stream of `(doc gap, tf)` pairs. That keeps a posting at
//! about 2–3 bytes, so a 1 GB budget holds tens of millions of them. When the
//! budget is reached the terms are sorted and written as a segment file.
//!
//! Segment file (temporary, in `<out>/segments/`): 24-byte header (`OXDXSEGM`,
//! u32 version, u32 reserved, u64 num_terms), then per term in byte order:
//! `u16 len + term, u32 df, u32 nbytes + pair stream`. The first gap of each
//! term is relative to 0, so each segment's lists decode on their own.
//!
//! The merge reads all segments side by side, like the merge step of merge
//! sort, and writes each term once. Segments cover increasing doc ID ranges,
//! so a term's lists are simply concatenated in segment order.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::{DocId, Error, FORMAT_VERSION, IndexMeta, NO_MORE_DOCS, Result, io_err, postings, vbyte};

pub(crate) const DICT_MAGIC: &[u8; 8] = b"OXDXDICT";
pub(crate) const POST_MAGIC: &[u8; 8] = b"OXDXPOST";
pub(crate) const DLEN_MAGIC: &[u8; 8] = b"OXDXDLEN";
const SEG_MAGIC: &[u8; 8] = b"OXDXSEGM";
pub(crate) const DICT_HEADER_LEN: usize = 32;
pub(crate) const DICT_ENTRY_LEN: usize = 32;
pub(crate) const SMALL_HEADER_LEN: usize = 16;
const SEG_HEADER_LEN: usize = 24;

/// Rough per-term cost of a HashMap entry beyond the key and posting bytes
/// (table slot, Box<str> and Vec headers, allocator slack).
const TERM_OVERHEAD: usize = 80;

struct TermBuf {
    last_doc: DocId,
    df: u32,
    pairs: Vec<u8>,
}

/// Final numbers from [`IndexBuilder::finish`].
#[derive(Debug, Clone, Copy)]
pub struct BuildStats {
    pub num_docs: u32,
    pub num_terms: u64,
    pub total_tokens: u64,
    pub num_segments: usize,
    pub postings_bytes: u64,
    pub dict_bytes: u64,
    pub doclens_bytes: u64,
    /// Largest estimated size of the in-memory segment buffer.
    pub peak_buffer_bytes: usize,
    /// Time spent writing segment files (sort + write).
    pub flush_secs: f64,
    /// Time spent in the final k-way merge.
    pub merge_secs: f64,
}

pub struct IndexBuilder {
    dir: PathBuf,
    seg_dir: PathBuf,
    terms: HashMap<Box<str>, TermBuf>,
    mem: usize,
    budget: usize,
    peak_mem: usize,
    segments: Vec<PathBuf>,
    doclens: BufWriter<File>,
    /// Doc lengths again in memory, for the per-term `min_doc_len` at merge
    /// time (4 bytes per doc: 28 MB for English Wikipedia).
    lens: Vec<u32>,
    next_doc: u64,
    total_tokens: u64,
    started: Instant,
    flush_secs: f64,
}

impl IndexBuilder {
    /// Starts a build in `dir`, flushing a segment whenever the in-memory
    /// buffer passes `mem_budget_bytes`. Any previous index files in `dir`
    /// are replaced.
    pub fn create(dir: impl AsRef<Path>, mem_budget_bytes: usize) -> Result<Self> {
        let dir = dir.as_ref().to_owned();
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        // Remove meta.json first: until a new one is written, the directory
        // must not look like a finished index.
        let meta = dir.join("meta.json");
        if meta.exists() {
            fs::remove_file(&meta).map_err(io_err(&meta))?;
        }
        let seg_dir = dir.join("segments");
        if seg_dir.exists() {
            fs::remove_dir_all(&seg_dir).map_err(io_err(&seg_dir))?;
        }
        fs::create_dir_all(&seg_dir).map_err(io_err(&seg_dir))?;

        let dl_path = dir.join("doclens.bin");
        let mut doclens = BufWriter::new(File::create(&dl_path).map_err(io_err(&dl_path))?);
        write_small_header(&mut doclens, DLEN_MAGIC).map_err(io_err(&dl_path))?;

        Ok(Self {
            dir,
            seg_dir,
            terms: HashMap::new(),
            mem: 0,
            budget: mem_budget_bytes.max(1),
            peak_mem: 0,
            segments: Vec::new(),
            doclens,
            lens: Vec::new(),
            next_doc: 0,
            total_tokens: 0,
            started: Instant::now(),
            flush_secs: 0.0,
        })
    }

    /// Adds the next document. `terms` must list each term once with its
    /// count (as [`crate::count_terms`] returns); `doc_len` is its token count.
    pub fn add_doc(&mut self, terms: &[(String, u32)], doc_len: u32) -> Result<DocId> {
        let doc = u32::try_from(self.next_doc)
            .ok()
            .filter(|&d| d != NO_MORE_DOCS)
            .ok_or(Error::TooManyDocs)?;
        self.next_doc += 1;

        let dl_path = self.dir.join("doclens.bin");
        self.doclens.write_all(&doc_len.to_le_bytes()).map_err(io_err(&dl_path))?;
        self.total_tokens += u64::from(doc_len);
        self.lens.push(doc_len);

        for (term, tf) in terms {
            if *tf == 0 {
                continue;
            }
            match self.terms.get_mut(term.as_str()) {
                Some(buf) => {
                    debug_assert!(buf.last_doc < doc, "term {term:?} listed twice for doc {doc}");
                    let cap = buf.pairs.capacity();
                    vbyte::encode(doc - buf.last_doc, &mut buf.pairs);
                    vbyte::encode(*tf, &mut buf.pairs);
                    buf.last_doc = doc;
                    buf.df += 1;
                    self.mem += buf.pairs.capacity() - cap;
                }
                None => {
                    let mut pairs = Vec::with_capacity(8);
                    vbyte::encode(doc, &mut pairs); // first gap is relative to 0
                    vbyte::encode(*tf, &mut pairs);
                    self.mem += term.len() + TERM_OVERHEAD + pairs.capacity();
                    self.terms.insert(term.as_str().into(), TermBuf { last_doc: doc, df: 1, pairs });
                }
            }
        }

        self.peak_mem = self.peak_mem.max(self.mem);
        if self.mem >= self.budget {
            self.flush_segment()?;
        }
        Ok(doc)
    }

    pub fn num_docs(&self) -> u64 {
        self.next_doc
    }

    pub fn num_segments(&self) -> usize {
        self.segments.len()
    }

    /// Estimated bytes held by the in-memory segment buffer right now.
    pub fn buffer_bytes(&self) -> usize {
        self.mem
    }

    fn flush_segment(&mut self) -> Result<()> {
        if self.terms.is_empty() {
            return Ok(());
        }
        let t0 = Instant::now();
        let mut terms: Vec<(Box<str>, TermBuf)> = std::mem::take(&mut self.terms).into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        let path = self.seg_dir.join(format!("seg-{:05}.bin", self.segments.len()));
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&path).map_err(io_err(&path))?);
        let write = |w: &mut BufWriter<File>| -> std::io::Result<()> {
            w.write_all(SEG_MAGIC)?;
            w.write_all(&FORMAT_VERSION.to_le_bytes())?;
            w.write_all(&0u32.to_le_bytes())?;
            w.write_all(&(terms.len() as u64).to_le_bytes())?;
            for (term, buf) in &terms {
                w.write_all(&(term.len() as u16).to_le_bytes())?;
                w.write_all(term.as_bytes())?;
                w.write_all(&buf.df.to_le_bytes())?;
                w.write_all(&(buf.pairs.len() as u32).to_le_bytes())?;
                w.write_all(&buf.pairs)?;
            }
            w.flush()
        };
        write(&mut w).map_err(io_err(&path))?;

        self.segments.push(path);
        self.mem = 0;
        self.flush_secs += t0.elapsed().as_secs_f64();
        Ok(())
    }

    /// Flushes the last segment, merges all segments into `terms.dict` and
    /// `postings.bin`, deletes the segments, and writes `meta.json` last.
    pub fn finish(mut self) -> Result<BuildStats> {
        self.flush_segment()?;
        let dl_path = self.dir.join("doclens.bin");
        self.doclens.flush().map_err(io_err(&dl_path))?;

        let num_docs = u32::try_from(self.next_doc).map_err(|_| Error::TooManyDocs)?;
        let t0 = Instant::now();
        let (num_terms, postings_bytes, dict_bytes) = merge(&self.segments, &self.dir, &self.lens)?;
        fs::remove_dir_all(&self.seg_dir).map_err(io_err(&self.seg_dir))?;
        let merge_secs = t0.elapsed().as_secs_f64();

        let doclens_bytes = (SMALL_HEADER_LEN + 4 * num_docs as usize) as u64;
        let stats = BuildStats {
            num_docs,
            num_terms,
            total_tokens: self.total_tokens,
            num_segments: self.segments.len(),
            postings_bytes,
            dict_bytes,
            doclens_bytes,
            peak_buffer_bytes: self.peak_mem,
            flush_secs: self.flush_secs,
            merge_secs,
        };
        let meta = IndexMeta {
            format_version: FORMAT_VERSION,
            num_docs,
            num_terms,
            total_tokens: self.total_tokens,
            avgdl: if num_docs == 0 { 0.0 } else { self.total_tokens as f64 / f64::from(num_docs) },
            num_segments: self.segments.len(),
            postings_bytes,
            dict_bytes,
            doclens_bytes,
            mem_budget_bytes: self.budget as u64,
            build_secs: self.started.elapsed().as_secs_f64(),
            built_unix_secs: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        };
        let meta_path = self.dir.join("meta.json");
        fs::write(&meta_path, serde_json::to_string_pretty(&meta)?).map_err(io_err(&meta_path))?;
        Ok(stats)
    }
}

pub(crate) fn write_small_header(w: &mut impl Write, magic: &[u8; 8]) -> std::io::Result<()> {
    w.write_all(magic)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Segment reading and the k-way merge
// ---------------------------------------------------------------------------

struct SegEntry {
    term: Vec<u8>,
    df: u32,
    pairs: Vec<u8>,
}

struct SegmentReader {
    r: BufReader<File>,
    remaining: u64,
    path: PathBuf,
}

impl SegmentReader {
    fn open(path: &Path) -> Result<Self> {
        let mut r = BufReader::with_capacity(1 << 20, File::open(path).map_err(io_err(path))?);
        let mut h = [0u8; SEG_HEADER_LEN];
        r.read_exact(&mut h).map_err(io_err(path))?;
        if &h[..8] != SEG_MAGIC {
            return Err(Error::BadMagic { path: path.to_owned() });
        }
        Ok(Self {
            r,
            remaining: crate::u64_at(&h, 16),
            path: path.to_owned(),
        })
    }

    fn next(&mut self) -> Result<Option<SegEntry>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let path = &self.path;
        let mut u16b = [0u8; 2];
        let mut u32b = [0u8; 4];
        self.r.read_exact(&mut u16b).map_err(io_err(path))?;
        let mut term = vec![0u8; u16::from_le_bytes(u16b) as usize];
        self.r.read_exact(&mut term).map_err(io_err(path))?;
        self.r.read_exact(&mut u32b).map_err(io_err(path))?;
        let df = u32::from_le_bytes(u32b);
        self.r.read_exact(&mut u32b).map_err(io_err(path))?;
        let mut pairs = vec![0u8; u32::from_le_bytes(u32b) as usize];
        self.r.read_exact(&mut pairs).map_err(io_err(path))?;
        Ok(Some(SegEntry { term, df, pairs }))
    }
}

/// Appends one segment's pair stream to `docs`/`tfs`, checking that doc IDs
/// keep increasing across segments.
fn decode_pairs(pairs: &[u8], df: u32, docs: &mut Vec<DocId>, tfs: &mut Vec<u32>) -> Result<()> {
    let corrupt = |what: &str| Error::Corrupt(format!("segment postings: {what}"));
    let mut pos = 0;
    let mut cur: DocId = 0;
    for k in 0..df {
        let gap = vbyte::decode(pairs, &mut pos).ok_or_else(|| corrupt("truncated gap"))?;
        let tf = vbyte::decode(pairs, &mut pos).ok_or_else(|| corrupt("truncated tf"))?;
        cur = if k == 0 {
            gap
        } else if gap == 0 {
            return Err(corrupt("zero gap"));
        } else {
            cur.checked_add(gap).ok_or_else(|| corrupt("doc overflow"))?
        };
        if docs.last().is_some_and(|&last| cur <= last) {
            return Err(corrupt("doc IDs not increasing across segments"));
        }
        docs.push(cur);
        tfs.push(tf);
    }
    if pos != pairs.len() {
        return Err(corrupt("trailing bytes"));
    }
    Ok(())
}

/// Merges segments into `terms.dict` + `postings.bin`. Returns
/// `(num_terms, postings_bytes, dict_bytes)`.
fn merge(segments: &[PathBuf], dir: &Path, lens: &[u32]) -> Result<(u64, u64, u64)> {
    let mut readers: Vec<SegmentReader> = segments.iter().map(|p| SegmentReader::open(p)).collect::<Result<_>>()?;
    let mut current: Vec<Option<SegEntry>> = Vec::with_capacity(readers.len());
    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        let e = r.next()?;
        if let Some(e) = &e {
            heap.push(Reverse((e.term.clone(), i)));
        }
        current.push(e);
    }

    let post_path = dir.join("postings.bin");
    let mut post = BufWriter::with_capacity(4 << 20, File::create(&post_path).map_err(io_err(&post_path))?);
    write_small_header(&mut post, POST_MAGIC).map_err(io_err(&post_path))?;
    let mut post_pos = SMALL_HEADER_LEN as u64;

    let dict_path = dir.join("terms.dict");
    let mut dict = BufWriter::with_capacity(4 << 20, File::create(&dict_path).map_err(io_err(&dict_path))?);
    dict.write_all(&[0u8; DICT_HEADER_LEN]).map_err(io_err(&dict_path))?; // rewritten at the end

    let heap_path = dir.join("terms.heap.tmp");
    let mut strs = BufWriter::with_capacity(1 << 20, File::create(&heap_path).map_err(io_err(&heap_path))?);
    let mut str_pos: u64 = 0;

    let mut num_terms: u64 = 0;
    let mut docs: Vec<DocId> = Vec::new();
    let mut tfs: Vec<u32> = Vec::new();
    let mut encoded: Vec<u8> = Vec::new();
    let mut group: Vec<usize> = Vec::new();

    while let Some(Reverse((term, first))) = heap.pop() {
        group.clear();
        group.push(first);
        while heap.peek().is_some_and(|Reverse((t, _))| *t == term) {
            if let Some(Reverse((_, j))) = heap.pop() {
                group.push(j);
            }
        }
        group.sort_unstable(); // segment order = doc ID order

        docs.clear();
        tfs.clear();
        for &j in &group {
            let e = current[j]
                .take()
                .ok_or_else(|| Error::Corrupt("merge lost track of a segment".into()))?;
            decode_pairs(&e.pairs, e.df, &mut docs, &mut tfs)?;
            let next = readers[j].next()?;
            if let Some(n) = &next {
                if n.term <= e.term {
                    return Err(Error::Corrupt(format!("{} is not sorted", segments[j].display())));
                }
                heap.push(Reverse((n.term.clone(), j)));
            }
            current[j] = next;
        }

        encoded.clear();
        postings::encode(&docs, &tfs, &mut encoded);
        post.write_all(&encoded).map_err(io_err(&post_path))?;

        let mut entry = [0u8; DICT_ENTRY_LEN];
        entry[0..8].copy_from_slice(&str_pos.to_le_bytes());
        entry[8..16].copy_from_slice(&post_pos.to_le_bytes());
        entry[16..20].copy_from_slice(&(docs.len() as u32).to_le_bytes());
        entry[20..24].copy_from_slice(&(term.len() as u32).to_le_bytes());
        let max_tf = tfs.iter().copied().max().unwrap_or(0);
        let min_dl = docs.iter().map(|&d| lens.get(d as usize).copied().unwrap_or(0)).min().unwrap_or(0);
        entry[24..28].copy_from_slice(&max_tf.to_le_bytes());
        entry[28..32].copy_from_slice(&min_dl.to_le_bytes());
        dict.write_all(&entry).map_err(io_err(&dict_path))?;
        strs.write_all(&term).map_err(io_err(&heap_path))?;

        str_pos += term.len() as u64;
        post_pos += encoded.len() as u64;
        num_terms += 1;
    }

    post.flush().map_err(io_err(&post_path))?;
    strs.flush().map_err(io_err(&heap_path))?;
    drop(strs);

    // Append the string heap, then fill in the header.
    let mut heap_file = File::open(&heap_path).map_err(io_err(&heap_path))?;
    std::io::copy(&mut heap_file, &mut dict).map_err(io_err(&dict_path))?;
    drop(heap_file);
    fs::remove_file(&heap_path).map_err(io_err(&heap_path))?;

    let mut dict = dict.into_inner().map_err(|e| Error::Io {
        path: dict_path.clone(),
        source: e.into_error(),
    })?;
    let heap_offset = DICT_HEADER_LEN as u64 + num_terms * DICT_ENTRY_LEN as u64;
    let mut header = Vec::with_capacity(DICT_HEADER_LEN);
    header.extend_from_slice(DICT_MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&num_terms.to_le_bytes());
    header.extend_from_slice(&heap_offset.to_le_bytes());
    dict.seek(SeekFrom::Start(0))
        .and_then(|_| dict.write_all(&header))
        .and_then(|_| dict.sync_all())
        .map_err(io_err(&dict_path))?;

    Ok((num_terms, post_pos, heap_offset + str_pos))
}
