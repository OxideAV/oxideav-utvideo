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
//! ## Decode acceleration
//!
//! Residuals are decoded straight into the output plane (the inverse
//! predictor then runs in place), from a 64-bit bit register refilled
//! one word at a time:
//!
//! - a `2^LUT_BITS`-entry table resolves any code of at most `LUT_BITS`
//!   bits in one probe; longer codes (`spec/02` §4.2 reports up to 16
//!   bits in practice) are found by a search of the per-length tiers;
//! - when the shortest code is at most half of `LUT_BITS`, a companion
//!   table resolves **two** symbols per probe wherever two codes fit in
//!   one index, with a branch-free refill — short codes dominate
//!   predicted pictures, and the per-symbol cost of a Huffman decoder
//!   is the serial index → load → shift chain, which this halves;
//! - codes longer than 32 bits (`spec/05` §7.2 allows 254) are decoded
//!   bit-serially from the length histogram.

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
    /// `(length, start, end)` index ranges into `by_length`, one per
    /// distinct code length, ascending; each range is sorted by code.
    tiers: Vec<(u8, u32, u32)>,
    /// Maximum non-sentinel code length, also = the longest tier's
    /// length. 0 if the table is the single-symbol special case.
    max_len: u8,
    /// `2^LUT_BITS`-entry flat lookup keyed by the next `LUT_BITS`
    /// bits at the read position. Entry `(sym, len)` resolves a code
    /// iff `len <= LUT_BITS`; otherwise the entry's `len` is
    /// `LUT_BITS + 1` (sentinel) and the slow-path search runs.
    /// Always exactly `1 << LUT_BITS` entries.
    lut: Vec<LutEntry>,
    /// Two-symbol companion of `lut`, same index: see [`build_pair_lut`].
    pair_lut: Vec<u32>,
    /// `Some(sym)` iff the descriptor is the single-symbol special
    /// case (`code_length[s] = 0`, all others 255). `slice_data` of
    /// length 0 then yields `n_pixels` copies of `sym`.
    pub single_symbol: Option<u8>,
    /// Bit-serial decode structure, present iff some code is longer
    /// than [`WIDE_CODE_BITS`] (`spec/05` §7.2 allows lengths up to
    /// 254). Such a table keeps only its `<= WIDE_CODE_BITS` codes in
    /// `entries` / `by_length` / `lut`.
    serial: Option<SerialCode>,
}

/// Longest code length whose bit pattern is materialised as a `u32`.
/// Longer codes (`spec/05` §7.2: any length in `1..=254` is legitimate
/// on the wire) are decoded bit-serially from the length histogram.
const WIDE_CODE_BITS: u8 = 32;

/// Length-histogram form of a prefix code, for bit-serial decoding of
/// codes of any length.
///
/// The `spec/05` §2.2 assignment (length DESC, symbol DESC, counting up
/// from zero) is the bitwise complement of the code that sorts symbols
/// by (length ASC, symbol ASC) and counts up from zero: complementing
/// every bit mirrors the code tree, which turns "longest code leftmost"
/// into "shortest code leftmost" and reverses the order inside each
/// length tier. Decoding the complemented bit stream therefore needs
/// only, per length, how many codes have that length, and the symbols
/// listed in (length ASC, symbol ASC) order: after reading `len` bits,
/// the value's offset past the first code of that length is either an
/// index into this tier or carries (minus the tier's size, doubled)
/// into the next. The offset never exceeds the symbol count for a
/// complete code, so no wide arithmetic is involved.
#[derive(Debug, Clone)]
struct SerialCode {
    /// `tier_size[len]` = number of symbols whose code is `len` bits.
    tier_size: [u16; 256],
    /// Symbols in (length ASC, symbol ASC) order.
    symbols: Vec<u8>,
    max_len: usize,
}

