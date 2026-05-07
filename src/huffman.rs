//! Ut Video canonical Huffman code construction + slice-data bit
//! reader.
//!
//! `spec/05` §2.2 fixes the construction algorithm: enumerate the
//! 256-entry code-length descriptor in **(length DESC, symbol idx
//! DESC)** order and assign codes starting at 0, incrementing within
//! a length tier and right-shifting at length transitions. This is
//! the structural mirror of RFC 1951 §3.2.2 and is what produces the
//! wiki's "longest code = zero prefix, shortest = all ones".
//!
//! The bit reader walks slice data as 32-bit little-endian words,
//! MSB-first within each word (`spec/05` §4). Trailing bits inside
//! the last word are zero padding.

use crate::error::{Error, Result};

/// `(length, code, sym)` triple as stored in the reverse table.
type RevEntry = (u8, u32, u8);

/// One entry of the codebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodeEntry {
    code: u32,
    length: u8,
}

/// A built Huffman codebook for a single plane. Keeps both the
/// per-symbol forward table (encoder side) and a length-grouped
/// reverse table (decoder side) so the decode loop can prefix-match
/// in `O(plane_pixels * max_codelen)` worst case.
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    /// Per-symbol forward table; `None` for unused / sentinel rows.
    entries: [Option<CodeEntry>; 256],
    /// `(length, code) -> symbol`, sorted by length then code, so the
    /// decoder can iterate length tiers in ascending order during the
    /// prefix match.
    by_length: Vec<RevEntry>,
    /// Maximum non-sentinel code length, also = the longest tier's
    /// length. 0 if the table is the single-symbol special case.
    max_len: u8,
    /// `Some(sym)` iff the descriptor is the single-symbol special
    /// case (`code_length[s] = 0`, all others 255). `slice_data` of
    /// length 0 then yields `n_pixels` copies of `sym`.
    pub single_symbol: Option<u8>,
}

impl HuffmanTable {
    /// Build a codebook from a 256-entry code-length descriptor per
    /// `spec/02` §4 + `spec/05` §2.2.
    pub fn build(code_length: &[u8; 256]) -> Result<Self> {
        // Detect the single-symbol special case (`spec/05` §6.1).
        let zero_indices: Vec<u8> = (0..=255u8)
            .filter(|s| code_length[*s as usize] == 0)
            .collect();
        if zero_indices.len() > 1 {
            return Err(Error::MultipleSingleSymbolSentinels);
        }
        if let Some(&single) = zero_indices.first() {
            // Every other byte must be 255 for the single-symbol case
            // to match `spec/02` §4 — but a defensive decoder treats
            // any other non-255 entry as malformed too.
            for s in 0..=255u8 {
                if s != single && code_length[s as usize] != 255 {
                    return Err(Error::KraftViolation);
                }
            }
            return Ok(Self {
                entries: [None; 256],
                by_length: Vec::new(),
                max_len: 0,
                single_symbol: Some(single),
            });
        }

        // Collect (length, sym) pairs for non-sentinel entries.
        let mut items: Vec<(u8, u8)> = (0..=255u8)
            .filter_map(|s| match code_length[s as usize] {
                0 | 255 => None,
                l => Some((l, s)),
            })
            .collect();

        if items.is_empty() {
            // No entries and no single-symbol marker: empty plane.
            // The caller MUST size the slice data to 0 in this case;
            // a non-empty slice with this descriptor is malformed
            // and gets caught at decode time.
            return Ok(Self {
                entries: [None; 256],
                by_length: Vec::new(),
                max_len: 0,
                single_symbol: None,
            });
        }

        // Verify Kraft equality before assigning codes — refuses
        // non-canonical descriptors per `spec/02` §4 invariant.
        kraft_check(&items)?;

        // Sort by (length DESC, sym DESC) per `spec/05` §2.2 step 2.
        items.sort_by(|a, b| match b.0.cmp(&a.0) {
            std::cmp::Ordering::Equal => b.1.cmp(&a.1),
            other => other,
        });

        let mut entries: [Option<CodeEntry>; 256] = [None; 256];
        let mut by_length = Vec::with_capacity(items.len());
        let mut code: u32 = 0;
        let mut prev_len = items[0].0;
        let max_len = prev_len;
        for (l, s) in items {
            if l < prev_len {
                code = (code + 1) >> (prev_len - l);
                prev_len = l;
            }
            entries[s as usize] = Some(CodeEntry { code, length: l });
            by_length.push((l, code, s));
            code = code.wrapping_add(1);
        }

        // Stable-sort the reverse table by length ASC to make the
        // decode loop's prefix-match pass iterate shortest-first.
        by_length.sort_by_key(|t| t.0);

        Ok(Self {
            entries,
            by_length,
            max_len,
            single_symbol: None,
        })
    }

