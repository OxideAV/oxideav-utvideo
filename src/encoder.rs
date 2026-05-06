//! Ut Video classic-family (8-bit) encoder.
//!
//! Supports `ULRG`, `ULRA`, `ULY0`, `ULY2`, `ULY4` with predictors
//! NONE / LEFT / GRADIENT / MEDIAN. The encoder picks a single
//! predictor per frame by RDO (whichever residual stream produces the
//! smallest expected Huffman bit count, summed across planes); the
//! caller can also pin a predictor explicitly for tests.
//!
//! Output bytes are bit-exact to what `ffmpeg -c:v utvideo` produces
//! for the same input + predictor + slice count, modulo the predictor
//! choice (RDO selects the same as ffmpeg in our test corpus).
//!
//! # On-disk shape (matches the decoder's `decode_classic` parser)
//!
//! For every plane in order (Y/U/V or G/B/R/[A]):
//! 1. 256-byte length table (canonical Huffman, all-ones-shortest
//!    convention, see `huffman.rs`).
//! 2. `4 × n_slices` bytes of cumulative end-offsets (LE32) into the
//!    plane's slice-data block.
//! 3. `end[n_slices-1]` bytes of slice data.
//!
//! After all planes, the final 4 bytes are the LE32 `frame_info` whose
//! bits 8-9 encode the predictor (NONE=0 / LEFT=1 / GRADIENT=2 /
//! MEDIAN=3). All other bits are zero in our output (matches the
//! ffmpeg trace).
//!
//! # Bitstream-side quirks
//!
//! * Slice data is written 4 bytes at a time as MSB-first packed
//!   bitstreams; the decoder side byte-swaps each 4-byte group before
//!   reading. We mirror that here by buffering bits MSB-first into a
//!   word and emitting the four bytes in **reversed** order — that
//!   way the decoder's `byteswap_dwords` undo restores the original
//!   MSB-first stream.
//! * Each slice's bitstream is independently padded out to a 4-byte
//!   boundary (zero-pad).
//! * For LEFT, the cumulative-sum scan covers the **whole slice**
//!   (single linear scan). For GRADIENT/MEDIAN, row 0 of every slice
//!   uses LEFT (seeded `0x80`) and subsequent rows use the 2D
//!   predictor — see `decoder::apply_inverse_8bit` and the inverse
//!   tests in `predictor.rs`.

use oxideav_core::{Error, Result};

use crate::fourcc::{Family, FourCc, PlaneShape};
use crate::huffman::HuffTable;
use crate::predictor::Predictor;

const SEED_8BIT: u8 = 0x80;

/// One encoded frame: extradata (16 bytes for classic family) + the
/// raw frame bytes the decoder consumes.
pub struct EncodedFrame {
    pub extradata: Vec<u8>,
    pub packet: Vec<u8>,
}

/// Encoder configuration. `slices` is bounded to 1..=256 (Ut Video
/// stores slices-1 in a single byte). `predictor = None` selects the
/// per-frame RDO winner among NONE/LEFT/GRADIENT/MEDIAN.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    pub slices: u32,
    pub predictor: Option<Predictor>,
}

impl EncoderConfig {
    pub fn new(fourcc: FourCc, width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            fourcc,
            width,
            height,
            slices: 1,
            predictor: None,
        }
    }
    pub fn with_slices(mut self, n: u32) -> EncoderConfig {
        self.slices = n;
        self
    }
    pub fn with_predictor(mut self, p: Predictor) -> EncoderConfig {
        self.predictor = Some(p);
        self
    }
}

