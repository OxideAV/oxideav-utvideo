//! Per-frame encoder for Ut Video classic-family streams.
//!
//! Round 1 scope: produce wire-format frame bodies that the in-crate
//! decoder ([`crate::decoder::decode_frame`]) accepts and round-trips
//! byte-for-byte. The encoder does NOT attempt de-facto-encoder byte-equality;
//! the Specifier work documents only the canonical carriage form, and the
//! 2026-05-06 audit retired the prior implementation precisely
//! because emitter byte-equality was the conformance criterion. Round
//! 1 picks the strictest *decoder*-driven test instead: feed our own
//! frames through our own decoder.
//!
//! The encoder pipeline mirrors the decoder:
//!
//! 1. RGB forward decorrelation (`spec/04` §6) for ULRG/ULRA.
//! 2. Per-slice forward predictor over each plane's samples.
//! 3. Per-plane Huffman: build a Kraft-complete length descriptor
//!    over the residual histogram, then construct the canonical code
//!    in (length DESC, sym DESC) order per `spec/05` §2.2.
//! 4. Emit the chunk payload: 256-byte descriptor, slice-end-offset
//!    table, slice data; trailing 4-byte frame-info.
//!
//! ## Slice-level parallelism (round 5; budget contract round 420)
//!
//! Mirror of the round-4 decoder parallelism. Every slice's
//! +128-seeded predictor state is independent (`spec/04` §§3.1, 4, 5,
//! 7) and every slice's Huffman bit-stream is a self-contained byte
//! blob (`spec/02` §5), so within one plane both the forward-predict
//! and the per-slice bit-pack steps fan out across `std::thread::scope`.
//! Per-plane Huffman descriptor build sits between the two parallel
//! stages — it must aggregate the cross-slice histogram on a single
//! thread before any slice can pack.
//!
//! The fan-out only happens when the **caller grants a thread
//! budget**: this module never queries host parallelism, and every
//! `std::thread::scope` dispatch is bounded by
//! `workers.min(num_slices).max(1)`. [`encode_frame`] is serial;
//! [`encode_frame_with_workers`] dispatches the parallel path when the
//! budget exceeds 1, the slice count exceeds 1, and the luma pixel
//! count crosses [`PARALLEL_PIXEL_THRESHOLD`];
//! [`encode_frame_serial`] / [`encode_frame_parallel`] give explicit
//! control. Output bytes are identical for every budget. Framework
//! callers grant the budget through `oxideav-core`'s
//! `ExecutionContext` / `set_execution_context` hook (see
//! `crate::registry`); the trait path is serial until that hook is
//! called.

use crate::error::{Error, Result};
use crate::fourcc::{Fourcc, Predictor, slice_row_range};
use crate::huffman::HuffmanTable;
use crate::predict;

/// Frames whose `width * height` (luma plane) is below this threshold
/// run the serial-encode path. Hand-picked to match the decoder's
/// `decoder::PARALLEL_PIXEL_THRESHOLD` (64 Ki pixels ≈ 320×200) so a
/// caller that decodes-then-re-encodes a frame takes the parallel path
/// on both sides for the same payload size.
pub const PARALLEL_PIXEL_THRESHOLD: usize = 64 * 1024;

/// One plane's caller-provided pixel samples. The plane's expected
/// dimensions are derived from the FOURCC + frame size.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlaneInput {
    pub samples: Vec<u8>,
}

impl PlaneInput {
    /// Build `PlaneInput` from every field, in declaration order.
    pub fn new(samples: Vec<u8>) -> Self {
        Self { samples }
    }
}

/// Encoder input describing one frame.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodedFrame {
    pub fourcc: Fourcc,
    pub width: u32,
    pub height: u32,
    pub predictor: Predictor,
    pub num_slices: usize,
    pub planes: Vec<PlaneInput>,
}

impl EncodedFrame {
    /// Build `EncodedFrame` from every field, in declaration order.
    pub fn new(
        fourcc: Fourcc,
        width: u32,
        height: u32,
        predictor: Predictor,
        num_slices: usize,
        planes: Vec<PlaneInput>,
    ) -> Self {
        Self {
            fourcc,
            width,
            height,
            predictor,
            num_slices,
            planes,
        }
    }
}