    /// Look up the bit pattern + length for symbol `s` if present.
    pub fn code_for(&self, s: u8) -> Option<(u32, u8)> {
        self.entries[s as usize].map(|e| (e.code, e.length))
    }

    /// Decode `n_pixels` residuals from `slice_data`. The slice byte
    /// length must be a multiple of 4 (`spec/05` §4.1) — caller
    /// validates.
    pub fn decode_slice(&self, slice_data: &[u8], n_pixels: usize) -> Result<Vec<u8>> {
        if let Some(sym) = self.single_symbol {
            // Single-symbol fast path: zero bits, every pixel = sym.
            // `slice_data` should be empty in this case (`spec/05` §6.1).
            return Ok(vec![sym; n_pixels]);
        }
        if n_pixels == 0 {
            return Ok(Vec::new());
        }
        if self.by_length.is_empty() {
            // No codebook + non-zero pixel count is malformed.
            return Err(Error::HuffmanDecodeFailure { bit_position: 0 });
        }
        let mut br = BitReader::new(slice_data);
        let mut out = Vec::with_capacity(n_pixels);
        // Group the reverse table into [start_idx_for_length] for fast
        // tiered scan: at each step we walk through tiers in
        // increasing length order, peeking exactly that many bits.
        // Pre-compute the contiguous slices for each distinct length.
        let mut tiers: Vec<(u8, &[RevEntry])> = Vec::new();
        let mut i = 0;
        while i < self.by_length.len() {
            let l = self.by_length[i].0;
            let start = i;
            while i < self.by_length.len() && self.by_length[i].0 == l {
                i += 1;
            }
            tiers.push((l, &self.by_length[start..i]));
        }

        for px in 0..n_pixels {
            let bp_start = br.position();
            let mut matched: Option<u8> = None;
            for (l, tier) in &tiers {
                if !br.has_bits(*l as usize) {
                    return Err(Error::SliceTruncated {
                        bit_position: bp_start,
                        expected_pixels: n_pixels,
                        decoded: px,
                    });
                }
                let candidate = br.peek_bits(*l as usize);
                if let Ok(idx) = tier.binary_search_by(|probe| probe.1.cmp(&candidate)) {
                    matched = Some(tier[idx].2);
                    br.consume_bits(*l as usize);
                    break;
                }
            }
            match matched {
                Some(sym) => out.push(sym),
                None => {
                    return Err(Error::HuffmanDecodeFailure {
                        bit_position: bp_start,
                    })
                }
            }
        }
        Ok(out)
    }

    /// Maximum non-sentinel code length in this table. 0 for the
    /// single-symbol case.
    pub fn max_code_length(&self) -> u8 {
        self.max_len
    }
}

fn kraft_check(items: &[(u8, u8)]) -> Result<()> {
    // Compute Σ 2^(-l) using a numerator over 2^max_l. Equality must
    // be exact.
    let max_l = items.iter().map(|(l, _)| *l).max().unwrap();
    if max_l == 0 || max_l > 32 {
        // 0 is the single-symbol path (handled before this fn);
        // > 32 is unreachable for a 256-symbol alphabet under
        // canonical Huffman, but reject defensively.
        return Err(Error::KraftViolation);
    }
    let scale: u64 = 1u64 << max_l;
    let mut sum: u64 = 0;
    for (l, _) in items {
        if *l == 0 || *l > max_l {
            return Err(Error::KraftViolation);
        }
        sum += scale >> *l;
    }
    if sum != scale {
        return Err(Error::KraftViolation);
    }
    Ok(())
}

