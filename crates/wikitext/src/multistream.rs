//! The multistream dump is many small bz2 streams of ~100 pages each, glued
//! together. Its index file lists `offset:page_id:title` for every page, where
//! `offset` is the byte position of the stream holding that page. Each stream
//! can be decompressed on its own, which is what makes parallel ingest possible.
//!
//! Stream layout: bytes `0..first_offset` hold the `<mediawiki><siteinfo>`
//! header, then one stream per distinct offset, the last running to end of file.

use std::io::{BufRead, BufReader, Read};

use bzip2::read::MultiBzDecoder;

use crate::{Error, Result};

/// Byte range of one bz2 stream inside the dump file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRange {
    pub start: u64,
    pub end: u64,
}

/// Reads a (bz2-compressed) multistream index and returns the distinct stream
/// offsets in increasing order.
pub fn parse_index(compressed: impl Read) -> Result<Vec<u64>> {
    let reader = BufReader::new(MultiBzDecoder::new(compressed));
    let mut offsets: Vec<u64> = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let offset = line
            .split(':')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| Error::BadIndexLine {
                line: i + 1,
                text: line.chars().take(200).collect(),
            })?;
        if offsets.last() != Some(&offset) {
            offsets.push(offset);
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    Ok(offsets)
}

/// Turns stream offsets into byte ranges. The returned list starts with the
/// header range `0..offsets[0]`, followed by one range per page stream.
pub fn stream_ranges(offsets: &[u64], file_len: u64) -> Vec<StreamRange> {
    let mut starts: Vec<u64> = Vec::with_capacity(offsets.len() + 1);
    if offsets.first() != Some(&0) {
        starts.push(0);
    }
    starts.extend(offsets.iter().copied().filter(|&o| o < file_len));

    let mut ranges = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(file_len);
        if end > start {
            ranges.push(StreamRange { start, end });
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use std::io::Write;

    fn bz(data: &[u8]) -> Vec<u8> {
        let mut enc = BzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn parses_offsets_with_colons_in_titles() {
        let index = b"600:10:April\n600:12:August\n9000:15:Talk: a: b\n9000:16:X\n";
        assert_eq!(parse_index(&bz(index)[..]).unwrap(), [600, 9000]);
    }

    #[test]
    fn bad_line_is_an_error_not_a_panic() {
        let err = parse_index(&bz(b"600:1:A\nnot-a-number:2:B\n")[..]).unwrap_err();
        assert!(matches!(err, Error::BadIndexLine { line: 2, .. }));
    }

    #[test]
    fn ranges_include_header_and_tail() {
        let r = stream_ranges(&[600, 9000], 12000);
        assert_eq!(
            r,
            [
                StreamRange { start: 0, end: 600 },
                StreamRange { start: 600, end: 9000 },
                StreamRange { start: 9000, end: 12000 },
            ]
        );
    }

    #[test]
    fn ranges_ignore_offsets_past_eof() {
        let r = stream_ranges(&[600, 99_999], 1000);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1], StreamRange { start: 600, end: 1000 });
    }
}