/// Encode one classic-family frame. Returns the chunk payload bytes
/// (the AVI `00dc` chunk's body — the AVI chunk header itself is
/// written by `oxideav-avi` if a container is in use).
///
/// **Serial.** Since round 420 this entry runs single-threaded — the
/// threading contract makes one worker the default, and internal
/// fan-out happens only when the caller grants an explicit budget.
/// Callers that want slice-parallel encode pass a worker budget to
/// [`encode_frame_with_workers`] (threshold-gated) or
/// [`encode_frame_parallel`] (forced fan-out). Every path produces
/// byte-identical output (verified by the round-5 and round-420
/// suites).
pub fn encode_frame(frame: &EncodedFrame) -> Result<Vec<u8>> {
    encode_frame_serial(frame)
}

/// Budgeted encode: dispatches the slice-parallel path when the
/// caller-granted `workers` budget exceeds 1, the frame has more than
/// one slice, **and** the luma pixel count crosses
/// [`PARALLEL_PIXEL_THRESHOLD`] (below it the thread-spawn + join cost
/// typically dominates the per-slice work). Otherwise encodes
/// serially. Output bytes are identical for every budget.
///
/// The fan-out never exceeds `workers.min(num_slices).max(1)` threads
/// and the host is never queried — deriving a budget from the machine
/// (e.g. via `std::thread::available_parallelism`) is the caller's
/// decision.
pub fn encode_frame_with_workers(frame: &EncodedFrame, workers: usize) -> Result<Vec<u8>> {
    let total = (frame.width as usize) * (frame.height as usize);
    if workers > 1 && frame.num_slices > 1 && total >= PARALLEL_PIXEL_THRESHOLD {
        encode_frame_parallel(frame, workers)
    } else {
        encode_frame_serial(frame)
    }
}

/// Serial-path encode: one plane after another, slices within a plane
/// also serialised. Exposed so callers can opt out of the thread-pool
/// spin-up cost in latency-sensitive single-frame paths.
pub fn encode_frame_serial(frame: &EncodedFrame) -> Result<Vec<u8>> {
    encode_frame_budgeted(frame, 1)
}

/// Parallel-path encode: per-slice forward-predict + histogram and
/// per-slice bit-pack both run on `std::thread::scope`, fanned out
/// across at most `workers.min(num_slices).max(1)` threads regardless
/// of frame size (no [`PARALLEL_PIXEL_THRESHOLD`] gate). Every slice's
/// predictor state restarts at the +128 seed (`spec/04` §§3.1, 4, 5,
/// 7) and every slice's Huffman bit-stream is self-contained
/// (`spec/02` §5), so the fan-out is bit-exact equivalent to the
/// serial path. A `workers` budget of 0 or 1 degenerates to the
/// serial walk.
pub fn encode_frame_parallel(frame: &EncodedFrame, workers: usize) -> Result<Vec<u8>> {
    encode_frame_budgeted(frame, workers)
}

fn encode_frame_budgeted(frame: &EncodedFrame, workers: usize) -> Result<Vec<u8>> {
    let plane_count = validate_frame(frame)?;

    // RGB forward decorrelation (`spec/04` §6): `B' = B - G + 128`,
    // `R' = R - G + 128`; G and A are coded as they are. Only the two
    // transformed planes are copied — every other plane is read in
    // place from the caller's buffers.
    let decorrelated = frame.fourcc.is_rgb_family().then(|| {
        let mut b = frame.planes[1].samples.clone();
        let mut r = frame.planes[2].samples.clone();
        predict::forward_decorrelate_rgb(&frame.planes[0].samples, &mut b, &mut r);
        (b, r)
    });

    let input_bytes: usize = frame.planes.iter().map(|p| p.samples.len()).sum();
    let mut out: Vec<u8> = Vec::with_capacity(input_bytes / 2 + plane_count * 320 + 4);
    for i in 0..plane_count {
        let samples: &[u8] = match (&decorrelated, i) {
            (Some((b, _)), 1) => b,
            (Some((_, r)), 2) => r,
            _ => &frame.planes[i].samples,
        };
        let (pw, ph) = frame.fourcc.plane_dim(i, frame.width, frame.height);
        encode_plane(
            &mut out,
            samples,
            pw as usize,
            ph as usize,
            frame.fourcc.slice_row_step(i),
            frame,
            workers,
        )?;
    }
    out.extend_from_slice(&frame.predictor.as_frame_info_bits().to_le_bytes());
    Ok(out)
}

