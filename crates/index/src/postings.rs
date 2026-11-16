//! Posting list encoding: delta + VByte, in blocks of 128 with skip pointers.
//!
//! One term's postings, `df` entries:
//!
//! ```text
//! [skip table]  only when there is more than one block:
//!               per block: u32 last_doc, u32 end offset (relative to data start)
//! [data]        per block of up to 128 postings:
//!               doc gaps (VByte) × n, then term frequencies (VByte) × n
//! ```
//!
//! Gaps restart at each block boundary from the previous block's last doc
//! (from 0 for the first block), so any block can be decoded on its own.
//! [`BlockCursor::advance`] binary-searches the skip table and decodes only the
//! block that can hold the target.

use crate::{DocId, NO_MORE_DOCS, PostingCursor, u32_at, vbyte};

pub const BLOCK_LEN: usize = 128;
const SKIP_ENTRY_LEN: usize = 8;

fn skip_table_len(df: usize) -> usize {
    let nblocks = df.div_ceil(BLOCK_LEN);
    if nblocks > 1 { nblocks * SKIP_ENTRY_LEN } else { 0 }
}

/// Appends the encoded list to `out`. `docs` must be strictly increasing and
/// every tf at least 1; the builder guarantees both.
pub fn encode(docs: &[DocId], tfs: &[u32], out: &mut Vec<u8>) {
    debug_assert_eq!(docs.len(), tfs.len());
    debug_assert!(docs.windows(2).all(|w| w[0] < w[1]));

    let nblocks = docs.len().div_ceil(BLOCK_LEN);
    let skip_start = out.len();
    out.resize(skip_start + skip_table_len(docs.len()), 0);
    let data_start = out.len();

    let mut prev: DocId = 0;
    for (b, (dchunk, tchunk)) in docs.chunks(BLOCK_LEN).zip(tfs.chunks(BLOCK_LEN)).enumerate() {
        for &d in dchunk {
            vbyte::encode(d - prev, out);
            prev = d;
        }
        for &t in tchunk {
            vbyte::encode(t, out);
        }
        if nblocks > 1 {
            let end = (out.len() - data_start) as u32;
            let at = skip_start + b * SKIP_ENTRY_LEN;
            out[at..at + 4].copy_from_slice(&prev.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&end.to_le_bytes());
        }
    }
}

/// Fully decodes and validates a list: sorted doc IDs, tf ≥ 1, skip table
/// consistent with the data, no trailing bytes. Used by the checker.
pub fn decode_all(bytes: &[u8], df: u32) -> Result<(Vec<DocId>, Vec<u32>), String> {
    let n = df as usize;
    let nblocks = n.div_ceil(BLOCK_LEN);
    let skip_len = skip_table_len(n);
    if bytes.len() < skip_len {
        return Err(format!("{} bytes cannot hold a {nblocks}-block skip table", bytes.len()));
    }
    let (skips, data) = bytes.split_at(skip_len);

    let mut docs = Vec::with_capacity(n);
    let mut tfs = Vec::with_capacity(n);
    let mut pos = 0usize;
    let mut prev: DocId = 0;
    for b in 0..nblocks {
        let cnt = BLOCK_LEN.min(n - b * BLOCK_LEN);
        for k in 0..cnt {
            let gap = vbyte::decode(data, &mut pos).ok_or_else(|| format!("block {b}: truncated doc gap"))?;
            let first = b == 0 && k == 0;
            if !first && gap == 0 {
                return Err(format!("block {b}: zero gap (duplicate doc {prev})"));
            }
            prev = prev.checked_add(gap).ok_or_else(|| format!("block {b}: doc ID overflow"))?;
            if prev == NO_MORE_DOCS {
                return Err(format!("block {b}: reserved doc ID {NO_MORE_DOCS}"));
            }
            docs.push(prev);
        }
        for _ in 0..cnt {
            let tf = vbyte::decode(data, &mut pos).ok_or_else(|| format!("block {b}: truncated tf"))?;
            if tf == 0 {
                return Err(format!("block {b}: tf of 0"));
            }
            tfs.push(tf);
        }
        if nblocks > 1 {
            let last = u32_at(skips, b * SKIP_ENTRY_LEN);
            let end = u32_at(skips, b * SKIP_ENTRY_LEN + 4) as usize;
            if last != prev || end != pos {
                return Err(format!(
                    "block {b}: skip entry (last {last}, end {end}) disagrees with data (last {prev}, end {pos})"
                ));
            }
        }
    }
    if pos != data.len() {
        return Err(format!("{} trailing bytes after {n} postings", data.len() - pos));
    }
    Ok((docs, tfs))
}

