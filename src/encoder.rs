//! Per-frame encoder for Ut Video classic-family streams.
//!
//! Round 1 scope: produce wire-format frame bodies that the in-crate
//! decoder ([`crate::decoder::decode_frame`]) accepts and round-trips
//! byte-for-byte. The encoder does NOT attempt FFmpeg byte-equality;
//! the Specifier work documents only what FFmpeg writes, and the
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

use crate::error::{Error, Result};
use crate::fourcc::{Fourcc, Predictor};
use crate::huffman::{BitWriter, HuffmanTable};
use crate::predict;

/// One plane's caller-provided pixel samples. The plane's expected
/// dimensions are derived from the FOURCC + frame size.
#[derive(Debug, Clone)]
pub struct PlaneInput {
    pub samples: Vec<u8>,
}

/// Encoder input describing one frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub fourcc: Fourcc,
    pub width: u32,
    pub height: u32,
    pub predictor: Predictor,
    pub num_slices: usize,
    pub planes: Vec<PlaneInput>,
}

/// Encode one classic-family frame. Returns the chunk payload bytes
/// (the AVI `00dc` chunk's body — the AVI chunk header itself is
/// written by `oxideav-avi` if a container is in use).
pub fn encode_frame(frame: &EncodedFrame) -> Result<Vec<u8>> {
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

    // Take a mutable copy so we can decorrelate RGB in place before
    // running the predictor.
    let mut planes: Vec<Vec<u8>> = frame.planes.iter().map(|p| p.samples.clone()).collect();
    for (i, p) in planes.iter().enumerate() {
        let (pw, ph) = frame.fourcc.plane_dim(i, frame.width, frame.height);
        let expected = (pw as usize) * (ph as usize);
        if p.len() != expected {
            return Err(Error::EncoderPlaneSizeMismatch {
                plane: i,
                expected,
                got: p.len(),
            });
        }
    }

    // RGB forward decorrelation: B' = B - G + 128, R' = R - G + 128.
    if frame.fourcc.is_rgb_family() {
        // Borrow checker dance: split off G, then mutate B and R against it.
        let g_clone = planes[0].clone();
        let (_, tail) = planes.split_first_mut().unwrap();
        let (b, rest) = tail.split_first_mut().unwrap();
        let r = &mut rest[0];
        predict::forward_decorrelate_rgb(&g_clone, b, r);
    }

    // Per-plane forward predict + Huffman + bit-pack.
    let mut plane_blobs: Vec<Vec<u8>> = Vec::with_capacity(plane_count);
    for (i, plane) in planes.iter().enumerate() {
        let (pw, ph) = frame.fourcc.plane_dim(i, frame.width, frame.height);
        let pw = pw as usize;
        let ph = ph as usize;
        let slice_residuals = predict::forward(frame.predictor, plane, pw, ph, frame.num_slices);
        let (descriptor, table) = build_plane_huffman(&slice_residuals)?;
        let blob = encode_plane(&descriptor, &table, &slice_residuals, frame.num_slices)?;
        plane_blobs.push(blob);
    }

    // Concatenate plane blobs + frame_info.
    let mut out: Vec<u8> = Vec::new();
    for b in plane_blobs {
        out.extend_from_slice(&b);
    }
    let frame_info = frame.predictor.as_frame_info_bits();
    out.extend_from_slice(&frame_info.to_le_bytes());
    Ok(out)
}

/// Build a 256-byte code-length descriptor + matching [`HuffmanTable`]
/// from the per-slice residual streams of one plane.
fn build_plane_huffman(slice_residuals: &[Vec<u8>]) -> Result<([u8; 256], HuffmanTable)> {
    // Histogram across all slices.
    let mut counts = [0u64; 256];
    let mut total = 0u64;
    for s in slice_residuals {
        for &r in s {
            counts[r as usize] += 1;
            total += 1;
        }
    }

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
    // empirical maximum across the FFmpeg corpus per `spec/05` §7).
    let lengths = build_lengths(&counts, 16)?;

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
            if c == 0 {
                None
            } else {
                Some((c, s))
            }
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

/// Pack one plane's slice residuals into a wire-format byte blob:
/// 256-byte descriptor, num_slices × u32 LE slice-end offsets, slice
/// data bytes.
fn encode_plane(
    descriptor: &[u8; 256],
    table: &HuffmanTable,
    slice_residuals: &[Vec<u8>],
    num_slices: usize,
) -> Result<Vec<u8>> {
    let mut slice_blobs: Vec<Vec<u8>> = Vec::with_capacity(num_slices);
    let mut slice_end_offsets = Vec::with_capacity(num_slices);
    let mut cumulative = 0usize;
    for residuals in slice_residuals.iter().take(num_slices) {
        let bytes = if table.single_symbol.is_some() {
            // Single-symbol special case: 0 bytes of slice data.
            Vec::new()
        } else if residuals.is_empty() {
            Vec::new()
        } else {
            let mut bw = BitWriter::new();
            for &r in residuals {
                let (c, l) = table
                    .code_for(r)
                    .ok_or(Error::HuffmanDecodeFailure { bit_position: 0 })?;
                bw.write_code(c, l);
            }
            bw.finish()
        };
        cumulative += bytes.len();
        slice_end_offsets.push(cumulative as u32);
        slice_blobs.push(bytes);
    }
    let mut out = Vec::with_capacity(
        256 + 4 * num_slices + slice_blobs.iter().map(|s| s.len()).sum::<usize>(),
    );
    out.extend_from_slice(descriptor);
    for v in &slice_end_offsets {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for blob in slice_blobs {
        out.extend_from_slice(&blob);
    }
    Ok(out)
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