/// Check the frame description and the per-plane buffer sizes; returns
/// the plane count.
fn validate_frame(frame: &EncodedFrame) -> Result<usize> {
    frame.fourcc.validate_dims(frame.width, frame.height)?;
    let plane_count = frame.fourcc.plane_count();
    if frame.planes.len() != plane_count {
        return Err(Error::EncoderPlaneSizeMismatch {
            plane: frame.planes.len(),
            expected: plane_count,
            got: frame.planes.len(),
        });
    }
    if frame.num_slices == 0 || frame.num_slices > 256 {
        return Err(Error::InvalidSliceCount);
    }

    // Interop cap: more slices than the smallest plane's row count would
    // force zero-length slices in multi-symbol planes, which conformant
    // decoders reject as malformed (black-box observation; the reference
    // encoder enforces the same cap at the subsampling-applied plane
    // height). Refuse to emit streams only this crate can read. The
    // in-crate decoder stays lenient and keeps accepting zero-pixel
    // slices.
    let min_plane_height = (0..plane_count)
        .map(|i| frame.fourcc.plane_dim(i, frame.width, frame.height).1 as usize)
        .min()
        .unwrap_or(0);
    if frame.num_slices > min_plane_height {
        return Err(Error::SliceCountExceedsPlaneHeight {
            num_slices: frame.num_slices,
            min_plane_height,
        });
    }

    for (i, p) in frame.planes.iter().enumerate() {
        let (pw, ph) = frame.fourcc.plane_dim(i, frame.width, frame.height);
        let expected = (pw as usize) * (ph as usize);
        if p.samples.len() != expected {
            return Err(Error::EncoderPlaneSizeMismatch {
                plane: i,
                expected,
                got: p.samples.len(),
            });
        }
    }
    Ok(plane_count)
}

/// Encode one plane and append its wire blob to `out`: 256-byte
/// descriptor, `num_slices` u32 LE end offsets, slice data
/// (`spec/02` §2).
///
/// Three stages. (1) Per slice: forward-predict and count the residual
/// symbols. (2) Once per plane: code lengths from the summed counts.
/// (3) Per slice: pack. Because stage 1 leaves a histogram per slice,
/// each slice's packed size is known exactly before stage 3, so the
/// slice data is packed straight into its final position in `out` —
/// no per-slice buffers, no concatenation. Stages 1 and 3 fan out
/// across the caller's worker budget.
fn encode_plane(
    out: &mut Vec<u8>,
    samples: &[u8],
    width: usize,
    height: usize,
    row_step: usize,
    frame: &EncodedFrame,
    workers: usize,
) -> Result<()> {
    let num_slices = frame.num_slices;
    let predictor = frame.predictor;

    let slices: Vec<(Vec<u8>, [u32; 256])> =
        fan_out((0..num_slices).collect(), workers, |_, s: usize| {
            let (r_start, r_end) = slice_row_range(height, row_step, num_slices, s);
            let residuals = predict::forward_slice(predictor, samples, width, r_start, r_end);
            let counts = histogram(&residuals);
            (residuals, counts)
        });

    let mut counts = [0u64; 256];
    for (_, slice_counts) in &slices {
        for (total, &c) in counts.iter_mut().zip(slice_counts) {
            *total += u64::from(c);
        }
    }
    let (descriptor, table) = build_plane_huffman(&counts)?;
    let packer = SlicePacker::new(&table);

    out.extend_from_slice(&descriptor);
    let mut end = 0usize;
    let sizes: Vec<usize> = slices
        .iter()
        .map(|(_, slice_counts)| packer.packed_bytes(slice_counts))
        .collect();
    for &size in &sizes {
        end += size;
        out.extend_from_slice(&(end as u32).to_le_bytes());
    }
    let data_start = out.len();
    out.resize(data_start + end, 0);

    let mut regions: Vec<(&mut [u8], &[u8])> = Vec::with_capacity(num_slices);
    let mut remaining = &mut out[data_start..];
    for (&size, (residuals, _)) in sizes.iter().zip(&slices) {
        let (region, tail) = remaining.split_at_mut(size);
        regions.push((region, residuals.as_slice()));
        remaining = tail;
    }
    let complete = fan_out(regions, workers, |_, (region, residuals)| {
        packer.pack(residuals, region)
    });
    if complete.contains(&false) {
        // A residual without a code: the table was not built from these
        // residuals. Unreachable from this function.
        return Err(Error::HuffmanDecodeFailure { bit_position: 0 });
    }
    Ok(())
}

