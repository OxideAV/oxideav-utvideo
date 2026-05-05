//! Per-plane canonical Huffman, with the Ut Video orientation.
//!
//! The on-disk representation is just a 256-byte (8-bit alphabet) array
//! of code lengths indexed by symbol value 0..=255. Per trace report
//! §4.6 / §4.7:
//!
//! * length = 0 → "the entire plane is filled with this symbol"
//!   (encoder fast path; the decoder skips the bitstream and emits
//!   `width × height` copies of the symbol).
//! * length = 0xFF → "symbol absent". Treated as length 0 in the code
//!   builder (excluded from the tree).
//! * length 1..=32 → valid bit-width.
//!
//! Canonical-Huffman conventions differ between codecs in two
//! independent dimensions: which way bits go down the tree (`0` left
//! vs `1` left) and which end of the (length, symbol) order gets the
//! all-zeros code. Ut Video's convention (per the MultimediaWiki page,
//! independently confirmed against ffmpeg's decoder): **all-ones is the
//! shortest code, all-zeros is the longest**, with symbols within the
//! same length assigned in ascending symbol-index order.
//!
//! Concrete code-assignment recipe used here:
//!
//! 1. Collect `(symbol, length)` pairs with `1 ≤ length ≤ 32`.
//! 2. Sort by `(length asc, symbol asc)`.
//! 3. Walk a counter `code` starting at `(1 << max_length) - 1` (all
//!    ones), assign `codeword = code >> (max_length - length)` for the
//!    current symbol (the top `length` bits of `code`), then decrement
//!    `code` by `1 << (max_length - length)`.
//!
//! That way the first (shortest) symbol gets all-ones, each subsequent
//! same-length symbol decrements its codeword by 1, and a length step
//! shifts further into the longer-tail. The bitstream reader scans MSB
//! first — see [`BitReader`] — so a codeword is read most-significant
//! bit first and matched against `code`.

use oxideav_core::{Error, Result};

/// One leaf in the canonical-Huffman table. Public so it can appear in
/// the [`HuffTable::Tree`] variant — but treated as opaque by callers,
/// who only reach it through [`HuffTable::decode_symbol`].
#[derive(Debug, Clone, Copy)]
pub struct Code {
    /// MSB-aligned codeword padded to `max_length` bits.
    msb_aligned: u32,
    /// Code length in bits (1..=32).
    length: u8,
    /// Symbol value (0..=255 for the 8-bit alphabet).
    symbol: u16,
}

/// A built canonical-Huffman decoder for one 8-bit plane. Carries
/// either a real code-list or the single-symbol "fill" shortcut. The
/// real-tree variant is boxed because its 2 KiB by-symbol lookup
/// dwarfs the 1-byte fill case; keeping the enum cheap to move makes
/// per-plane / per-slice borrow patterns simpler.
#[derive(Debug, Clone)]
pub enum HuffTable {
    /// Length-0 entry detected — emit `symbol` for every output pixel
    /// without touching the bitstream reader.
    Fill { symbol: u8 },
    /// Real Huffman tree.
    Tree(Box<HuffTree>),
}

/// Heap payload for the real-Huffman variant of [`HuffTable`].
#[derive(Debug, Clone)]
pub struct HuffTree {
    /// Indexed by symbol (0..=255). `None` if absent.
    pub by_symbol: [Option<u32>; 256],
    /// Sorted by `(length asc, msb_aligned asc)` for the slow-path
    /// scan. (Fast path will be a 2048-entry direct table once we
    /// add it; the slow scan is correct and observably fast on the
    /// short codes we see in real streams — max length 13 in the
    /// trace corpus.)
    pub leaves: Vec<Code>,
    pub max_length: u8,
}