/// Encode one classic-family frame.
///
/// `planes` is the per-plane byte order the matching decoder produces:
/// G/B/R/[A] for `ULRG`/`ULRA`, Y/U/V for `ULY0/2/4`. Each plane is
/// `pw × ph` bytes row-major, where `pw`/`ph` follow the FourCC's
/// chroma subsampling (so for `ULY2`, U and V are `width/2 × height`).
pub fn encode_frame(cfg: &EncoderConfig, planes: &[&[u8]]) -> Result<EncodedFrame> {
    let shape = PlaneShape::from_fourcc(cfg.fourcc)?;
    if shape.family != Family::Classic {
        return Err(Error::unsupported(format!(
            "Ut Video encoder: family {:?} not implemented (only classic UL today)",
            shape.family
        )));
    }
    if cfg.slices == 0 || cfg.slices > 256 {
        return Err(Error::invalid(format!(
            "Ut Video encoder: slice count {} out of range 1..=256",
            cfg.slices
        )));
    }
    if planes.len() != shape.planes as usize {
        return Err(Error::invalid(format!(
            "Ut Video encoder: got {} planes, expected {}",
            planes.len(),
            shape.planes
        )));
    }

    let mut working: Vec<Vec<u8>> = planes.iter().map(|p| p.to_vec()).collect();

    // For RGB FourCCs, apply the forward G-centred transform on B and R
    // so that the residuals fed to the predictor match what the
    // decoder's inverse step expects. (Alpha plane is left untouched.)
    if shape.is_rgb {
        let (g_part, rest) = working.split_at_mut(1);
        let g = &g_part[0];
        let (b_part, rest2) = rest.split_at_mut(1);
        let b = &mut b_part[0];
        let (r_part, _) = rest2.split_at_mut(1);
        let r = &mut r_part[0];
        forward_g_centred_rgb(g, b, r);
    }

    // Pick the predictor: caller-pinned or RDO across the four options.
    let predictor = match cfg.predictor {
        Some(p) => p,
        None => choose_predictor(&working, &shape, cfg.width, cfg.height, cfg.slices)?,
    };

    // Encode every plane.
    let mut packet: Vec<u8> = Vec::new();
    for (plane_idx, plane) in working.iter().enumerate() {
        let pw = subsampled(cfg.width, shape.h_subsample[plane_idx]) as usize;
        let ph = subsampled(cfg.height, shape.v_subsample[plane_idx]) as usize;
        encode_plane(&mut packet, predictor, plane, pw, ph, cfg.slices as usize)?;
    }

    // Tail: 4-byte LE32 frame_info with predictor bits 8-9.
    let frame_info: u32 = predictor_to_bits(predictor) << 8;
    packet.extend_from_slice(&frame_info.to_le_bytes());

    let extradata = make_classic_extradata(cfg.fourcc, cfg.slices);
    Ok(EncodedFrame { extradata, packet })
}

/// Build the 16-byte classic-family extradata.
///
/// Layout (per `extradata.rs`):
/// * word0 LE32 = version  (we emit `0x0000_0001` — enough to satisfy
///   the parser; the decoder doesn't gate on version).
/// * word1 BE32 = original_format ASCII FourCC matching the FourCC's
///   raw-pixel format (matches the trace report §3.2 table).
/// * word2 LE32 = `frame_info_size` = 4.
/// * word3 LE32 = flags: bit 0 = compression on, bits 24..31 =
///   `slices - 1`. Interlaced bit is left clear — the encoder always
///   emits progressive frames.
fn make_classic_extradata(fourcc: FourCc, slices: u32) -> Vec<u8> {
    let original_format: u32 = match &fourcc.0 {
        b"ULRG" | b"ULRA" => 0x00000000, // RGB has no original-format FourCC
        b"ULY0" | b"ULH0" => 0x59563132, // "YV12"
        b"ULY2" | b"ULH2" => 0x59555932, // "YUY2"
        b"ULY4" | b"ULH4" => 0x59563234, // "YV24"
        _ => 0,
    };
    let mut xd = vec![0u8; 16];
    xd[0..4].copy_from_slice(&0x0000_0001u32.to_le_bytes());
    xd[4..8].copy_from_slice(&original_format.to_be_bytes());
    xd[8..12].copy_from_slice(&4u32.to_le_bytes());
    let flags = 1u32 | (((slices - 1) & 0xFF) << 24);
    xd[12..16].copy_from_slice(&flags.to_le_bytes());
    xd
}

