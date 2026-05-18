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
//!
//! ## Decode acceleration (round 3)
//!
//! The decode loop builds a flat lookup table indexed by the next
//! `LUT_BITS` bits at the read position (`peek_bits(LUT_BITS)`). Each
//! entry pairs the symbol with the actual code length. When the
//! peeked LUT slot resolves a complete code (its `length` is the
//! symbol's true length and `length <= LUT_BITS`), one shift + one
//! load decodes the symbol. The peek-bits loop falls back to the
//! length-tier binary search only when the table's `max_len` exceeds
//! `LUT_BITS` (rare in the wild — `spec/02` §4.2 documents 16-bit
//! max-codelen as the empirical bound, so a 12-bit LUT covers nearly
//! every plane outright and the fallback only fires on the ~5–10
//! longest-coded symbols of a high-entropy plane).

use crate::error::{Error, Result};

/// LUT depth in bits. 12 is large enough to resolve the entire code
/// for every fixture in the `spec/05` corpus whose max-codelen is
/// `<= 12` (~all solid-colour, testsrc, and small-entropy frames),
/// and to give a fast skip-ahead even on the larger 16-bit cases.
/// `2^12 = 4096 entries` × 4 bytes = 16 KiB per plane — tiny next
/// to a typical 320×240 plane decode.
const LUT_BITS: u8 = 12;

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
/// in `O(plane_pixels * max_codelen)` worst case. For the common
/// `max_len <= LUT_BITS` case the decoder skips the prefix match and
/// uses a one-shot LUT keyed on the peeked next `LUT_BITS` bits.
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    /// Per-symbol forward table; `None` for unused / sentinel rows.
    entries: [Option<CodeEntry>; 256],
    /// `(length, code) -> symbol`, sorted by length then code, so the
    /// decoder can iterate length tiers in ascending order during the
    /// prefix match. Used by the slow-path fallback when the LUT misses.
    by_length: Vec<RevEntry>,
    /// Maximum non-sentinel code length, also = the longest tier's
    /// length. 0 if the table is the single-symbol special case.
    max_len: u8,
    /// `2^LUT_BITS`-entry flat lookup keyed by the next `LUT_BITS`
    /// bits at the read position. Entry `(sym, len)` resolves a code
    /// iff `len <= LUT_BITS`; otherwise the entry's `len` is
    /// `LUT_BITS + 1` (sentinel) and the slow-path search runs.
    /// Always exactly `1 << LUT_BITS` entries.
    lut: Vec<LutEntry>,
    /// `Some(sym)` iff the descriptor is the single-symbol special
    /// case (`code_length[s] = 0`, all others 255). `slice_data` of
    /// length 0 then yields `n_pixels` copies of `sym`.
    pub single_symbol: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LutEntry {
    sym: u8,
    /// Code length in bits. `0xff` is the "no fast match" sentinel —
    /// caller must fall back to the binary-search prefix walk.
    len: u8,
}