/// Slice-data bit reader: 32-bit LE words, MSB-first within each
/// word, padded to the next 32-bit word boundary with zeros.
/// `spec/05` §4.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Cumulative bit position from start of `data`.
    pos: usize,
    /// Total bits available in `data` (= `data.len() * 8`).
    total_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            total_bits: data.len() * 8,
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn has_bits(&self, n: usize) -> bool {
        self.pos + n <= self.total_bits
    }

    /// Peek the next `n` bits as a u32, MSB-first. `n` MUST be
    /// `1..=32`. Caller MUST ensure `has_bits(n)` first.
    pub fn peek_bits(&self, n: usize) -> u32 {
        debug_assert!((1..=32).contains(&n));
        let mut value: u32 = 0;
        for i in 0..n {
            let bit = self.read_bit_at(self.pos + i);
            value = (value << 1) | bit as u32;
        }
        value
    }

    pub fn consume_bits(&mut self, n: usize) {
        self.pos += n;
    }

    /// Read bit at absolute bit-stream position `bp`. The slice's
    /// 32-bit LE word is `bp / 32`; the bit number within that word
    /// (with MSB = 0) is `bp % 32`. Maps to the byte
    /// `word_off + 3 - (bit_in_word / 8)` and the bit (MSB=0) within
    /// that byte = `bit_in_word % 8`.
    fn read_bit_at(&self, bp: usize) -> u8 {
        let word = bp / 32;
        let bit_in_word = bp % 32;
        let word_off = word * 4;
        let byte_in_word = 3 - (bit_in_word / 8);
        let bit_in_byte = 7 - (bit_in_word % 8);
        let byte = self.data[word_off + byte_in_word];
        (byte >> bit_in_byte) & 1
    }
}