/// Run `f(index, item)` for every item and return the results in item
/// order. With a budget above one the items are split into contiguous
/// runs, one scoped thread per run, at most `workers.min(items.len())`
/// threads; the host is never queried (threading contract). The join
/// is a blocking wait.
fn fan_out<I: Send, T: Send>(
    items: Vec<I>,
    workers: usize,
    f: impl Fn(usize, I) -> T + Sync,
) -> Vec<T> {
    let n = items.len();
    let threads = workers.min(n).max(1);
    if threads == 1 {
        return items
            .into_iter()
            .enumerate()
            .map(|(i, item)| f(i, item))
            .collect();
    }
    let run = n.div_ceil(threads);
    let mut results: Vec<Option<T>> = (0..n).map(|_| None).collect();
    std::thread::scope(|scope| {
        let f = &f;
        let mut items = items.into_iter().enumerate();
        for slots in results.chunks_mut(run) {
            let batch: Vec<(usize, I)> = items.by_ref().take(slots.len()).collect();
            scope.spawn(move || {
                for (slot, (i, item)) in slots.iter_mut().zip(batch) {
                    *slot = Some(f(i, item));
                }
            });
        }
    });
    results
        .into_iter()
        .map(|r| r.expect("every slot is filled before the scope ends"))
        .collect()
}

/// Symbol counts of `data`. Four interleaved tables break the
/// store-to-load dependency between consecutive equal symbols (long
/// runs of one residual are the common case after prediction).
fn histogram(data: &[u8]) -> [u32; 256] {
    let mut lanes = [[0u32; 256]; 4];
    let (quads, rest) = data.as_chunks::<4>();
    for q in quads {
        lanes[0][q[0] as usize] += 1;
        lanes[1][q[1] as usize] += 1;
        lanes[2][q[2] as usize] += 1;
        lanes[3][q[3] as usize] += 1;
    }
    for &b in rest {
        lanes[0][b as usize] += 1;
    }
    let mut total = [0u32; 256];
    for (s, t) in total.iter_mut().enumerate() {
        *t = lanes[0][s] + lanes[1][s] + lanes[2][s] + lanes[3][s];
    }
    total
}

/// Per-plane packing table: every symbol's code left-aligned in a
/// 32-bit word, and its length (0 = no code).
struct SlicePacker {
    code: [u32; 256],
    len: [u8; 256],
    /// No slice data at all (`spec/05` §6.1 single-symbol plane, or an
    /// empty plane).
    empty: bool,
    max_len: u8,
}

impl SlicePacker {
    fn new(table: &HuffmanTable) -> Self {
        let mut code = [0u32; 256];
        let mut len = [0u8; 256];
        let mut max_len = 0u8;
        for s in 0..=255u8 {
            if let Some((c, l)) = table.code_for(s) {
                code[s as usize] = c << (32 - u32::from(l));
                len[s as usize] = l;
                max_len = max_len.max(l);
            }
        }
        SlicePacker {
            code,
            len,
            empty: table.single_symbol.is_some() || max_len == 0,
            max_len,
        }
    }

    /// Exact byte size of a slice with these symbol counts: the code
    /// bits rounded up to a 32-bit word (`spec/05` §4.4).
    fn packed_bytes(&self, counts: &[u32; 256]) -> usize {
        if self.empty {
            return 0;
        }
        let bits: u64 = counts
            .iter()
            .zip(&self.len)
            .map(|(&c, &l)| u64::from(c) * u64::from(l))
            .sum();
        (bits.div_ceil(32) * 4) as usize
    }