/// Lazily decodes a list one 128-posting block at a time.
///
/// Corrupt data never panics: the cursor just ends early. Use the checker to
/// find corruption; this type is for the query hot path.
pub struct BlockCursor<'a> {
    skips: &'a [u8],
    data: &'a [u8],
    df: usize,
    nblocks: usize,
    block: usize,
    docs: [DocId; BLOCK_LEN],
    tfs: [u32; BLOCK_LEN],
    len: usize,
    i: usize,
    done: bool,
    max_tf: u32,
    min_doc_len: u32,
}

impl<'a> BlockCursor<'a> {
    pub fn new(bytes: &'a [u8], df: u32) -> Self {
        let df = df as usize;
        let skip_len = skip_table_len(df);
        let mut c = Self {
            skips: &[],
            data: &[],
            df,
            nblocks: df.div_ceil(BLOCK_LEN),
            block: 0,
            docs: [0; BLOCK_LEN],
            tfs: [0; BLOCK_LEN],
            len: 0,
            i: 0,
            done: true,
            // Bounds that are always safe; `with_bounds` tightens them.
            max_tf: u32::MAX,
            min_doc_len: 0,
        };
        if df > 0 && bytes.len() >= skip_len {
            (c.skips, c.data) = bytes.split_at(skip_len);
            c.done = false;
            c.load_block(0);
        }
        c
    }

    /// Sets the score-bound inputs stored in the dictionary.
    pub fn with_bounds(mut self, max_tf: u32, min_doc_len: u32) -> Self {
        self.max_tf = max_tf;
        self.min_doc_len = min_doc_len;
        self
    }

    fn skip_last(&self, b: usize) -> DocId {
        u32_at(self.skips, b * SKIP_ENTRY_LEN)
    }

    fn skip_end(&self, b: usize) -> usize {
        u32_at(self.skips, b * SKIP_ENTRY_LEN + 4) as usize
    }

    fn load_block(&mut self, b: usize) {
        if b >= self.nblocks {
            self.done = true;
            return;
        }
        let (mut pos, mut prev) = if b == 0 { (0, 0) } else { (self.skip_end(b - 1), self.skip_last(b - 1)) };
        let cnt = BLOCK_LEN.min(self.df - b * BLOCK_LEN);
        for k in 0..cnt {
            match vbyte::decode(self.data, &mut pos) {
                Some(gap) => {
                    prev = prev.wrapping_add(gap);
                    self.docs[k] = prev;
                }
                None => {
                    self.done = true;
                    return;
                }
            }
        }
        for k in 0..cnt {
            match vbyte::decode(self.data, &mut pos) {
                Some(tf) => self.tfs[k] = tf,
                None => {
                    self.done = true;
                    return;
                }
            }
        }
        self.block = b;
        self.len = cnt;
        self.i = 0;
    }
}