impl SerialCode {
    fn new(items: &[(u8, u8)]) -> Self {
        let mut tier_size = [0u16; 256];
        for &(l, _) in items {
            tier_size[l as usize] += 1;
        }
        let mut sorted: Vec<(u8, u8)> = items.to_vec();
        sorted.sort_unstable();
        SerialCode {
            tier_size,
            symbols: sorted.iter().map(|&(_, s)| s).collect(),
            max_len: sorted.last().map_or(0, |&(l, _)| l as usize),
        }
    }

    /// Decode one symbol at `br`'s cursor, advancing past its code.
    /// `None` when the slice ends inside the code.
    fn decode_one(&self, br: &mut BitReader<'_>) -> Option<u8> {
        let mut offset = 0usize; // value minus the tier's first code
        let mut first = 0usize; // index of the tier's first symbol
        for len in 1..=self.max_len {
            if !br.has_bits(1) {
                return None;
            }
            // Complemented stream bit.
            offset = (offset << 1) | (1 - br.peek_bits(1) as usize);
            br.consume_bits(1);
            let tier = self.tier_size[len] as usize;
            if offset < tier {
                return Some(self.symbols[first + offset]);
            }
            offset -= tier;
            first += tier;
        }
        // Unreachable for a complete code (the last tier absorbs every
        // remaining value); treated as end-of-data defensively.
        None
    }
}

/// Outcome of [`HuffmanTable::search_tiers`].
enum TierHit {
    /// `(symbol, code length)`.
    Code(u8, u32),
    /// A candidate length exceeds the bits available.
    NeedsMoreBits,
    /// No code of any length matches.
    NoCode,
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
                tiers: Vec::new(),
                max_len: 0,
                lut: Vec::new(),
                pair_lut: Vec::new(),
                single_symbol: Some(single),
                serial: None,
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
                tiers: Vec::new(),
                max_len: 0,
                lut: Vec::new(),
                pair_lut: Vec::new(),
                single_symbol: None,
                serial: None,
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
        let max_len = items[0].0;
        let serial = (max_len > WIDE_CODE_BITS).then(|| SerialCode::new(&items));
        if serial.is_none() {
            let mut code: u32 = 0;
            let mut prev_len = max_len;
            for (l, s) in items {
                if l < prev_len {
                    code = (code + 1) >> (prev_len - l);
                    prev_len = l;
                }
                entries[s as usize] = Some(CodeEntry { code, length: l });
                by_length.push((l, code, s));
                code = code.wrapping_add(1);
            }
        } else {
            // Some code is too long for a `u32`. Materialise the short
            // ones through the complement identity (see `SerialCode`):
            // walk (length ASC, symbol ASC), counting up from zero and
            // doubling at each length step, and complement the result.
            let mut value: u32 = 0;
            let mut prev_len: u8 = 0;
            for &(l, s) in items.iter().rev() {
                if l > WIDE_CODE_BITS {
                    break;
                }
                // `value < 2^prev_len` holds before the shift (complete
                // prefix code), so the shifted value fits `l <= 32` bits.
                value = ((u64::from(value)) << (l - prev_len)) as u32;
                prev_len = l;
                let mask = if l == 32 { u32::MAX } else { (1u32 << l) - 1 };
                let code = !value & mask;
                entries[s as usize] = Some(CodeEntry { code, length: l });
                by_length.push((l, code, s));
                value = value.wrapping_add(1);
            }
        }

        // Stable-sort the reverse table by length ASC to make the
        // decode loop's prefix-match pass iterate shortest-first.
        by_length.sort_unstable_by_key(|t| (t.0, t.1));
        let mut tiers: Vec<(u8, u32, u32)> = Vec::new();
        for (i, entry) in by_length.iter().enumerate() {
            match tiers.last_mut() {
                Some(t) if t.0 == entry.0 => t.2 = i as u32 + 1,
                _ => tiers.push((entry.0, i as u32, i as u32 + 1)),
            }
        }

        let lut = build_lut(&by_length);
        let pair_lut = build_pair_lut(&lut);

