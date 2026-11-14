//! Variable-byte integers: 7 data bits per byte, high bit set means "more
//! bytes follow". Values under 128 take one byte; a u32 takes at most five.

#[inline]
pub fn encode(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decodes one value at `*pos` and advances `*pos`. Returns `None` on
/// truncated input or a value that does not fit in u32, never panics.
#[inline]
pub fn decode(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v: u32 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        if shift == 28 && b > 0x0F {
            return None; // would overflow u32 (or more than 5 bytes)
        }
        v |= u32::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn enc(v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        encode(v, &mut out);
        out
    }

    #[test]
    fn sizes_and_edges() {
        assert_eq!(enc(0), [0x00]);
        assert_eq!(enc(127), [0x7F]);
        assert_eq!(enc(128), [0x80, 0x01]);
        assert_eq!(enc(300), [0xAC, 0x02]);
        assert_eq!(enc(u32::MAX).len(), 5);
        for v in [0, 1, 127, 128, 16_383, 16_384, u32::MAX - 1, u32::MAX] {
            let mut pos = 0;
            assert_eq!(decode(&enc(v), &mut pos), Some(v));
            assert_eq!(pos, enc(v).len());
        }
    }

    #[test]
    fn truncated_and_overflowing_input() {
        assert_eq!(decode(&[], &mut 0), None);
        assert_eq!(decode(&[0x80], &mut 0), None);
        assert_eq!(decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10], &mut 0), None);
        assert_eq!(decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01], &mut 0), None);
    }

    proptest! {
        /// The spec's example: [4, 19, 22, 40] stored as gaps [4, 15, 3, 18].
        #[test]
        fn sequences_round_trip(values in prop::collection::vec(any::<u32>(), 0..300)) {
            let mut buf = Vec::new();
            for &v in &values { encode(v, &mut buf); }
            let mut pos = 0;
            let mut back = Vec::new();
            while pos < buf.len() { back.push(decode(&buf, &mut pos).unwrap()); }
            prop_assert_eq!(back, values);
        }

        #[test]
        fn never_panics_on_garbage(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
            let mut pos = 0;
            while decode(&bytes, &mut pos).is_some() {}
        }
    }
}