    /// Pack `residuals` into `region`, which is exactly
    /// [`packed_bytes`](Self::packed_bytes) long and zero-filled:
    /// 32-bit LE words, MSB first, zero padding (`spec/05` §4).
    /// Returns `false` if a residual has no code.
    ///
    /// The pending bits sit left-aligned in a 64-bit register. After
    /// each step the register's top word is stored at the current
    /// output word — a complete word when 32 or more bits are pending,
    /// otherwise a prefix that a later store overwrites — and the
    /// position advances by a conditional move, so the loop has no
    /// data-dependent branch. Codes of at most 16 bits (always the
    /// case for this encoder's tables) go two per step.
    fn pack(&self, residuals: &[u8], region: &mut [u8]) -> bool {
        if self.empty {
            return true;
        }
        let (words, _) = region.as_chunks_mut::<4>();
        let mut acc: u64 = 0;
        let mut pending: u32 = 0; // bits in `acc`, always < 32 between steps
        let mut at = 0usize;
        let mut complete = true;
        let mut step = |code: u32, len: u32| {
            acc |= u64::from(code) << (32 - pending);
            pending += len;
            if let Some(word) = words.get_mut(at) {
                *word = ((acc >> 32) as u32).to_le_bytes();
            }
            let full = pending >= 32;
            at += usize::from(full);
            acc <<= if full { 32 } else { 0 };
            pending -= if full { 32 } else { 0 };
        };
        let singles = if self.max_len <= 16 {
            let (pairs, rest) = residuals.as_chunks::<2>();
            for &[a, b] in pairs {
                let (len_a, len_b) = (self.len[a as usize], self.len[b as usize]);
                complete &= len_a != 0 && len_b != 0;
                let code = self.code[a as usize] | (self.code[b as usize] >> len_a);
                step(code, u32::from(len_a) + u32::from(len_b));
            }
            rest
        } else {
            residuals
        };
        for &r in singles {
            let len = self.len[r as usize];
            complete &= len != 0;
            step(self.code[r as usize], u32::from(len));
        }
        // Bits carried past the last completed word (a zero-length
        // step stores the register's prefix without adding to it).
        step(0, 0);
        complete
    }
}

/// Build a 256-byte code-length descriptor + matching [`HuffmanTable`]
/// from the per-slice residual streams of one plane.
fn build_plane_huffman(counts: &[u64; 256]) -> Result<([u8; 256], HuffmanTable)> {
    let total: u64 = counts.iter().sum();

    // Special case 1: no residuals at all (all slices have zero
    // pixels — only happens in degenerate fixtures). Emit an empty
    // descriptor.
    if total == 0 {
        let descriptor = [255u8; 256];
        let table = HuffmanTable::build(&descriptor)?;
        return Ok((descriptor, table));
    }

    // Special case 2: exactly one symbol used (constant residual
    // stream). Emit the single-symbol sentinel per `spec/05` §6.1.
    let used: Vec<usize> = counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(s, _)| s)
        .collect();
    if used.len() == 1 {
        let mut descriptor = [255u8; 256];
        descriptor[used[0]] = 0;
        let table = HuffmanTable::build(&descriptor)?;
        return Ok((descriptor, table));
    }

    // General case: build canonical Huffman code lengths via a
    // length-limited package-merge (length cap = 16, matching the
    // empirical maximum across the reference corpus per `spec/05` §7).
    let lengths = build_lengths(counts, 16)?;

    let mut descriptor = [255u8; 256];
    for (s, l) in lengths.iter().enumerate() {
        if *l > 0 {
            descriptor[s] = *l;
        }
    }
    let table = HuffmanTable::build(&descriptor)?;
    Ok((descriptor, table))
}