impl HuffTable {
    /// Build from a 256-byte length array.
    pub fn from_lengths(lens: &[u8; 256]) -> Result<HuffTable> {
        // Single-symbol fast path: a length-0 entry means "this symbol
        // fills the plane". The trace report (§9) confirms a 1024×768
        // ULRA single-symbol frame compresses to a 256-byte length
        // table + zero-byte slice data per plane.
        for (sym, &len) in lens.iter().enumerate() {
            if len == 0 {
                return Ok(HuffTable::Fill { symbol: sym as u8 });
            }
        }

        let mut entries: Vec<(u16, u8)> = Vec::with_capacity(64);
        for (sym, &len) in lens.iter().enumerate() {
            if len == 0xFF {
                continue; // absent
            }
            if len > 32 {
                return Err(Error::invalid(format!(
                    "Ut Video Huffman: length {len} > 32 for symbol {sym}"
                )));
            }
            entries.push((sym as u16, len));
        }
        if entries.is_empty() {
            return Err(Error::invalid(
                "Ut Video Huffman: no codes (every length is 0xFF)",
            ));
        }
        entries.sort_by_key(|&(sym, len)| (len, sym));
        let max_length = entries.last().unwrap().1;

        let mut by_symbol: [Option<u32>; 256] = [None; 256];
        let mut leaves: Vec<Code> = Vec::with_capacity(entries.len());
        // Counter walks down from "all-ones at width max_length".
        let mut code: u64 = (1u64 << max_length) - 1;
        for (sym, len) in entries {
            // Top `len` bits of `code`, MSB-aligned to a `max_length`-bit
            // window. We store the MSB-aligned value so the decoder can
            // peek `max_length` bits and compare-and-mask.
            let codeword = code >> (max_length - len);
            let msb_aligned = (codeword << (max_length - len)) as u32;
            by_symbol[sym as usize] = Some(codeword as u32);
            leaves.push(Code {
                msb_aligned,
                length: len,
                symbol: sym,
            });
            // Decrement `code` by one slot at this length.
            let step = 1u64 << (max_length - len);
            // Allow the very last step to underflow — it just means we
            // consumed the entire length-budget, which is the correct
            // termination condition for a complete Huffman tree.
            code = code.wrapping_sub(step);
        }
        // Sort leaves by length-then-codeword for deterministic match.
        leaves.sort_by_key(|c| (c.length, c.msb_aligned));
        Ok(HuffTable::Tree(Box::new(HuffTree {
            by_symbol,
            leaves,
            max_length,
        })))
    }

    /// Encode-side helper used only by tests: look up a symbol's
    /// codeword (without the MSB pad).
    #[cfg(test)]
    pub fn codeword_of(&self, sym: u8) -> Option<(u32, u8)> {
        match self {
            HuffTable::Fill { symbol } => {
                if *symbol == sym {
                    Some((0, 0))
                } else {
                    None
                }
            }
            HuffTable::Tree(tree) => {
                let cw = tree.by_symbol[sym as usize]?;
                let len = tree.leaves.iter().find(|l| l.symbol == sym as u16)?.length;
                Some((cw, len))
            }
        }
    }

    /// Decode the next symbol from `reader`. Returns the symbol value
    /// (0..=255). For [`HuffTable::Fill`] the bitstream is **not**
    /// consumed and the caller is expected to short-circuit before
    /// calling this.
    pub fn decode_symbol(&self, reader: &mut BitReader<'_>) -> Result<u8> {
        match self {
            HuffTable::Fill { symbol } => Ok(*symbol),
            HuffTable::Tree(tree) => {
                let leaves = &tree.leaves;
                let max = tree.max_length as u32;
                // Peek up to `max` bits, MSB-first. If the buffer runs
                // out we still get whatever's left (zero-padded on the
                // tail), which is fine — a well-formed slice ends on a
                // codeword boundary, padded to a 32-bit word.
                let peeked = reader.peek_bits(max)?;
                // Linear scan over leaves. At a max length of 13 this
                // is well under the constant-factor of a 2-stage table
                // for the per-frame symbol counts in our corpus
                // (tens of distinct symbols per plane).
                for leaf in leaves {
                    let mask = if leaf.length == 32 {
                        0xFFFF_FFFF
                    } else {
                        // High `length` bits of a `max_length`-bit window.
                        ((1u32 << leaf.length) - 1) << (max as u8 - leaf.length)
                    };
                    if (peeked & mask) == leaf.msb_aligned {
                        reader.consume(leaf.length as u32)?;
                        return Ok(leaf.symbol as u8);
                    }
                }
                Err(Error::invalid(
                    "Ut Video Huffman: no matching codeword (corrupt slice?)",
                ))
            }
        }
    }
}