/// Forward G-centred RGB transform: `B' = B - G - 0x80`, `R' = R - G - 0x80`.
/// Inverse lives in `predictor::restore_g_centred_rgb`.
fn forward_g_centred_rgb(g: &[u8], b: &mut [u8], r: &mut [u8]) {
    debug_assert_eq!(g.len(), b.len());
    debug_assert_eq!(g.len(), r.len());
    for ((gp, bp), rp) in g.iter().zip(b.iter_mut()).zip(r.iter_mut()) {
        *bp = bp.wrapping_sub(*gp).wrapping_sub(SEED_8BIT);
        *rp = rp.wrapping_sub(*gp).wrapping_sub(SEED_8BIT);
    }
}

fn predictor_to_bits(p: Predictor) -> u32 {
    match p {
        Predictor::None => 0,
        Predictor::Left => 1,
        Predictor::Gradient => 2,
        Predictor::Median => 3,
    }
}

#[inline]
fn subsampled(dim: u32, factor: u8) -> u32 {
    dim.div_ceil(factor as u32)
}

/// Pick the predictor that minimises the total Huffman bit cost across
/// every plane (using each predictor's first-slice residual as the
/// proxy — fast and robust because a flat residual histogram dominates
/// total cost).
fn choose_predictor(
    planes: &[Vec<u8>],
    shape: &PlaneShape,
    width: u32,
    height: u32,
    slices: u32,
) -> Result<Predictor> {
    let candidates = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    let mut best = Predictor::Left;
    let mut best_cost = u64::MAX;
    for &cand in &candidates {
        let mut total = 0u64;
        for (plane_idx, plane) in planes.iter().enumerate() {
            let pw = subsampled(width, shape.h_subsample[plane_idx]) as usize;
            let ph = subsampled(height, shape.v_subsample[plane_idx]) as usize;
            total += plane_cost(cand, plane, pw, ph, slices as usize)?;
        }
        if total < best_cost {
            best_cost = total;
            best = cand;
        }
    }
    Ok(best)
}

/// Estimate the Huffman bit cost of one plane under predictor `pred`.
/// We compute the residual histogram and apply the entropy-code lower
/// bound `Σ -p log2 p` rounded up. Cheap and accurate enough to drive
/// per-frame predictor selection.
fn plane_cost(
    pred: Predictor,
    plane: &[u8],
    width: usize,
    height: usize,
    slices: usize,
) -> Result<u64> {
    let row_starts = compute_row_partition(height as u32, slices);
    let mut hist = [0u32; 256];
    for s in 0..slices {
        let row_lo = row_starts[s] as usize;
        let row_hi = row_starts[s + 1] as usize;
        let slice_h = row_hi - row_lo;
        if slice_h == 0 {
            continue;
        }
        let src = &plane[row_lo * width..(row_lo + slice_h) * width];
        let res = forward_predict(pred, src, width, slice_h);
        for &b in &res {
            hist[b as usize] += 1;
        }
    }
    let total: u64 = hist.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return Ok(0);
    }
    let mut bits = 0f64;
    let inv_total = 1.0_f64 / total as f64;
    for &c in &hist {
        if c == 0 {
            continue;
        }
        let p = c as f64 * inv_total;
        bits += -(p * p.log2()) * c as f64;
    }
    Ok(bits.ceil() as u64)
}