/// Slice-data bit writer: mirror of [`BitReader`]. Writes MSB-first
/// inside each 32-bit LE word; pads to a 32-bit boundary on `finish`.
pub struct BitWriter {
    /// Pending word; bits accumulate from MSB downward.
    word: u32,
    /// Bits already written into `word` (0..=32).
    bits_in_word: u32,
    /// Words committed so far, LE.
    out: Vec<u8>,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            word: 0,
            bits_in_word: 0,
            out: Vec::new(),
        }
    }

    /// Append the low `length` bits of `code` to the bit stream,
    /// MSB-first. `length` MUST be in `1..=32`.
    pub fn write_code(&mut self, code: u32, length: u8) {
        debug_assert!((1..=32).contains(&length));
        let mut remaining = length as u32;
        // Mask code to its low `length` bits.
        let mut value: u64 = if length == 32 {
            code as u64
        } else {
            (code as u64) & ((1u64 << length) - 1)
        };
        while remaining > 0 {
            let free = 32 - self.bits_in_word;
            let take = free.min(remaining);
            let shift = remaining - take;
            let chunk = ((value >> shift) as u32)
                & if take == 32 {
                    0xffff_ffff
                } else {
                    (1u32 << take) - 1
                };
            self.word |= chunk << (free - take);
            self.bits_in_word += take;
            remaining -= take;
            // Drop the bits we just consumed from the high end of `value`.
            value &= if shift == 0 { 0 } else { (1u64 << shift) - 1 };
            if self.bits_in_word == 32 {
                self.flush_word();
            }
        }
    }

    /// Flush the pending word (with zero padding) and return the
    /// accumulated byte stream. The returned length is always a
    /// multiple of 4.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bits_in_word > 0 {
            self.flush_word();
        }
        self.out
    }

    fn flush_word(&mut self) {
        // 32-bit LE word with the bits MSB-first inside it: when read
        // back, the most-significant bit of `self.word` is bit 0 of
        // the bit stream. Encode as `to_le_bytes` of `self.word`.
        self.out.extend_from_slice(&self.word.to_le_bytes());
        self.word = 0;
        self.bits_in_word = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_descriptor(pairs: &[(u8, u8)]) -> [u8; 256] {
        // Default to 255 (sentinel-unused).
        let mut d = [255u8; 256];
        for &(s, l) in pairs {
            d[s as usize] = l;
        }
        d
    }

    #[test]
    fn build_zeros_left_table_matches_spec_3_1_1() {
        // R3-uly4-zeros-left: codelens {0: 1, 128: 1}.
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();
        // Per spec/05 §3.1.1: codes[128] = "0" (1 bit), codes[0] = "1".
        assert_eq!(t.code_for(128), Some((0, 1)));
        assert_eq!(t.code_for(0), Some((1, 1)));
        assert_eq!(t.max_code_length(), 1);
    }

    #[test]
    fn build_arith_left_table_matches_spec_3_1_2() {
        // {3: 1, 128: 2, 218: 2}. Expected: 218 → "00", 128 → "01", 3 → "1".
        let d = make_descriptor(&[(3, 1), (128, 2), (218, 2)]);
        let t = HuffmanTable::build(&d).unwrap();
        assert_eq!(t.code_for(218), Some((0b00, 2)));
        assert_eq!(t.code_for(128), Some((0b01, 2)));
        assert_eq!(t.code_for(3), Some((0b1, 1)));
    }

    #[test]
    fn build_uniform_byte_table_matches_spec_6_3() {
        // Every symbol at length 8: byte symbol s -> bit pattern ~s & 0xff.
        let mut d = [8u8; 256];
        // Sanity: keep as constant 8 across the alphabet — Kraft sum = 1.
        let _ = &mut d;
        let t = HuffmanTable::build(&d).unwrap();
        assert_eq!(t.code_for(255), Some((0x00, 8)));
        assert_eq!(t.code_for(254), Some((0x01, 8)));
        assert_eq!(t.code_for(0), Some((0xff, 8)));
        assert_eq!(t.code_for(127), Some((0x80, 8)));
    }

    #[test]
    fn build_single_symbol_marker() {
        let mut d = [255u8; 256];
        d[42] = 0;
        let t = HuffmanTable::build(&d).unwrap();
        assert_eq!(t.single_symbol, Some(42));
        assert!(t.code_for(42).is_none());
    }

    #[test]
    fn build_rejects_non_kraft() {
        let d = make_descriptor(&[(0, 1), (1, 3)]); // 1/2 + 1/8 = 5/8
        assert!(matches!(
            HuffmanTable::build(&d),
            Err(Error::KraftViolation)
        ));
    }

    #[test]
    fn build_rejects_multiple_zero_sentinels() {
        let mut d = [255u8; 256];
        d[5] = 0;
        d[10] = 0;
        assert!(matches!(
            HuffmanTable::build(&d),
            Err(Error::MultipleSingleSymbolSentinels)
        ));
    }

    #[test]
    fn bit_reader_zeros_left_fixture_first_bits_from_spec_3_1_1() {
        // 32 bytes with the first 32-bit LE word = 0x7fffffff. The
        // bit stream then opens with a 0 followed by 31 ones.
        // We synthesise this by hand: bytes are the LE representation
        // of 0x7fffffff = ff ff ff 7f.
        let mut data = vec![0xffu8, 0xff, 0xff, 0x7f];
        // Pad to 32 bytes total with all-ones words = 0xffffffff.
        for _ in 1..8 {
            data.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        }
        let mut br = BitReader::new(&data);
        // First bit is 0, next 31 bits are 1.
        assert_eq!(br.peek_bits(1), 0);
        br.consume_bits(1);
        for _ in 0..31 {
            assert_eq!(br.peek_bits(1), 1);
            br.consume_bits(1);
        }
        // Next word is all-ones (0xffffffff), so the next bit is 1.
        assert_eq!(br.peek_bits(1), 1);
    }

    #[test]
    fn bit_reader_decode_matches_spec_3_1_1() {
        // Reproduce the prediction of spec/05 §3.1.1: a 256-symbol
        // residual stream `[128, 0, 0, ..., 0]` packs into 32 bytes
        // whose first word is 0x7fffffff and the rest are 0xffffffff.
        // Decode using the table from build_zeros_left_table_matches_spec.
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();

        let mut data = vec![0xffu8, 0xff, 0xff, 0x7f];
        for _ in 1..8 {
            data.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        }
        let residuals = t.decode_slice(&data, 256).unwrap();
        assert_eq!(residuals[0], 128);
        for r in &residuals[1..] {
            assert_eq!(*r, 0);
        }
    }

    #[test]
    fn bit_writer_round_trip_zeros_left() {
        // Encode: first symbol 128 -> "0"; remaining 255 zeros -> 255 × "1".
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();
        let mut bw = BitWriter::new();
        // 128 first
        let (c, l) = t.code_for(128).unwrap();
        bw.write_code(c, l);
        for _ in 0..255 {
            let (c, l) = t.code_for(0).unwrap();
            bw.write_code(c, l);
        }
        let bytes = bw.finish();
        // 256 bits = 32 bytes, no padding required.
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[0..4], &[0xff, 0xff, 0xff, 0x7f]);
        for chunk in bytes[4..].chunks_exact(4) {
            assert_eq!(chunk, &[0xff, 0xff, 0xff, 0xff]);
        }
    }

    #[test]
    fn bit_writer_pads_partial_word_with_zeros() {
        // 1 bit `1` -> word 0x80000000 -> bytes 00 00 00 80.
        let mut bw = BitWriter::new();
        bw.write_code(1, 1);
        let bytes = bw.finish();
        assert_eq!(bytes, vec![0x00, 0x00, 0x00, 0x80]);
    }

    #[test]
    fn bit_writer_arith_left_matches_spec_3_1_2_first_word() {
        // From spec/05 §3.1.2: bit stream begins "01 1×15 00 1×13 …".
        // First 32 bits then equal 0x7fff9fff = LE bytes ff 9f ff 7f.
        // (The spec's textual "0x7fffff9f" is a typo; the spec's own
        // hex dump first 4 bytes are `ff 9f ff 7f`, matching this.)
        let d = make_descriptor(&[(3, 1), (128, 2), (218, 2)]);
        let t = HuffmanTable::build(&d).unwrap();
        let mut bw = BitWriter::new();
        // Symbol 128 (code "01"), then 15 × symbol 3 (each "1"),
        // then 15 × (symbol 218 ("00") + 15 × symbol 3 ("1")).
        bw.write_code(t.code_for(128).unwrap().0, t.code_for(128).unwrap().1);
        for _ in 0..15 {
            bw.write_code(t.code_for(3).unwrap().0, t.code_for(3).unwrap().1);
        }
        for _ in 0..15 {
            bw.write_code(t.code_for(218).unwrap().0, t.code_for(218).unwrap().1);
            for _ in 0..15 {
                bw.write_code(t.code_for(3).unwrap().0, t.code_for(3).unwrap().1);
            }
        }
        let bytes = bw.finish();
        // Total bits: 2 + 15 + 15 × 17 = 272, padded to 288 -> 36 bytes.
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[0..4], &[0xff, 0x9f, 0xff, 0x7f]);
    }

    #[test]
    fn round_trip_random_descriptor() {
        // Pick a Kraft-complete descriptor and round-trip.
        let d = make_descriptor(&[(10, 2), (20, 2), (30, 2), (40, 2)]); // 4 × 1/4 = 1
        let t = HuffmanTable::build(&d).unwrap();
        let symbols = [10u8, 20, 30, 40, 10, 40, 30, 20, 10, 10, 20, 30];
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (c, l) = t.code_for(s).unwrap();
            bw.write_code(c, l);
        }
        let bytes = bw.finish();
        let decoded = t.decode_slice(&bytes, symbols.len()).unwrap();
        assert_eq!(&decoded, &symbols);
    }
}