/// MSB-first bit reader over a buffer that's already been word-swapped
/// into a stream of `LE32` words. The trace report (§4.8) describes the
/// VfW-era quirk: on-disk slice bytes are read 4 at a time, byte-swapped
/// inside each group, then the resulting buffer is parsed MSB-first.
pub struct BitReader<'a> {
    /// Byte-swapped scratch buffer. All bit reads come from this view.
    buf: &'a [u8],
    /// Next byte to consume.
    byte_pos: usize,
    /// Bit cache — top `bits_in_cache` bits are valid (MSB-aligned).
    cache: u64,
    bits_in_cache: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(swapped: &'a [u8]) -> BitReader<'a> {
        BitReader {
            buf: swapped,
            byte_pos: 0,
            cache: 0,
            bits_in_cache: 0,
        }
    }

    /// Peek `n` bits (1..=32) MSB-first without consuming. Result is
    /// right-aligned in a `u32`.
    pub fn peek_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        self.refill(n)?;
        Ok(((self.cache >> (64 - n)) & ((1u64 << n) - 1)) as u32)
    }

    /// Consume `n` bits previously peeked. Cheap — does not touch the
    /// underlying buffer beyond updating the cache pointer.
    pub fn consume(&mut self, n: u32) -> Result<()> {
        debug_assert!(n <= 32);
        if self.bits_in_cache < n {
            // Need to refill before consuming.
            self.refill(n)?;
        }
        self.cache = self.cache.wrapping_shl(n);
        self.bits_in_cache -= n;
        Ok(())
    }

    fn refill(&mut self, want: u32) -> Result<()> {
        while self.bits_in_cache < want {
            if self.byte_pos >= self.buf.len() {
                if self.bits_in_cache == 0 && want > 0 {
                    // Allow zero-padded tail reads on the last symbol of a
                    // slice (the encoder pads to a 32-bit word). The
                    // returned bits will be zero; the canonical-Huffman
                    // matcher will fail noisily if the codeword wasn't
                    // actually emitted.
                    return Ok(());
                }
                return Ok(());
            }
            let byte = self.buf[self.byte_pos] as u64;
            self.byte_pos += 1;
            // Place the byte at the MSB end of the still-empty slot.
            let shift = 64 - 8 - self.bits_in_cache;
            self.cache |= byte << shift;
            self.bits_in_cache += 8;
        }
        Ok(())
    }
}

/// Word-swap slice data from on-disk order into the `LE32`-word view
/// the bit reader expects. Concretely: for each 4-byte group, reverse
/// the 4 bytes. The tail (a partial group) is left as-is — the trace
/// report is silent on incomplete tails, and our test fixtures always
/// end on a 4-byte boundary.
pub fn byteswap_dwords(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut chunks = input.chunks_exact(4);
    for c in &mut chunks {
        out.push(c[3]);
        out.push(c[2]);
        out.push(c[1]);
        out.push(c[0]);
    }
    out.extend_from_slice(chunks.remainder());
    out
}

/// A built canonical-Huffman decoder for a 10-bit (1024-symbol) plane.
/// Same orientation as `HuffTable` (all-ones shortest), over the 10-bit
/// alphabet 0..=1023.
#[derive(Debug, Clone)]
pub enum HuffTable10 {
    /// Length-0 entry detected — emit `symbol` for every output sample.
    Fill { symbol: u16 },
    /// Real Huffman tree over the 10-bit alphabet.
    Tree(Box<HuffTree10>),
}

/// Heap payload for the 10-bit real-Huffman variant.
#[derive(Debug, Clone)]
pub struct HuffTree10 {
    /// Leaves sorted by `(length asc, msb_aligned asc)`.
    pub leaves: Vec<Code10>,
    pub max_length: u8,
}

/// One leaf in the 10-bit canonical-Huffman table.
#[derive(Debug, Clone, Copy)]
pub struct Code10 {
    pub msb_aligned: u32,
    pub length: u8,
    pub symbol: u16,
}

impl HuffTable10 {
    /// Build from a 1024-byte length array (one byte per 10-bit symbol).
    pub fn from_lengths_1024(lens: &[u8]) -> Result<HuffTable10> {
        debug_assert_eq!(lens.len(), 1024);
        // Single-symbol fill path.
        for (sym, &len) in lens.iter().enumerate() {
            if len == 0 {
                return Ok(HuffTable10::Fill {
                    symbol: sym as u16,
                });
            }
        }
        let mut entries: Vec<(u16, u8)> = Vec::with_capacity(128);
        for (sym, &len) in lens.iter().enumerate() {
            if len == 0xFF {
                continue;
            }
            if len > 32 {
                return Err(Error::invalid(format!(
                    "Ut Video 10-bit Huffman: length {len} > 32 for symbol {sym}"
                )));
            }
            entries.push((sym as u16, len));
        }
        if entries.is_empty() {
            return Err(Error::invalid(
                "Ut Video 10-bit Huffman: no codes (every length is 0xFF)",
            ));
        }
        entries.sort_by_key(|&(sym, len)| (len, sym));
        let max_length = entries.last().unwrap().1;

        let mut leaves: Vec<Code10> = Vec::with_capacity(entries.len());
        let mut code: u64 = (1u64 << max_length) - 1;
        for (sym, len) in entries {
            let codeword = code >> (max_length - len);
            let msb_aligned = (codeword << (max_length - len)) as u32;
            leaves.push(Code10 {
                msb_aligned,
                length: len,
                symbol: sym,
            });
            let step = 1u64 << (max_length - len);
            code = code.wrapping_sub(step);
        }
        leaves.sort_by_key(|c| (c.length, c.msb_aligned));
        Ok(HuffTable10::Tree(Box::new(HuffTree10 {
            leaves,
            max_length,
        })))
    }