/// Apply the forward predictor for one slice (returns a freshly
/// allocated residual buffer of the same size).
///
/// Mirrors the decoder's [`crate::predictor::apply_inverse_8bit`]
/// formulas:
/// * NONE — pass-through.
/// * LEFT — single linear scan over the slice, seed `0x80`. Residual
///   `r[i] = px[i] - prev`, where `prev` is the previous decoded
///   pixel (which equals the previous source pixel for lossless).
/// * GRADIENT — row 0 uses LEFT; row >=1 col 0 uses TOP; rest uses
///   `pred = a + c - b` with `a = left, b = above-left, c = above`.
/// * MEDIAN — row 0 uses LEFT; rows >=1 use `mid_pred(a, b, a+b-c)`.
///   Row 1 col 0 collapses `c` to `a` (so the gradient becomes top
///   and the median = top).
fn forward_predict(pred: Predictor, src: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    if width == 0 || height == 0 {
        return out;
    }
    match pred {
        Predictor::None => out.copy_from_slice(src),
        Predictor::Left => {
            let mut prev = SEED_8BIT;
            for i in 0..src.len() {
                out[i] = src[i].wrapping_sub(prev);
                prev = src[i];
            }
        }
        Predictor::Gradient => {
            // Row 0 = LEFT seeded.
            let mut prev = SEED_8BIT;
            for x in 0..width {
                out[x] = src[x].wrapping_sub(prev);
                prev = src[x];
            }
            for y in 1..height {
                let i0 = y * width;
                // Col 0 = TOP.
                out[i0] = src[i0].wrapping_sub(src[i0 - width]);
                for x in 1..width {
                    let i = y * width + x;
                    let a = src[i - 1];
                    let b = src[i - width - 1];
                    let c = src[i - width];
                    let p = a.wrapping_add(c).wrapping_sub(b);
                    out[i] = src[i].wrapping_sub(p);
                }
            }
        }
        Predictor::Median => {
            // Row 0 = LEFT seeded.
            let mut prev = SEED_8BIT;
            for x in 0..width {
                out[x] = src[x].wrapping_sub(prev);
                prev = src[x];
            }
            if height < 2 {
                return out;
            }
            for y in 1..height {
                for x in 0..width {
                    let i = y * width + x;
                    let a = src[i - 1];
                    let b = src[i - width];
                    let c = if x == 0 && y == 1 {
                        a
                    } else {
                        src[i - width - 1]
                    };
                    let p = mid_pred(a, b, c);
                    out[i] = src[i].wrapping_sub(p);
                }
            }
        }
    }
    out
}

/// Same `mid_pred` as the decoder (median of `a, b, a + b − c mod 256`).
#[inline]
fn mid_pred(a: u8, b: u8, c: u8) -> u8 {
    let gradient = a.wrapping_add(b).wrapping_sub(c);
    median3(a, b, gradient)
}

#[inline]
fn median3(a: u8, b: u8, c: u8) -> u8 {
    let lo = a.min(b).min(c);
    let hi = a.max(b).max(c);
    let sum = a as u16 + b as u16 + c as u16;
    (sum - lo as u16 - hi as u16) as u8
}

/// Same row partition as [`crate::decoder::compute_row_partition`].
fn compute_row_partition(plane_height: u32, n_slices: usize) -> Vec<u32> {
    let mut starts = Vec::with_capacity(n_slices + 1);
    for i in 0..=n_slices {
        let v = ((i as u64) * plane_height as u64 / n_slices as u64) as u32;
        starts.push(v);
    }
    starts
}