const LUT_MISS: LutEntry = LutEntry { sym: 0, len: 0xff };

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
                lut: Vec::new(),
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
                lut: Vec::new(),
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

        let lut = build_lut(&by_length);

        Ok(Self {
            entries,
            by_length,
            max_len,
            lut,
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
    ///
    /// Fast path: peek `LUT_BITS` at the bit-stream position, look up
    /// `(sym, len)` in `self.lut`. If `len <= LUT_BITS`, emit and
    /// advance by `len`. Otherwise, fall back to length-tier scan.
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
        let lut_bits = LUT_BITS as usize;

        // Group the reverse table into [start_idx_for_length] tiers.
        // The slow path needs them whenever LUT lookup is bypassed —
        // either because `max_len > LUT_BITS` (real long-code branch)
        // or because the bit-stream tail is shorter than `LUT_BITS`
        // (end-of-slice for shorter codes that still fit).
        let tiers: Vec<(u8, &[RevEntry])> = {
            let mut v = Vec::new();
            let mut i = 0;
            while i < self.by_length.len() {
                let l = self.by_length[i].0;
                let start = i;
                while i < self.by_length.len() && self.by_length[i].0 == l {
                    i += 1;
                }
                v.push((l, &self.by_length[start..i]));
            }
            v
        };

        for px in 0..n_pixels {
            let bp_start = br.position();

            // Fast LUT path: peek the next `LUT_BITS` bits if available.
            if br.has_bits(lut_bits) {
                let key = br.peek_bits(lut_bits) as usize;
                let entry = self.lut[key];
                if entry.len <= LUT_BITS {
                    // Guard: the symbol's true length must still fit in
                    // the unread tail of the stream. (Padding bits ARE
                    // zero per `spec/05` §4.3 and would otherwise
                    // wedge here; the slice-bit budget is the bound.)
                    let l = entry.len as usize;
                    if !br.has_bits(l) {
                        return Err(Error::SliceTruncated {
                            bit_position: bp_start,
                            expected_pixels: n_pixels,
                            decoded: px,
                        });
                    }
                    out.push(entry.sym);
                    br.consume_bits(l);
                    continue;
                }
                // entry.len == LUT_MISS sentinel (0xff) -> longer than
                // LUT_BITS. Fall through to the slow path.
            }

            // Slow path: bit-by-bit prefix scan across length tiers,
            // shortest first.
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

/// Build the flat 2^LUT_BITS prefix-lookup table for codes with
/// `length <= LUT_BITS`. Codes whose length exceeds the cap are
/// represented as no-fast-match entries (`LUT_MISS`).
///
/// Construction: for every entry `(len, code, sym)` in `by_length`
/// with `len <= LUT_BITS`, fill all `2^(LUT_BITS - len)` slots
/// whose top `len` bits equal `code` with `(sym, len)`. This is
/// the standard width-extension trick: the slot's index, read MSB
/// first as `LUT_BITS` bits, has the `len`-bit code as its prefix.
fn build_lut(by_length: &[RevEntry]) -> Vec<LutEntry> {
    let size = 1usize << LUT_BITS;
    let mut lut = vec![LUT_MISS; size];
    for &(len, code, sym) in by_length {
        if len > LUT_BITS {
            continue;
        }
        let shift = LUT_BITS - len;
        let base = (code as usize) << shift;
        let span = 1usize << shift;
        for slot in lut.iter_mut().skip(base).take(span) {
            *slot = LutEntry { sym, len };
        }
    }
    lut
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
    ///
    /// Fast path: combine the current 32-bit LE word and (optionally)
    /// the next one into a 64-bit register, shift to align the
    /// requested `n` bits at the bottom. Saves the `O(n)` byte read
    /// loop of the obvious bit-by-bit implementation.
    pub fn peek_bits(&self, n: usize) -> u32 {
        debug_assert!((1..=32).contains(&n));
        let bp = self.pos;
        let word_idx = bp / 32;
        let bit_in_word = bp % 32;
        let byte_off = word_idx * 4;
        // Load current word as u32 LE, treat the resulting `u32` as a
        // big-endian bit stream — the top `bit_in_word` bits have
        // already been consumed; the next 32 - `bit_in_word` bits
        // are unread.
        let w0 = self.load_word(byte_off);
        let combined: u64 = if bit_in_word + n <= 32 {
            // Single-word read suffices.
            (w0 as u64) << 32
        } else {
            // Need bits from the next word.
            let w1 = self.load_word(byte_off + 4);
            ((w0 as u64) << 32) | (w1 as u64)
        };
        // Shift left to drop the already-consumed prefix and right to
        // align the next `n` bits at the bottom.
        let aligned = combined << bit_in_word;
        (aligned >> (64 - n)) as u32
    }

    /// Load 4 bytes at `byte_off` as a u32 LE. Returns 0 past end of
    /// buffer (only used when the caller has already verified bit
    /// availability — the next-word load is past-end only when none
    /// of those bits are read by the immediate peek).
    fn load_word(&self, byte_off: usize) -> u32 {
        if byte_off + 4 <= self.data.len() {
            u32::from_le_bytes([
                self.data[byte_off],
                self.data[byte_off + 1],
                self.data[byte_off + 2],
                self.data[byte_off + 3],
            ])
        } else {
            // Partial / past-end: zero-extend. Bit reader only crosses
            // the past-end line when `peek_bits` was called with
            // `has_bits(n) == true` AND `n` bits stay within the
            // current word. The unused tail goes back as 0.
            let mut buf = [0u8; 4];
            for (i, b) in buf.iter_mut().enumerate() {
                if byte_off + i < self.data.len() {
                    *b = self.data[byte_off + i];
                }
            }
            u32::from_le_bytes(buf)
        }
    }

    pub fn consume_bits(&mut self, n: usize) {
        self.pos += n;
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

    #[test]
    fn lut_is_populated_for_short_codes() {
        // {3: 1, 128: 2, 218: 2}: sym 3 -> "1", sym 128 -> "01",
        // sym 218 -> "00". LUT bits = 12, so the LUT slot 0..2^11 are
        // sym 218 (code "00" prefix), slot 2^11..2^12 are sym 128
        // (code "01" prefix), then 2^12..2^13 / ... = sym 3.
        // Actually for LUT_BITS=12: code "1" with len=1 fills slots
        // 0x800..0xfff (top bit = 1), code "00" with len=2 fills
        // 0x000..0x3ff (top two bits = 00), code "01" with len=2
        // fills 0x400..0x7ff (top two bits = 01).
        let d = make_descriptor(&[(3, 1), (128, 2), (218, 2)]);
        let t = HuffmanTable::build(&d).unwrap();
        assert_eq!(t.lut[0x000], LutEntry { sym: 218, len: 2 });
        assert_eq!(t.lut[0x3ff], LutEntry { sym: 218, len: 2 });
        assert_eq!(t.lut[0x400], LutEntry { sym: 128, len: 2 });
        assert_eq!(t.lut[0x7ff], LutEntry { sym: 128, len: 2 });
        assert_eq!(t.lut[0x800], LutEntry { sym: 3, len: 1 });
        assert_eq!(t.lut[0xfff], LutEntry { sym: 3, len: 1 });
    }

    #[test]
    fn lut_skip_marks_long_codes_as_miss() {
        // Build a descriptor with one code of length 14 (> LUT_BITS=12)
        // and the rest filling up Kraft equality at shorter lengths.
        // Kraft: 2^-14 + ? = 1. Use: one symbol at 14, one at 14, then
        // shorter codes to complete: 2/2^14 + ... = 1.
        // Easiest: 2 syms × len=14, 1 sym × len=13, etc., or just
        // construct from a known length distribution. Use a uniform
        // 14-bit code over 4 symbols would be 4/16384 = 1/4096 — far
        // from Kraft. Better: {sym0: 14, sym1: 14, sym2: 2, sym3: 2, sym4: 2}.
        // Kraft: 2 × 2^-14 + 3 × 2^-2 = 2/16384 + 3/4 = 0.7501... not 1.
        // Try: {sym0: 14, sym1: 14, sym2: 13, sym3: 1}: 2/16384 + 1/8192 + 1/2 = ...
        //   = 2/16384 + 2/16384 + 8192/16384 = 8196/16384 ≠ 1.
        // Try: {sym0: 14, sym1: 14, sym2: 1, sym3: 2, sym4: 4, sym5: 4}:
        //   1/16384 + 1/16384 + 1/2 + 1/4 + 1/16 + 1/16 = 16384/16384 ? Compute.
        //   2/16384 = 1/8192. 1/2 + 1/4 = 3/4 = 12288/16384.
        //   1/16 + 1/16 = 1/8 = 2048/16384.
        //   Total: 2 + 12288 + 2048 = 14338 / 16384 ≠ 1.
        // Use the constructive method: append a single 14-bit code to
        // a Kraft-complete shorter set by splitting.
        // {1: 1, 2: 2, 3: 3, 4: 4, ..., 14: 14, sentinel-end: 14}:
        //   1/2 + 1/4 + 1/8 + ... + 1/16384 + 1/16384 = 1.
        // Build: codelen[sym] = sym_index_in_list + 1, distinct symbols.
        let pairs: Vec<(u8, u8)> = (0..14u8)
            .map(|i| (i, (i + 1)))
            .chain(std::iter::once((20u8, 14u8))) // double-up at length 14 closes Kraft.
            .collect();
        let d = make_descriptor(&pairs);
        let t = HuffmanTable::build(&d).unwrap();
        assert_eq!(t.max_code_length(), 14);
        // Find a LUT slot that points at a code whose length > LUT_BITS.
        // After construction, the longest two codes (len=14) are the
        // two symbols with smallest count → most-zeros prefix → live
        // in the low LUT-index range. Confirm at least one LUT_MISS.
        let mut miss_count = 0;
        for entry in &t.lut {
            if entry.len > LUT_BITS {
                miss_count += 1;
            }
        }
        assert!(
            miss_count > 0,
            "long-codelen table must have at least one LUT_MISS entry"
        );

        // Round-trip a stream containing every symbol; the slow-path
        // fallback handles the > LUT_BITS codes and the LUT handles
        // the short ones.
        let symbols: Vec<u8> = (0..14u8).chain(std::iter::once(20u8)).collect();
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (c, l) = t.code_for(s).unwrap();
            bw.write_code(c, l);
        }
        let bytes = bw.finish();
        let decoded = t.decode_slice(&bytes, symbols.len()).unwrap();
        assert_eq!(&decoded, &symbols);
    }

    #[test]
    fn decode_handles_short_codes_at_end_of_buffer() {
        // Regression: when fewer than LUT_BITS bits remain, the
        // decoder must still resolve a short code via the slow-path
        // tier scan. Build a 1-bit-code descriptor (most extreme case)
        // and emit just 16 bits (one 32-bit word's worth of "1" codes
        // is 32 pixels; ensure no truncation at the final bits).
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();
        // 4 bytes: 32 bits, all = 1 (sym 0). Decode 32 symbols.
        let data = vec![0xffu8, 0xff, 0xff, 0xff];
        let out = t.decode_slice(&data, 32).unwrap();
        assert!(out.iter().all(|&s| s == 0));
        assert_eq!(out.len(), 32);
    }
}
