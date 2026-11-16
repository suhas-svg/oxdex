//! Index checker: decodes every posting list and cross-checks the totals.
//!
//! Confirms, for every term: the dictionary is strictly sorted, the term is
//! UTF-8, the list decodes cleanly with sorted doc IDs below `num_docs`, tf ≥ 1,
//! and the skip table agrees with the data. Then, summed over all terms, each
//! document's term frequencies must equal its length in `doclens.bin`, and the
//! grand total must equal `meta.total_tokens`.

use crate::{DiskIndex, SearchIndex, postings};

const MAX_REPORTED: usize = 20;

#[derive(Debug, Default)]
pub struct CheckReport {
    pub num_terms: u64,
    pub num_postings: u64,
    pub max_df: u32,
    pub max_df_term: String,
    pub total_errors: u64,
    /// The first few error messages.
    pub errors: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.total_errors == 0
    }

    fn error(&mut self, msg: String) {
        self.total_errors += 1;
        if self.errors.len() < MAX_REPORTED {
            self.errors.push(msg);
        }
    }
}

/// Runs every check. `progress` is called with the number of terms done.
pub fn check(index: &DiskIndex, mut progress: impl FnMut(u64)) -> CheckReport {
    let mut r = CheckReport::default();
    let num_docs = index.num_docs();
    let mut per_doc = vec![0u64; num_docs as usize];
    let mut prev: Option<&[u8]> = None;

    for i in 0..index.num_terms() {
        let e = index.term_entry(i);
        let shown = String::from_utf8_lossy(e.term).into_owned();
        if std::str::from_utf8(e.term).is_err() {
            r.error(format!("term #{i} is not UTF-8: {shown:?}"));
        }
        if e.term.is_empty() {
            r.error(format!("term #{i} is empty"));
        }
        if let Some(p) = prev
            && p >= e.term
        {
            r.error(format!("dictionary not strictly sorted at term #{i} {shown:?}"));
        }
        prev = Some(e.term);
        if e.df == 0 {
            r.error(format!("term {shown:?} has df 0"));
            continue;
        }

        match postings::decode_all(e.postings, e.df) {
            Ok((docs, tfs)) => {
                if let Some(&last) = docs.last()
                    && last >= num_docs
                {
                    r.error(format!("term {shown:?} points at doc {last}, but there are {num_docs} docs"));
                    continue;
                }
                let max_tf = tfs.iter().copied().max().unwrap_or(0);
                let min_dl = docs.iter().map(|&d| index.doc_len(d)).min().unwrap_or(0);
                if (max_tf, min_dl) != (e.max_tf, e.min_doc_len) {
                    r.error(format!(
                        "term {shown:?}: dictionary says max_tf {} / min_doc_len {}, list has {max_tf} / {min_dl}",
                        e.max_tf, e.min_doc_len
                    ));
                }
                for (d, tf) in docs.iter().zip(&tfs) {
                    per_doc[*d as usize] += u64::from(*tf);
                }
                r.num_postings += u64::from(e.df);
                if e.df > r.max_df {
                    r.max_df = e.df;
                    r.max_df_term = shown;
                }
            }
            Err(msg) => r.error(format!("term {shown:?}: {msg}")),
        }

        r.num_terms += 1;
        if r.num_terms % 1_000_000 == 0 {
            progress(r.num_terms);
        }
    }

    let mut total: u64 = 0;
    for (d, &sum) in per_doc.iter().enumerate() {
        total += sum;
        let len = u64::from(index.doc_len(d as u32));
        if sum != len {
            r.error(format!("doc {d}: postings add up to {sum} tokens, doclens.bin says {len}"));
        }
    }
    if total != index.meta().total_tokens {
        r.error(format!("postings hold {total} tokens, meta.json says {}", index.meta().total_tokens));
    }
    if r.num_terms != index.meta().num_terms {
        r.error(format!("{} terms checked, meta.json says {}", r.num_terms, index.meta().num_terms));
    }
    r
}

#[cfg(test)]
mod tests {
    use crate::{DiskIndex, IndexBuilder, InMemoryIndex, PostingCursor, SearchIndex, count_terms};