        Ok(Self {
            entries,
            by_length,
            tiers,
            max_len,
            lut,
            pair_lut,
            single_symbol: None,
            serial,
        })
    }

    /// Look up the bit pattern + length for symbol `s` if present.
    /// `None` also for a symbol whose code is longer than 32 bits (such
    /// tables are decode-only).
    pub fn code_for(&self, s: u8) -> Option<(u32, u8)> {
        self.entries[s as usize].map(|e| (e.code, e.length))
    }

    /// Decode `n_pixels` residuals from `slice_data`. The slice byte
    /// length must be a multiple of 4 (`spec/05` §4.1) — caller
    /// validates.
    pub fn decode_slice(&self, slice_data: &[u8], n_pixels: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; n_pixels];
        self.decode_slice_into(slice_data, &mut out)?;
        Ok(out)
    }

    /// Strict-mode slice decode: decode `n_pixels` residuals exactly as
    /// [`decode_slice`](Self::decode_slice), then additionally verify
    /// the trailing word-boundary padding (`spec/05` §4.3) is all zero.
    /// `spec/05` §8 names a non-zero padding bit a SHOULD-warn for
    /// defensive decoders; this path elevates it to a hard
    /// [`Error::NonZeroPadding`] so a conformance-checking caller can
    /// reject streams whose encoder did not zero the slice tail.
    ///
    /// `plane` / `slice` are threaded through only to populate the error
    /// variant's location fields; they do not affect decoding.
    pub fn decode_slice_strict(
        &self,
        slice_data: &[u8],
        n_pixels: usize,
        plane: usize,
        slice: usize,
    ) -> Result<Vec<u8>> {
        let mut out = vec![0u8; n_pixels];
        self.decode_slice_into_strict(slice_data, &mut out, plane, slice)?;
        Ok(out)
    }

    /// [`decode_slice_strict`](Self::decode_slice_strict) writing into a
    /// caller-provided buffer (one residual per element of `out`).
    pub fn decode_slice_into_strict(
        &self,
        slice_data: &[u8],
        out: &mut [u8],
        plane: usize,
        slice: usize,
    ) -> Result<()> {
        // The single-symbol / empty paths consume no bits (`spec/05`
        // §6.1), so the whole of `slice_data` is scanned as padding.
        let payload_bits = self.decode_slice_into(slice_data, out)?;
        let mut br = BitReader::new(slice_data);
        br.consume_bits(payload_bits);
        if let Some(bit_position) = br.first_nonzero_bit_from_here() {
            return Err(Error::NonZeroPadding {
                plane,
                slice,
                bit_position,
            });
        }
        Ok(())
    }

    /// Decode `out.len()` residuals from `slice_data` into `out` and
    /// return the number of bits consumed (the position of the first
    /// padding bit). No allocation.
    ///
    /// The next bits of the stream sit left-aligned in a 64-bit
    /// register refilled one 32-bit word at a time (`spec/05` §4: LE
    /// words, MSB first). While the register holds at least `max_len`
    /// stream bits every code resolves from real data — one probe of
    /// the `LUT_BITS`-wide table, or a search of the longer length
    /// tiers on a miss — with no end-of-data test per symbol. Only the
    /// last few symbols of a slice, when the data has run out and fewer
    /// than `max_len` bits remain, take the length-by-length walk that
    /// distinguishes a short slice from an undecodable prefix.
    pub fn decode_slice_into(&self, slice_data: &[u8], out: &mut [u8]) -> Result<usize> {
        if let Some(sym) = self.single_symbol {
            // Zero bits, every pixel = sym (`spec/05` §6.1).
            out.fill(sym);
            return Ok(0);
        }
        let n_pixels = out.len();
        if n_pixels == 0 {
            return Ok(0);
        }
        if self.by_length.is_empty() && self.serial.is_none() {
            // No codebook + non-zero pixel count is malformed.
            return Err(Error::HuffmanDecodeFailure { bit_position: 0 });
        }
        if let Some(serial) = &self.serial {
            return self.decode_slice_serial(serial, slice_data, out);
        }

        let (words, rest) = slice_data.as_chunks::<4>();
        // A trailing partial word (only reachable by direct callers —
        // the frame decoder requires word-aligned slices) is read as a
        // zero-extended LE word of which the first `8 * rest.len()`
        // stream bits are in range.
        let partial_bits = 8 * rest.len() as u32;
        let mut partial_pending = partial_bits != 0;
        let max_len = u32::from(self.max_len);
        let lut = &self.lut[..];

        let mut acc: u64 = 0; // next stream bits, left-aligned
        let mut acc_len: u32 = 0; // valid bits in `acc`
        let mut next_word = 0usize;
        let mut partial_loaded = 0u32;
        let mut px = 0usize;

        // Bulk loop, taken when two codes can share one table index
        // (shortest code at most half of `LUT_BITS`); a table of longer
        // codes yields one symbol per probe either way and goes straight
        // to the loop below. It runs while a whole word remains to
        // refill from and four output slots are free. The refill is a
        // conditional move (no branch to mispredict on variable code
        // lengths): after it the register holds at least 33 stream
        // bits, enough for two probes of at most `LUT_BITS` bits each,
        // or for any single code this path can meet (`max_len <= 32`).
        let pair_lut = &self.pair_lut[..];
        let pairs_possible = 2 * u32::from(self.tiers[0].0) <= u32::from(LUT_BITS);
        while pairs_possible && px + 4 <= n_pixels && next_word < words.len() {
            let word = u64::from(u32::from_le_bytes(words[next_word]));
            let refill = acc_len <= 32;
            let shifted = word << (32u32.wrapping_sub(acc_len) & 63);
            acc |= if refill { shifted } else { 0 };
            acc_len += if refill { 32 } else { 0 };
            next_word += usize::from(refill);

            let entry = pair_lut[(acc >> (64 - u32::from(LUT_BITS))) as usize];
            let first_len = entry >> 24;
            if first_len == PAIR_MISS {
                match self.search_tiers(acc, u32::from(LUT_BITS) + 1, max_len) {
                    TierHit::Code(sym, len) => {
                        out[px] = sym;
                        acc <<= len;
                        acc_len -= len;
                        px += 1;
                        continue;
                    }
                    _ => {
                        return Err(Error::HuffmanDecodeFailure {
                            bit_position: next_word * 32 - acc_len as usize,
                        });
                    }
                }
            }
            let total = (entry >> 16) & 0xff;
            // The second store is harmless when the entry holds one
            // symbol: the slot is rewritten by the next probe.
            out[px] = entry as u8;
            out[px + 1] = (entry >> 8) as u8;
            px += 1 + usize::from(total != first_len);
            acc <<= total;
            acc_len -= total;

            // Second probe on the same refill (at least 21 bits left).
            let entry = pair_lut[(acc >> (64 - u32::from(LUT_BITS))) as usize];
            let first_len = entry >> 24;
            if first_len == PAIR_MISS {
                continue; // a long code: refill first, resolve at the top
            }
            let total = (entry >> 16) & 0xff;
            out[px] = entry as u8;
            out[px + 1] = (entry >> 8) as u8;
            px += 1 + usize::from(total != first_len);
            acc <<= total;
            acc_len -= total;
        }

        // Remainder: at most one symbol, or whatever the last (up to
        // two) words hold. Careful refill, then the same one-symbol
        // probe; once the data is exhausted and fewer than `max_len`
        // bits remain, the length-by-length walk.
        'refill: while px < n_pixels {
            while acc_len <= 32 {
                if let Some(w) = words.get(next_word) {
                    acc |= u64::from(u32::from_le_bytes(*w)) << (32 - acc_len);
                    acc_len += 32;
                    next_word += 1;
                } else if partial_pending {
                    let mut buf = [0u8; 4];
                    buf[..rest.len()].copy_from_slice(rest);
                    let w = u32::from_le_bytes(buf) & (u32::MAX << (32 - partial_bits));
                    acc |= u64::from(w) << (32 - acc_len);
                    acc_len += partial_bits;
                    partial_loaded = partial_bits;
                    partial_pending = false;
                } else {
                    break;
                }
            }
            if acc_len < max_len {
                break; // data exhausted: finish below
            }
            while acc_len >= max_len {
                let entry = lut[(acc >> (64 - u32::from(LUT_BITS))) as usize];
                let len = if entry.len <= LUT_BITS {
                    out[px] = entry.sym;
                    u32::from(entry.len)
                } else {
                    match self.search_tiers(acc, u32::from(LUT_BITS) + 1, max_len) {
                        TierHit::Code(sym, len) => {
                            out[px] = sym;
                            len
                        }
                        _ => {
                            return Err(Error::HuffmanDecodeFailure {
                                bit_position: next_word * 32 + partial_loaded as usize
                                    - acc_len as usize,
                            });
                        }
                    }
                };
                acc <<= len;
                acc_len -= len;
                px += 1;
                if px == n_pixels {
                    break 'refill;
                }
            }
        }

        // End of data with fewer than `max_len` bits left.
        while px < n_pixels {
            let bit_position = next_word * 32 + partial_loaded as usize - acc_len as usize;
            match self.search_tiers(acc, 1, acc_len) {
                TierHit::Code(sym, len) => {
                    out[px] = sym;
                    acc <<= len;
                    acc_len -= len;
                    px += 1;
                }
                TierHit::NeedsMoreBits => {
                    return Err(Error::SliceTruncated {
                        bit_position,
                        expected_pixels: n_pixels,
                        decoded: px,
                    });
                }
                TierHit::NoCode => return Err(Error::HuffmanDecodeFailure { bit_position }),
            }
        }
        Ok(next_word * 32 + partial_loaded as usize - acc_len as usize)
    }

    /// Match the left-aligned bits of `acc` against the codes whose
    /// length lies in `min_len..=avail`, shortest first. A tier longer
    /// than `avail` reports [`TierHit::NeedsMoreBits`]; running out of
    /// tiers reports [`TierHit::NoCode`].
    #[inline]
    fn search_tiers(&self, acc: u64, min_len: u32, avail: u32) -> TierHit {
        for &(tier_len, start, end) in &self.tiers {
            let tier_len = u32::from(tier_len);
            if tier_len < min_len {
                continue;
            }
            if tier_len > avail {
                return TierHit::NeedsMoreBits;
            }
            let candidate = (acc >> (64 - tier_len)) as u32;
            let tier = &self.by_length[start as usize..end as usize];
            if let Ok(i) = tier.binary_search_by(|probe| probe.1.cmp(&candidate)) {
                return TierHit::Code(tier[i].2, tier_len);
            }
        }
        TierHit::NoCode
    }

    /// Slice decode for a table holding codes longer than
    /// [`WIDE_CODE_BITS`]: one LUT probe per symbol, falling back to the
    /// bit-serial walk for a miss or near the slice end.
    fn decode_slice_serial(
        &self,
        serial: &SerialCode,
        slice_data: &[u8],
        out: &mut [u8],
    ) -> Result<usize> {
        let n_pixels = out.len();
        let mut br = BitReader::new(slice_data);
        let lut_bits = LUT_BITS as usize;
        for (px, slot) in out.iter_mut().enumerate() {
            if br.has_bits(lut_bits) {
                let entry = self.lut[br.peek_bits(lut_bits) as usize];
                if entry.len <= LUT_BITS {
                    *slot = entry.sym;
                    br.consume_bits(entry.len as usize);
                    continue;
                }
            }
            let bp_start = br.position();
            match serial.decode_one(&mut br) {
                Some(sym) => *slot = sym,
                None => {
                    return Err(Error::SliceTruncated {
                        bit_position: bp_start,
                        expected_pixels: n_pixels,
                        decoded: px,
                    });
                }
            }
        }
        Ok(br.position())
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
/// Marker in the first-length field of a [`build_pair_lut`] entry: the
/// first code at this index is longer than `LUT_BITS`.
const PAIR_MISS: u32 = 0xff;

/// Derive the two-symbol table from the one-symbol table.
///
/// Entry layout, low byte first: first symbol, second symbol, total
/// bits consumed, first code's length (or [`PAIR_MISS`]). When a second
/// complete code fits in the `LUT_BITS - first_len` index bits that
/// follow the first code, the entry carries both and `total` is the sum
/// of the two lengths; otherwise `total == first_len` and the second
/// symbol byte is unspecified. One probe then yields one or two
/// symbols, which shortens the per-symbol serial dependency (index →
/// load → shift) for the short codes that dominate real pictures.
fn build_pair_lut(lut: &[LutEntry]) -> Vec<u32> {
    let mask = lut.len() - 1;
    lut.iter()
        .enumerate()
        .map(|(key, first)| {
            if first.len > LUT_BITS {
                return PAIR_MISS << 24;
            }
            let first_len = u32::from(first.len);
            let second = lut[(key << first.len) & mask];
            let room = u32::from(LUT_BITS) - first_len;
            let (second_sym, total) = if u32::from(second.len) <= room {
                (u32::from(second.sym), first_len + u32::from(second.len))
            } else {
                (0, first_len)
            };
            u32::from(first.sym) | (second_sym << 8) | (total << 16) | (first_len << 24)
        })
        .collect()
}

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

/// Kraft equality (`spec/02` §4) for code lengths anywhere in
/// `1..=254`: fold the length histogram from the longest tier upwards.
/// A complete code has an even number of nodes at every depth below the
/// root (each pair merges into one node a level up) and exactly one
/// node at depth 0. Counts stay below 512, so no wide arithmetic.
fn kraft_check(items: &[(u8, u8)]) -> Result<()> {
    let mut tier = [0u32; 256];
    for &(l, _) in items {
        if l == 0 || l == 255 {
            return Err(Error::KraftViolation);
        }
        tier[l as usize] += 1;
    }
    let mut nodes = 0u32;
    for len in (1..=254usize).rev() {
        nodes += tier[len];
        if !nodes.is_multiple_of(2) {
            return Err(Error::KraftViolation);
        }
        nodes /= 2;
    }
    if nodes != 1 {
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

    /// Total bits in the backing buffer (`data.len() * 8`).
    #[inline]
    pub fn total_bits(&self) -> usize {
        self.total_bits
    }

    /// Scan the bits in `[self.pos, self.total_bits)` and return the
    /// absolute bit offset of the first set (non-zero) bit, or `None`
    /// if every remaining bit is zero. Used by the strict decode path
    /// to validate the trailing word-boundary padding is zero per
    /// `spec/05` §4.3. Does not advance the read cursor.
    ///
    /// Bits are scanned in the same MSB-first-within-LE-word order the
    /// reader consumes, so a returned offset is directly comparable to
    /// [`position`](Self::position).
    pub fn first_nonzero_bit_from_here(&self) -> Option<usize> {
        let mut bp = self.pos;
        // Walk bit-by-bit at the boundary, then word-at-a-time once
        // word-aligned, to keep the common (all-zero, often <32 bits)
        // case cheap without a per-bit loop over whole zero words.
        while bp < self.total_bits && !bp.is_multiple_of(32) {
            if self.bit_at(bp) {
                return Some(bp);
            }
            bp += 1;
        }
        while bp + 32 <= self.total_bits {
            let word = self.load_word((bp / 32) * 4);
            if word != 0 {
                // Some bit in this word is set; find which (MSB-first).
                let lead = word.leading_zeros() as usize;
                return Some(bp + lead);
            }
            bp += 32;
        }
        while bp < self.total_bits {
            if self.bit_at(bp) {
                return Some(bp);
            }
            bp += 1;
        }
        None
    }

    /// Read a single bit at absolute offset `bit`, MSB-first within
    /// the enclosing 32-bit LE word. Caller ensures `bit < total_bits`.
    #[inline]
    fn bit_at(&self, bit: usize) -> bool {
        let word = self.load_word((bit / 32) * 4);
        let shift = 31 - (bit % 32);
        (word >> shift) & 1 != 0
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
        for chunk in bytes[4..].as_chunks::<4>().0 {
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

    // -----------------------------------------------------------------
    // Round 335 — trailing-padding scan + strict slice decode.
    // -----------------------------------------------------------------

    #[test]
    fn first_nonzero_bit_finds_msb_first_position() {
        // One LE word 0x0000_0001 = byte-stream [01, 00, 00, 00]. The
        // single set bit is value 0x00000001, the LSB of the word, which
        // is bit 31 in MSB-first reading order.
        let data = 1u32.to_le_bytes().to_vec();
        let br = BitReader::new(&data);
        assert_eq!(br.first_nonzero_bit_from_here(), Some(31));

        // A fully-zero word -> no set bit.
        let zeros = vec![0u8; 8];
        let brz = BitReader::new(&zeros);
        assert_eq!(brz.first_nonzero_bit_from_here(), None);

        // 0x8000_0000 -> the MSB of the word is bit 0.
        let top = 0x8000_0000u32.to_le_bytes().to_vec();
        let brt = BitReader::new(&top);
        assert_eq!(brt.first_nonzero_bit_from_here(), Some(0));
    }

    #[test]
    fn first_nonzero_bit_respects_cursor() {
        // Two words: word0 all zero, word1 = 0x0000_0001 (set bit at
        // absolute bit 63). After consuming the first 32 bits the scan
        // starts mid-stream and still locates bit 63.
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&1u32.to_le_bytes());
        let mut br = BitReader::new(&data);
        br.consume_bits(32);
        assert_eq!(br.first_nonzero_bit_from_here(), Some(63));
        // From the very start, same bit is found.
        let br2 = BitReader::new(&data);
        assert_eq!(br2.first_nonzero_bit_from_here(), Some(63));
    }

    #[test]
    fn decode_slice_strict_accepts_zero_padding() {
        // Two-symbol {0:1, 128:1} table; emit 18 codes = 18 bits, padded
        // to 32 with zeros by the writer (spec/05 §4.3). Strict decode
        // must accept and match the lenient decode.
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();
        let symbols: Vec<u8> = (0..18).map(|i| if i % 2 == 0 { 0 } else { 128 }).collect();
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (c, l) = t.code_for(s).unwrap();
            bw.write_code(c, l);
        }
        let bytes = bw.finish();
        assert_eq!(bytes.len(), 4, "18 bits pad to one 32-bit word");
        let strict = t.decode_slice_strict(&bytes, symbols.len(), 0, 0).unwrap();
        assert_eq!(strict, symbols);
    }

    #[test]
    fn decode_slice_strict_rejects_nonzero_padding() {
        let d = make_descriptor(&[(0, 1), (128, 1)]);
        let t = HuffmanTable::build(&d).unwrap();
        let symbols: Vec<u8> = (0..18).map(|i| if i % 2 == 0 { 0 } else { 128 }).collect();
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (c, l) = t.code_for(s).unwrap();
            bw.write_code(c, l);
        }
        let mut bytes = bw.finish();
        // Set the lowest-value bit of the word (absolute bit 31, well
        // past the 18 payload bits) -> a non-zero padding bit.
        bytes[0] |= 0x01;
        // Lenient still decodes (ignores the tail).
        assert_eq!(t.decode_slice(&bytes, symbols.len()).unwrap(), symbols);
        // Strict rejects with the location fields populated.
        match t.decode_slice_strict(&bytes, symbols.len(), 2, 5) {
            Err(Error::NonZeroPadding {
                plane,
                slice,
                bit_position,
            }) => {
                assert_eq!(plane, 2);
                assert_eq!(slice, 5);
                assert_eq!(bit_position, 31);
            }
            other => panic!("expected NonZeroPadding, got {other:?}"),
        }
    }
}