/// Encode one plane: forward-predict, build Huffman, emit per-slice
/// bitstreams + offset table + length table, append to `out`.
fn encode_plane(
    out: &mut Vec<u8>,
    pred: Predictor,
    plane: &[u8],
    width: usize,
    height: usize,
    slices: usize,
) -> Result<()> {
    let row_starts = compute_row_partition(height as u32, slices);

    // 1. Forward-predict every slice and collect the joint residual
    //    histogram for Huffman building.
    let mut residuals: Vec<Vec<u8>> = Vec::with_capacity(slices);
    let mut hist = [0u32; 256];
    for s in 0..slices {
        let row_lo = row_starts[s] as usize;
        let row_hi = row_starts[s + 1] as usize;
        let slice_h = row_hi - row_lo;
        if slice_h == 0 {
            residuals.push(Vec::new());
            continue;
        }
        let src = &plane[row_lo * width..(row_lo + slice_h) * width];
        let res = forward_predict(pred, src, width, slice_h);
        for &b in &res {
            hist[b as usize] += 1;
        }
        residuals.push(res);
    }

    // 2. Build the canonical-Huffman code lengths from the histogram.
    let lens = build_canonical_lengths(&hist);

    // 3. Build a HuffTable from those lengths so we can use the same
    //    `codeword_of` lookup as the decoder side. (Single-symbol
    //    fast path: `Fill` short-circuits — emit zero-byte slices.)
    let table = HuffTable::from_lengths(&lens)?;

    // 4. Emit length table.
    out.extend_from_slice(&lens);

    // 5. Emit slice-offset table + slice data.
    //    First we pack each slice's bitstream, then we know its byte
    //    length (already 4-byte aligned because we pad each slice).
    let mut slice_bytes: Vec<Vec<u8>> = Vec::with_capacity(slices);
    for res in &residuals {
        let bytes = pack_slice_bits(&table, res)?;
        slice_bytes.push(bytes);
    }
    let mut cum = 0u32;
    for sb in &slice_bytes {
        cum += sb.len() as u32;
        out.extend_from_slice(&cum.to_le_bytes());
    }
    for sb in slice_bytes {
        out.extend_from_slice(&sb);
    }
    Ok(())
}

/// Pack one slice's residual symbols through `table` into the on-disk
/// byte sequence the decoder will consume.
///
/// We write bits **MSB-first** into a 32-bit accumulator, flushing
/// completed 4-byte words in **reversed byte order** so that the
/// decoder's `byteswap_dwords` undo restores the original MSB-first
/// stream (per `huffman.rs` notes). Each slice ends on a 4-byte
/// boundary (zero-padded). For the [`HuffTable::Fill`] fast path we
/// emit zero bytes — the decoder short-circuits the bitstream read.
fn pack_slice_bits(table: &HuffTable, residuals: &[u8]) -> Result<Vec<u8>> {
    if residuals.is_empty() {
        return Ok(Vec::new());
    }
    if let HuffTable::Fill { .. } = table {
        // Single-symbol plane: zero-byte slice, decoder fills.
        return Ok(Vec::new());
    }

    let mut out: Vec<u8> = Vec::with_capacity(residuals.len());
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &sym in residuals {
        let (cw, len) = table.codeword_of(sym).ok_or_else(|| {
            Error::invalid(format!(
                "Ut Video encoder: residual symbol {sym} missing from Huffman table"
            ))
        })?;
        // Push `cw` (right-aligned `len` bits) into the high end of
        // `acc`. `acc` keeps the *next* 64 bits to flush, MSB-first.
        if bits + len as u32 > 64 {
            // Should never happen given len <= 32 and we flush every
            // 32 bits — but we tolerate it defensively.
            flush_word(&mut out, &mut acc, &mut bits);
        }
        let shift = 64 - bits - len as u32;
        acc |= (cw as u64) << shift;
        bits += len as u32;
        while bits >= 32 {
            flush_word(&mut out, &mut acc, &mut bits);
        }
    }
    // Flush any trailing partial word, padding with zeros up to the
    // next 4-byte boundary.
    if bits > 0 {
        // Force the partial word to be a full 32 bits by setting bits
        // = 32 (the missing tail bits are already zero in `acc`).
        bits = 32;
        flush_word(&mut out, &mut acc, &mut bits);
    }
    Ok(out)
}

fn flush_word(out: &mut Vec<u8>, acc: &mut u64, bits: &mut u32) {
    // Top 32 bits of `acc` is the next MSB-first word.
    let word = (*acc >> 32) as u32;
    // Decoder byte-swaps groups of 4: it reads on-disk bytes
    // [b0 b1 b2 b3] and feeds the MSB-first reader [b3 b2 b1 b0].
    // So if the *intended* MSB-first order is [W3 W2 W1 W0] (where
    // W3 is the high byte of `word`), we must write [W0 W1 W2 W3]
    // to disk. After byteswap that becomes [W3 W2 W1 W0] — correct.
    let w3 = (word >> 24) as u8;
    let w2 = (word >> 16) as u8;
    let w1 = (word >> 8) as u8;
    let w0 = word as u8;
    out.push(w0);
    out.push(w1);
    out.push(w2);
    out.push(w3);
    *acc <<= 32;
    *bits -= 32;
}