/// Length-limited Huffman code-length builder via the package-merge
/// algorithm (Larmore-Hirschberg 1990). Produces code lengths that
/// sum to a Kraft-complete prefix code with `max_length` cap.
fn build_lengths(counts: &[u64; 256], max_length: u8) -> Result<Vec<u8>> {
    let max_length = max_length as usize;
    // Collect (count, sym) for symbols that actually appear.
    let mut symbols: Vec<(u64, u8)> = (0..=255u8)
        .filter_map(|s| {
            let c = counts[s as usize];
            if c == 0 { None } else { Some((c, s)) }
        })
        .collect();
    if symbols.len() <= 1 {
        // Special-cased earlier; still guard.
        let mut out = vec![0u8; 256];
        if let Some(&(_, s)) = symbols.first() {
            out[s as usize] = 1;
        }
        return Ok(out);
    }
    // Verify length cap is feasible (`2^max_length >= n`).
    if (1usize << max_length) < symbols.len() {
        return Err(Error::InvalidInput(
            "encoder histogram exceeds length-limited Huffman alphabet capacity",
        ));
    }
    symbols.sort_by_key(|t| (t.0, t.1));

    // Package-merge proper. Each "node" is `(weight, set-of-sym-indices)`.
    // The set of symbols is represented compactly as a Vec.
    #[derive(Clone)]
    struct Node {
        weight: u64,
        syms: Vec<u8>,
    }
    let leaves: Vec<Node> = symbols
        .iter()
        .map(|(c, s)| Node {
            weight: *c,
            syms: vec![*s],
        })
        .collect();

    // L_max: list of leaves at the longest tier.
    let mut prev: Vec<Node> = leaves.clone();
    for _depth in 1..max_length {
        // Pair adjacent items in `prev` into "packages", then merge with leaves.
        let mut packaged: Vec<Node> = Vec::with_capacity(prev.len() / 2);
        let mut i = 0;
        while i + 1 < prev.len() {
            let mut a = prev[i].clone();
            let b = &prev[i + 1];
            a.weight += b.weight;
            a.syms.extend_from_slice(&b.syms);
            packaged.push(a);
            i += 2;
        }
        // Merge `packaged` and `leaves`, keeping ascending weight.
        let mut next: Vec<Node> = Vec::with_capacity(packaged.len() + leaves.len());
        let (mut p, mut l) = (0, 0);
        while p < packaged.len() && l < leaves.len() {
            if packaged[p].weight <= leaves[l].weight {
                next.push(packaged[p].clone());
                p += 1;
            } else {
                next.push(leaves[l].clone());
                l += 1;
            }
        }
        while p < packaged.len() {
            next.push(packaged[p].clone());
            p += 1;
        }
        while l < leaves.len() {
            next.push(leaves[l].clone());
            l += 1;
        }
        prev = next;
    }

    // Take the first `2 * symbols.len() - 2` nodes (the package-merge
    // "active" set) and count occurrences of each symbol; that count
    // IS its code length.
    let take = 2 * symbols.len() - 2;
    let mut counts_per_sym = [0u32; 256];
    for node in prev.iter().take(take) {
        for &s in &node.syms {
            counts_per_sym[s as usize] += 1;
        }
    }
    let mut lengths = vec![0u8; 256];
    for s in 0..=255u8 {
        let n = counts_per_sym[s as usize];
        if n > 0 {
            lengths[s as usize] = n as u8;
        }
    }

    // Sanity: Kraft equality. (Should hold by package-merge construction;
    // verify defensively.)
    let max_l: u8 = *lengths.iter().max().unwrap();
    let scale: u64 = 1u64 << max_l;
    let mut sum = 0u64;
    for &l in &lengths {
        if l > 0 {
            sum += scale >> l;
        }
    }
    if sum != scale {
        return Err(Error::KraftViolation);
    }
    Ok(lengths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fourcc::Fourcc;

    #[test]
    fn build_lengths_simple_two_symbol_histogram() {
        let mut counts = [0u64; 256];
        counts[0] = 100;
        counts[128] = 100;
        let lengths = build_lengths(&counts, 16).unwrap();
        assert_eq!(lengths[0], 1);
        assert_eq!(lengths[128], 1);
    }

    #[test]
    fn encode_plane_solid_constant_uly0() {
        // A solid Y=128 plane under predictor=Left collapses to:
        //   first sample residual = 128 - 128 = 0; 255 zeros after.
        // i.e. all 256 residuals are 0, single-symbol-zero plane.
        let frame = EncodedFrame {
            fourcc: Fourcc::Uly0,
            width: 16,
            height: 16,
            predictor: Predictor::Left,
            num_slices: 1,
            planes: vec![
                PlaneInput {
                    samples: vec![128u8; 256],
                },
                PlaneInput {
                    samples: vec![128u8; 64],
                },
                PlaneInput {
                    samples: vec![128u8; 64],
                },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        // Each plane has 256-byte descriptor + 4-byte single end offset (0)
        // + 0 slice bytes; 3 planes; + 4 bytes frame_info.
        assert_eq!(bytes.len(), 3 * (256 + 4) + 4);
    }
}