    /// Decode the next 10-bit symbol from `reader`. For `Fill` the
    /// bitstream is not consumed.
    pub fn decode_symbol(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        match self {
            HuffTable10::Fill { symbol } => Ok(*symbol),
            HuffTable10::Tree(tree) => {
                let leaves = &tree.leaves;
                let max = tree.max_length as u32;
                let peeked = reader.peek_bits(max)?;
                for leaf in leaves {
                    let mask = if leaf.length == 32 {
                        0xFFFF_FFFF
                    } else {
                        ((1u32 << leaf.length) - 1) << (max as u8 - leaf.length)
                    };
                    if (peeked & mask) == leaf.msb_aligned {
                        reader.consume(leaf.length as u32)?;
                        return Ok(leaf.symbol);
                    }
                }
                Err(Error::invalid(
                    "Ut Video 10-bit Huffman: no matching codeword (corrupt slice?)",
                ))
            }
        }
    }
}

/// LE bit reader for the SymPack family (trace doc §7 / §12.5).
///
/// Unlike the classic/pro family which byte-swaps by 4 and reads MSB-first,
/// SymPack uses `init_get_bits8_le` — natural byte order, LE bit packing.
/// Bits are consumed LSB-first from each byte.
pub struct LeBitReader<'a> {
    buf: &'a [u8],
    byte_pos: usize,
    cache: u64,
    bits_in_cache: u32,
}

impl<'a> LeBitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        LeBitReader {
            buf,
            byte_pos: 0,
            cache: 0,
            bits_in_cache: 0,
        }
    }

    /// Read `n` bits (1..=32) LSB-first. Result is right-aligned.
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        self.refill(n)?;
        let val = (self.cache & ((1u64 << n) - 1)) as u32;
        self.cache >>= n;
        self.bits_in_cache = self.bits_in_cache.saturating_sub(n);
        Ok(val)
    }

    fn refill(&mut self, want: u32) -> Result<()> {
        while self.bits_in_cache < want {
            if self.byte_pos >= self.buf.len() {
                return Ok(()); // zero-pad the tail
            }
            let byte = self.buf[self.byte_pos] as u64;
            self.byte_pos += 1;
            self.cache |= byte << self.bits_in_cache;
            self.bits_in_cache += 8;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_table_short_circuits() {
        let mut lens = [0xFFu8; 256];
        lens[42] = 0;
        let t = HuffTable::from_lengths(&lens).unwrap();
        let mut br = BitReader::new(&[]);
        assert_eq!(t.decode_symbol(&mut br).unwrap(), 42);
    }

    #[test]
    fn two_symbol_tree_orientation() {
        // Two symbols, both length 1.
        // Convention (all-ones first): codeword for the lower-indexed
        // symbol = 0b1, higher-indexed = 0b0.
        let mut lens = [0xFFu8; 256];
        lens[10] = 1;
        lens[200] = 1;
        let t = HuffTable::from_lengths(&lens).unwrap();
        let (cw_a, len_a) = t.codeword_of(10).unwrap();
        let (cw_b, len_b) = t.codeword_of(200).unwrap();
        assert_eq!(len_a, 1);
        assert_eq!(len_b, 1);
        assert_eq!(cw_a, 1, "first sorted-by-symbol gets the all-ones code");
        assert_eq!(cw_b, 0);
    }

    #[test]
    fn round_trip_canonical() {
        // Lengths 1, 2, 2 → codewords 1, 01, 00.
        let mut lens = [0xFFu8; 256];
        lens[1] = 1;
        lens[2] = 2;
        lens[3] = 2;
        let t = HuffTable::from_lengths(&lens).unwrap();
        // Build a bitstream with symbols 1, 2, 3 in order: bits
        // "1" "01" "00" = 10100  (5 bits, MSB first inside one byte).
        // Bypass the word-swap: feed the bytes that the bit reader
        // would have seen *after* swap. Take a 4-byte word laid out
        // MSB-first.
        let buf = [0b1010_0000u8, 0u8, 0u8, 0u8];
        let mut br = BitReader::new(&buf);
        assert_eq!(t.decode_symbol(&mut br).unwrap(), 1);
        assert_eq!(t.decode_symbol(&mut br).unwrap(), 2);
        assert_eq!(t.decode_symbol(&mut br).unwrap(), 3);
    }

    #[test]
    fn byteswap_groups_of_four() {
        let inp = [0u8, 1, 2, 3, 4, 5, 6, 7, 8];
        let out = byteswap_dwords(&inp);
        assert_eq!(out, vec![3, 2, 1, 0, 7, 6, 5, 4, 8]);
    }
}