impl PostingCursor for BlockCursor<'_> {
    #[inline]
    fn doc(&self) -> DocId {
        if self.done { NO_MORE_DOCS } else { self.docs[self.i] }
    }

    #[inline]
    fn tf(&self) -> u32 {
        if self.done { 0 } else { self.tfs[self.i] }
    }

    #[inline]
    fn next(&mut self) {
        if self.done {
            return;
        }
        self.i += 1;
        if self.i >= self.len {
            self.load_block(self.block + 1);
        }
    }

    fn advance(&mut self, target: DocId) {
        if self.done || self.docs[self.i] >= target {
            return;
        }
        if self.nblocks > 1 && self.skip_last(self.block) < target {
            // First later block whose last doc reaches the target.
            let (mut lo, mut hi) = (self.block + 1, self.nblocks);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.skip_last(mid) < target { lo = mid + 1 } else { hi = mid }
            }
            self.load_block(lo);
        }
        while !self.done && self.docs[self.i] < target {
            self.next();
        }
    }

    fn df(&self) -> u32 {
        self.df as u32
    }

    fn max_tf(&self) -> u32 {
        self.max_tf
    }

    fn min_doc_len(&self) -> u32 {
        self.min_doc_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn build(docs: &[DocId]) -> (Vec<u8>, Vec<u32>) {
        let tfs: Vec<u32> = docs.iter().map(|d| d % 5 + 1).collect();
        let mut out = Vec::new();
        encode(docs, &tfs, &mut out);
        (out, tfs)
    }

    fn walk(mut c: BlockCursor) -> Vec<(DocId, u32)> {
        let mut v = Vec::new();
        while c.doc() != NO_MORE_DOCS {
            v.push((c.doc(), c.tf()));
            c.next();
        }
        v
    }

    /// Strictly increasing doc IDs with a mix of small and large gaps.
    fn sorted_docs(max_len: usize) -> impl Strategy<Value = Vec<DocId>> {
        prop::collection::vec(prop_oneof![1u32..4, 1u32..300, 1u32..100_000], 1..max_len).prop_map(|gaps| {
            let mut d = 0u32;
            gaps.iter()
                .enumerate()
                .map(|(i, g)| {
                    d = if i == 0 { g - 1 } else { d + g };
                    d
                })
                .collect()
        })
    }

    #[test]
    fn spec_example_gaps() {
        let (bytes, _) = build(&[4, 19, 22, 40]);
        // Single block: no skip table, gaps 4 15 3 18, then tfs.
        assert_eq!(&bytes[..4], &[4, 15, 3, 18]);
    }

    #[test]
    fn block_boundaries() {
        for n in [1, 127, 128, 129, 256, 257, 1000] {
            let docs: Vec<DocId> = (0..n).map(|i| i * 3).collect();
            let (bytes, tfs) = build(&docs);
            assert_eq!(decode_all(&bytes, n).unwrap(), (docs.clone(), tfs.clone()), "n = {n}");
            let walked = walk(BlockCursor::new(&bytes, n));
            assert_eq!(walked.len(), n as usize);
        }
    }

    #[test]
    fn doc_zero_first_is_fine() {
        let (bytes, tfs) = build(&[0, 1, 2]);
        assert_eq!(decode_all(&bytes, 3).unwrap(), (vec![0, 1, 2], tfs));
    }

    #[test]
    fn corruption_is_an_error_not_a_panic() {
        let docs: Vec<DocId> = (0..300).map(|i| i * 7).collect();
        let (bytes, _) = build(&docs);
        for cut in [0, 1, 10, 17, bytes.len() - 1] {
            assert!(decode_all(&bytes[..cut], 300).is_err(), "cut at {cut}");
            let _ = walk(BlockCursor::new(&bytes[..cut], 300));
        }
        let mut flipped = bytes.clone();
        flipped[3] ^= 0xFF; // inside the skip table
        assert!(decode_all(&flipped, 300).is_err());
        let _ = walk(BlockCursor::new(&flipped, 300));
        assert!(decode_all(&bytes, 299).is_err(), "wrong df");
    }

    proptest! {
        #[test]
        fn round_trip(docs in sorted_docs(700)) {
            let (bytes, tfs) = build(&docs);
            let n = docs.len() as u32;
            prop_assert_eq!(decode_all(&bytes, n).unwrap(), (docs.clone(), tfs.clone()));
            let expected: Vec<(DocId, u32)> = docs.iter().copied().zip(tfs.iter().copied()).collect();
            prop_assert_eq!(walk(BlockCursor::new(&bytes, n)), expected);
        }

        /// advance(target) lands on the first doc ≥ target, as a linear scan would.
        #[test]
        fn advance_matches_linear_scan(
            docs in sorted_docs(700),
            targets in prop::collection::vec(any::<u32>(), 1..20),
        ) {
            let (bytes, _) = build(&docs);
            let mut targets = targets;
            let max = *docs.last().unwrap();
            for t in targets.iter_mut() { *t %= max.saturating_add(10).max(1); }
            targets.sort_unstable();

            let mut c = BlockCursor::new(&bytes, docs.len() as u32);
            for &t in &targets {
                c.advance(t);
                // Targets are sorted, so the cursor never needs to move back.
                let want = docs.iter().copied().find(|&d| d >= t).unwrap_or(NO_MORE_DOCS);
                prop_assert_eq!(c.doc(), want);
            }
        }

        #[test]
        fn garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..400), df in 0u32..600) {
            let _ = decode_all(&bytes, df);
            let mut c = BlockCursor::new(&bytes, df);
            c.advance(1000);
            let _ = walk(c);
        }
    }
}