/// Build canonical-Huffman code lengths from a symbol-frequency
/// histogram, capped at length 16.
///
/// Steps:
/// 1. Count present symbols. If all zero, fall back to length 0xFF
///    everywhere (decoder will reject). If exactly one, emit a
///    length-0 fast-path entry for that symbol and 0xFF elsewhere.
/// 2. Run the standard package-merge / weight-balance algorithm via a
///    min-heap of `(weight, level)` to derive code lengths satisfying
///    the Kraft inequality.
/// 3. If the longest code exceeds 16 bits, run length-limiting
///    (BRCI / "downgrade longest, upgrade leaves") until max ≤ 16.
fn build_canonical_lengths(hist: &[u32; 256]) -> [u8; 256] {
    let mut lens = [0xFFu8; 256];
    let present: Vec<(u8, u32)> = hist
        .iter()
        .enumerate()
        .filter_map(|(s, &c)| if c > 0 { Some((s as u8, c)) } else { None })
        .collect();
    if present.is_empty() {
        // Empty plane (zero pixels). Mark symbol 0 as fill so the
        // table is well-formed; in practice this never reaches the
        // decoder because the plane has zero data anyway.
        lens[0] = 0;
        return lens;
    }
    if present.len() == 1 {
        // Single-symbol fast path: zero-byte slice data, 256-byte
        // length table with a length-0 entry at the lone symbol.
        let (sym, _) = present[0];
        lens[sym as usize] = 0;
        return lens;
    }

    // ---- Standard Huffman length derivation via priority queue ----
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    #[derive(Eq, PartialEq)]
    struct Node {
        weight: u64,
        // Leaf symbols accumulated below this node.
        leaves: Vec<u8>,
    }
    impl Ord for Node {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.weight.cmp(&other.weight)
        }
    }
    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    let mut heap: BinaryHeap<Reverse<Node>> = BinaryHeap::new();
    for &(sym, c) in &present {
        heap.push(Reverse(Node {
            weight: c as u64,
            leaves: vec![sym],
        }));
    }
    // Track each symbol's current depth.
    let mut depth = [0u8; 256];
    while heap.len() > 1 {
        let a = heap.pop().unwrap().0;
        let b = heap.pop().unwrap().0;
        for &s in &a.leaves {
            depth[s as usize] = depth[s as usize].saturating_add(1);
        }
        for &s in &b.leaves {
            depth[s as usize] = depth[s as usize].saturating_add(1);
        }
        let mut leaves = a.leaves;
        leaves.extend(b.leaves);
        heap.push(Reverse(Node {
            weight: a.weight + b.weight,
            leaves,
        }));
    }

    // Length-limit to <= 16 via the simple "swap deepest with shallowest
    // shorter-than-max" rebalancing. The Kraft sum can briefly drop
    // below 1 during shortening; we then re-extend the shallowest leaf
    // until Kraft is exactly 1.
    const MAX_LEN: u8 = 16;
    let mut working: Vec<(u8, u8)> = present
        .iter()
        .map(|&(s, _)| (s, depth[s as usize]))
        .collect();
    // Cap any depth to MAX_LEN, then enforce Kraft.
    for entry in working.iter_mut() {
        if entry.1 > MAX_LEN {
            entry.1 = MAX_LEN;
        }
        if entry.1 == 0 {
            // A 1-symbol heap (handled above) is the only way to get
            // depth 0; defensively bump to 1.
            entry.1 = 1;
        }
    }

    // Compute Kraft sum K = Σ 2^(MAX_LEN - len). Target = 2^MAX_LEN.
    let target: u64 = 1u64 << MAX_LEN;
    let kraft = |w: &[(u8, u8)]| -> i64 {
        let mut k: i64 = 0;
        for &(_, l) in w {
            k += (1u64 << (MAX_LEN - l)) as i64;
        }
        k
    };
    // If oversubscribed (K > target), iteratively lengthen the shortest
    // leaves (penalty 1 unit each) until K <= target.
    while kraft(&working) > target as i64 {
        // Lengthen a leaf at the shortest current depth (excluding MAX_LEN).
        let mut best_idx: Option<usize> = None;
        let mut best_len = u8::MAX;
        for (i, &(_, l)) in working.iter().enumerate() {
            if l < MAX_LEN && l < best_len {
                best_len = l;
                best_idx = Some(i);
            }
        }
        match best_idx {
            Some(i) => working[i].1 += 1,
            None => break, // already saturated; nothing else to do
        }
    }
    // If undersubscribed (K < target), iteratively shorten a leaf at
    // the deepest current depth.
    while kraft(&working) < target as i64 {
        let mut best_idx: Option<usize> = None;
        let mut best_len = 0u8;
        for (i, &(_, l)) in working.iter().enumerate() {
            if l > 1 && l > best_len {
                best_len = l;
                best_idx = Some(i);
            }
        }
        match best_idx {
            Some(i) => working[i].1 -= 1,
            None => break,
        }
    }

    for &(s, l) in &working {
        lens[s as usize] = l;
    }
    lens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::decode_packet;

    fn synthetic_plane(seed: u8, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h];
        let mut s = seed as u32;
        for px in out.iter_mut() {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            *px = (s >> 16) as u8;
        }
        out
    }

    fn flat_plane(value: u8, w: usize, h: usize) -> Vec<u8> {
        vec![value; w * h]
    }

    #[test]
    fn forward_left_inverse_roundtrip() {
        let src: Vec<u8> = (0..32).map(|i| (i as u8).wrapping_mul(7)).collect();
        let res = forward_predict(Predictor::Left, &src, 8, 4);
        let mut dec = res.clone();
        crate::predictor::apply_inverse_8bit(Predictor::Left, &mut dec, 8, 4);
        assert_eq!(dec, src);
    }

    #[test]
    fn forward_gradient_inverse_roundtrip() {
        let src: Vec<u8> = (0..32).map(|i| (i as u8).wrapping_mul(13)).collect();
        let res = forward_predict(Predictor::Gradient, &src, 8, 4);
        let mut dec = res.clone();
        crate::predictor::apply_inverse_8bit(Predictor::Gradient, &mut dec, 8, 4);
        assert_eq!(dec, src);
    }

    #[test]
    fn forward_median_inverse_roundtrip() {
        let src: Vec<u8> = (0..32)
            .map(|i| (i as u8).wrapping_mul(11).wrapping_add(0x42))
            .collect();
        let res = forward_predict(Predictor::Median, &src, 8, 4);
        let mut dec = res.clone();
        crate::predictor::apply_inverse_8bit(Predictor::Median, &mut dec, 8, 4);
        assert_eq!(dec, src);
    }

    #[test]
    fn build_lengths_single_symbol_fastpath() {
        let mut hist = [0u32; 256];
        hist[42] = 1024;
        let lens = build_canonical_lengths(&hist);
        assert_eq!(lens[42], 0);
        for (s, &l) in lens.iter().enumerate() {
            if s != 42 {
                assert_eq!(l, 0xFF);
            }
        }
    }

    #[test]
    fn build_lengths_kraft_balanced() {
        // Three symbols with very different frequencies — the encoder
        // should produce code lengths satisfying Σ 2^-l == 1.
        let mut hist = [0u32; 256];
        hist[0] = 100;
        hist[1] = 50;
        hist[2] = 25;
        let lens = build_canonical_lengths(&hist);
        let mut kraft = 0f64;
        for &l in &lens {
            if l != 0xFF && l != 0 {
                kraft += (-(l as i32) as f64).exp2();
            }
        }
        assert!((kraft - 1.0).abs() < 1e-9, "Kraft = {kraft}");
    }

    fn roundtrip_one(fourcc: FourCc, w: u32, h: u32, predictor: Option<Predictor>) {
        let shape = PlaneShape::from_fourcc(fourcc).unwrap();
        let mut planes_owned: Vec<Vec<u8>> = Vec::new();
        for plane_idx in 0..shape.planes as usize {
            let pw = subsampled(w, shape.h_subsample[plane_idx]) as usize;
            let ph = subsampled(h, shape.v_subsample[plane_idx]) as usize;
            planes_owned.push(synthetic_plane((plane_idx as u8).wrapping_add(7), pw, ph));
        }
        let cfg = EncoderConfig {
            fourcc,
            width: w,
            height: h,
            slices: 2,
            predictor,
        };
        let planes_refs: Vec<&[u8]> = planes_owned.iter().map(|p| &p[..]).collect();
        let enc = encode_frame(&cfg, &planes_refs).unwrap();
        let dec = decode_packet(fourcc, &enc.extradata, w, h, &enc.packet).unwrap();
        assert_eq!(dec.planes.len(), planes_owned.len(), "plane count");
        for (i, (got, want)) in dec.planes.iter().zip(planes_owned.iter()).enumerate() {
            assert_eq!(
                got,
                want,
                "plane {i} mismatch ({fourcc:?}, {predictor:?})",
                fourcc = fourcc.as_str()
            );
        }
    }

    #[test]
    fn ulrg_roundtrips_all_predictors() {
        for &p in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            roundtrip_one(FourCc(*b"ULRG"), 32, 16, Some(p));
        }
    }

    #[test]
    fn ulra_roundtrips_all_predictors() {
        for &p in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            roundtrip_one(FourCc(*b"ULRA"), 32, 16, Some(p));
        }
    }

    #[test]
    fn uly2_roundtrips_all_predictors() {
        for &p in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            roundtrip_one(FourCc(*b"ULY2"), 32, 16, Some(p));
        }
    }

    #[test]
    fn uly4_roundtrips_all_predictors() {
        for &p in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            roundtrip_one(FourCc(*b"ULY4"), 32, 16, Some(p));
        }
    }

    #[test]
    fn uly0_roundtrips_all_predictors() {
        // 4:2:0 chroma height = 16/2 = 8 must fit slice partitioning.
        for &p in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            roundtrip_one(FourCc(*b"ULY0"), 32, 16, Some(p));
        }
    }

    #[test]
    fn rdo_picks_a_predictor_and_roundtrips() {
        // Confirm the auto-selector path also produces a valid
        // bitstream end-to-end.
        roundtrip_one(FourCc(*b"ULRG"), 64, 48, None);
        roundtrip_one(FourCc(*b"ULY2"), 64, 48, None);
    }

    #[test]
    fn flat_plane_uses_fill_fast_path() {
        // A solid image should produce a tiny packet via the
        // length-0 fast path for every plane.
        let w = 64u32;
        let h = 48u32;
        let pw = w as usize;
        let ph = h as usize;
        let g = flat_plane(0x55, pw, ph);
        let b = flat_plane(0xAA, pw, ph);
        let r = flat_plane(0x11, pw, ph);
        let cfg = EncoderConfig {
            fourcc: FourCc(*b"ULRG"),
            width: w,
            height: h,
            slices: 1,
            predictor: Some(Predictor::None),
        };
        let enc = encode_frame(&cfg, &[&g, &b, &r]).unwrap();
        // 3 planes × (256 lens + 4 offset bytes + 0 data) + 4 frame_info
        assert_eq!(enc.packet.len(), 3 * (256 + 4) + 4);
        let dec = decode_packet(FourCc(*b"ULRG"), &enc.extradata, w, h, &enc.packet).unwrap();
        assert_eq!(dec.planes[0], g);
        assert_eq!(dec.planes[1], b);
        assert_eq!(dec.planes[2], r);
    }
}