    /// Deterministic pseudo-random corpus with a skewed vocabulary, so a few
    /// terms are in most docs (many blocks) and most are rare (one block).
    fn corpus(n: usize) -> Vec<(String, String)> {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        (0..n)
            .map(|i| {
                let len = 5 + (rnd() % 60) as usize;
                let words: Vec<String> = (0..len)
                    .map(|_| {
                        let r = rnd() % 1000;
                        // Zipf-ish: low ids are common.
                        let id = (r * r) / 1000;
                        format!("w{id}")
                    })
                    .collect();
                (format!("doc{i}"), words.join(" "))
            })
            .collect()
    }

    fn build_both(docs: &[(String, String)], budget: usize, dir: &std::path::Path) -> (InMemoryIndex, crate::BuildStats) {
        let mut mem = InMemoryIndex::new();
        let mut b = IndexBuilder::create(dir, budget).unwrap();
        for (t, x) in docs {
            mem.add(t, x);
            let (terms, len) = count_terms(t, x);
            b.add_doc(&terms, len).unwrap();
        }
        (mem, b.finish().unwrap())
    }

    #[test]
    fn disk_matches_memory_across_many_segments() {
        let dir = tempfile::tempdir().unwrap();
        let docs = corpus(3000);
        let (mem, stats) = build_both(&docs, 16 * 1024, dir.path());
        assert!(stats.num_segments > 5, "tiny budget should force many segments, got {}", stats.num_segments);
        assert!(!dir.path().join("segments").exists(), "segments are deleted after the merge");

        let disk = DiskIndex::open(dir.path()).unwrap();
        assert_eq!(disk.num_docs(), mem.num_docs());
        assert_eq!(disk.num_terms(), mem.num_terms());
        assert_eq!(disk.meta().total_tokens, mem.total_tokens());
        assert!((disk.avgdl() - mem.avgdl()).abs() < 1e-9);

        for term in mem.terms() {
            let mut c = disk.cursor(term).unwrap_or_else(|| panic!("{term} missing on disk"));
            let mut got = Vec::new();
            while c.doc() != crate::NO_MORE_DOCS {
                got.push(crate::Posting { doc: c.doc(), tf: c.tf() });
                c.next();
            }
            assert_eq!(got, mem.postings(term), "term {term}");
        }
        for d in 0..mem.num_docs() {
            assert_eq!(SearchIndex::doc_len(&disk, d), mem.doc_len(d));
        }
        assert!(disk.cursor("not-a-term").is_none());

        let report = super::check(&disk, |_| {});
        assert!(report.ok(), "{:?}", report.errors);
        assert_eq!(report.num_terms as usize, mem.num_terms());
        assert!(report.max_df > 128, "some lists span several blocks");
    }

    #[test]
    fn one_segment_and_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let (_, stats) = build_both(&corpus(50), usize::MAX, dir.path());
        assert_eq!(stats.num_segments, 1);
        assert!(super::check(&DiskIndex::open(dir.path()).unwrap(), |_| {}).ok());

        let empty = tempfile::tempdir().unwrap();
        let stats = IndexBuilder::create(empty.path(), 1024).unwrap().finish().unwrap();
        assert_eq!((stats.num_docs, stats.num_terms), (0, 0));
        let disk = DiskIndex::open(empty.path()).unwrap();
        assert_eq!(disk.num_terms(), 0);
        assert!(disk.cursor("x").is_none());
        assert!(super::check(&disk, |_| {}).ok());
    }

    #[test]
    fn checker_finds_corruption() {
        let dir = tempfile::tempdir().unwrap();
        build_both(&corpus(800), 1 << 20, dir.path());
        let post = dir.path().join("postings.bin");
        let mut bytes = std::fs::read(&post).unwrap();
        for i in (100..bytes.len()).step_by(997) {
            bytes[i] ^= 0x5A;
        }
        std::fs::write(&post, &bytes).unwrap();

        let disk = DiskIndex::open(dir.path()).unwrap();
        let report = super::check(&disk, |_| {});
        assert!(!report.ok());
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn refuses_unfinished_and_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        build_both(&corpus(20), 1 << 20, dir.path());

        let dict = dir.path().join("terms.dict");
        let mut bytes = std::fs::read(&dict).unwrap();
        bytes[8] = 7;
        std::fs::write(&dict, &bytes).unwrap();
        assert!(matches!(DiskIndex::open(dir.path()), Err(crate::Error::BadVersion { found: 7, .. })));

        std::fs::remove_file(dir.path().join("meta.json")).unwrap();
        assert!(matches!(DiskIndex::open(dir.path()), Err(crate::Error::Incomplete(_))));
    }
}
