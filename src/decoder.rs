//! Per-frame decoder for Ut Video classic-family streams.
//!
//! The pipeline at this layer is:
//!
//! 1. Walk the chunk payload plane-by-plane per `spec/02` §7,
//!    extracting the 256-byte Huffman descriptor + slice-end-offset
//!    table + slice data for each plane in turn.
//! 2. Build a [`HuffmanTable`](crate::huffman::HuffmanTable) for each
//!    plane and decode every slice's residuals via the bit reader.
//! 3. Inverse-predict each slice using the predictor named by
//!    `frame_info & 0x300`.
//! 4. For RGB streams, undo the +128 / G-subtraction decorrelation.
//!
//! The decoded frame leaves this module as one [`DecodedPlane`] per
//! wire plane, in the on-wire order. Downstream consumers do their
//! own per-pixel reordering (e.g. R, G, B for an interleaved BGR
//! buffer) — this is consistent with `oxideav-magicyuv`'s decoded
//! plane API and keeps the codec free of pixel-packing policy.
//!
//! ## Slice-level parallelism (round 4)
//!
//! Per `spec/02` §7 "Implementation notes": *"Parallel per-slice
//! decoding is therefore achievable: a multi-threaded decoder reads
//! the offset table once per plane, then dispatches the slice-data
//! byte ranges to per-thread decoders."* The decoder now does exactly
//! that for frames whose pixel count crosses
//! [`PARALLEL_PIXEL_THRESHOLD`] and whose slice count is `> 1`. Each
//! plane's Huffman table is built once, then the slices' Huffman
//! decodes + inverse-predicts run on a `std::thread::scope`
//! thread-pool sized at `min(num_slices, available_parallelism())`.
//!
//! Slices are fully independent for both stages: every slice's
//! Huffman bit-stream is self-contained (`spec/02` §5) and every
//! slice's predictor state restarts at the per-slice `+128` seed
//! (`spec/04` §§3.1, 4, 5, 7). The parent thread joins all slices'
//! reconstructed row strips into the per-plane output buffer in
//! display order before the RGB inverse-decorrelation pass runs.

use crate::error::{Error, Result};
use crate::fourcc::{Fourcc, Predictor, StreamConfig};
use crate::huffman::HuffmanTable;
use crate::predict;

/// Frames whose `width * height` (luma plane) is below this threshold
/// run the serial-decode path. The thread-spawn + join cost typically
/// dominates the per-slice decode work for small frames; this bound
/// reserves the parallel path for `~ 320×240`+ payloads where it
/// actually pays. The threshold is hand-picked from
/// `tests/round4_parallel_decode.rs` perf smoke (320×240 = 76 800
/// pixels lies just above it).
pub const PARALLEL_PIXEL_THRESHOLD: usize = 64 * 1024;

/// One decoded plane: width × height in `samples`, plus the symbolic
/// label for the plane (Y, U, V, G, B, R, A) for the FOURCC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPlane {
    pub label: PlaneLabel,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<u8>,
}

/// Symbolic plane labels; reflect on-wire order, not BGR/RGB display
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneLabel {
    Y,
    U,
    V,
    G,
    B,
    R,
    A,
}

impl PlaneLabel {
    pub fn for_fourcc(fc: Fourcc, idx: usize) -> Self {
        match fc {
            Fourcc::Uly0 | Fourcc::Uly2 | Fourcc::Uly4 => match idx {
                0 => PlaneLabel::Y,
                1 => PlaneLabel::U,
                _ => PlaneLabel::V,
            },
            Fourcc::Ulrg => match idx {
                0 => PlaneLabel::G,
                1 => PlaneLabel::B,
                _ => PlaneLabel::R,
            },
            Fourcc::Ulra => match idx {
                0 => PlaneLabel::G,
                1 => PlaneLabel::B,
                2 => PlaneLabel::R,
                _ => PlaneLabel::A,
            },
        }
    }
}

/// Output of a successful frame decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub fourcc: Fourcc,
    pub width: u32,
    pub height: u32,
    pub predictor: Predictor,
    pub planes: Vec<DecodedPlane>,
    /// Trailing 4-byte frame-info dword as parsed off the wire. The
    /// prediction-mode bits have already been folded into `predictor`;
    /// other bits surface here for diagnostics.
    pub frame_info: u32,
}

/// Decode one Ut Video frame given its `00dc` chunk payload bytes
/// and a parsed [`StreamConfig`].
///
/// Picks the serial or [`PARALLEL_PIXEL_THRESHOLD`]-gated parallel
/// path automatically. Callers that want explicit control can use
/// [`decode_frame_serial`] / [`decode_frame_parallel`] directly.
pub fn decode_frame(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<DecodedFrame> {
    let total = (cfg.width as usize) * (cfg.height as usize);
    if cfg.num_slices() > 1 && total >= PARALLEL_PIXEL_THRESHOLD {
        decode_frame_parallel(cfg, chunk_payload)
    } else {
        decode_frame_serial(cfg, chunk_payload)
    }
}

/// Serial-path decode: one slice after another, single-threaded.
/// Always used for `num_slices == 1` or for frames smaller than
/// [`PARALLEL_PIXEL_THRESHOLD`]. Exposed so callers can opt out of
/// the thread-pool spin-up cost in latency-sensitive single-frame
/// paths.
pub fn decode_frame_serial(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<DecodedFrame> {
    let parsed = parse_payload(cfg, chunk_payload)?;
    finish_decode(cfg, parsed, /*parallel=*/ false)
}

/// Parallel-path decode: per-slice decode + inverse-predict run on
/// `std::thread::scope`. Each slice is independent (per-slice +128
/// seed; self-contained Huffman bit-stream), so the join produces a
/// bit-exact equivalent of the serial output.
pub fn decode_frame_parallel(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<DecodedFrame> {
    let parsed = parse_payload(cfg, chunk_payload)?;
    finish_decode(cfg, parsed, /*parallel=*/ true)
}

/// Strict-conformance decode: identical output to [`decode_frame`] for
/// any well-formed stream, but additionally enforces the `spec/05`
/// §4.3 zero-padding convention. After each slice's `n_pixels` symbols
/// are decoded, the bits from the last code up to the slice's 32-bit
/// word boundary MUST all be zero; a set bit yields
/// [`Error::NonZeroPadding`] naming the offending plane / slice /
/// bit-position.
///
/// `spec/05` §8 classifies a non-zero padding bit as a SHOULD-warn for
/// defensive decoders — the default [`decode_frame`] path follows the
/// spec's "a decoder MUST NOT consume the padding bits" rule and
/// ignores the slice tail. This entry point is for callers that want
/// to *reject* a stream whose encoder did not zero the padding (e.g. a
/// conformance harness or a re-encode-equivalence check). It always
/// runs the serial path so the padding scan stays deterministic and
/// the location fields are exact.
pub fn decode_frame_strict(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<DecodedFrame> {
    let parsed = parse_payload(cfg, chunk_payload)?;
    finish_decode_strict(cfg, parsed)
}

/// Parsed per-plane structure ready to feed into the slice-decode
/// stage. The slice data is left as a borrowed sub-slice of the
/// original chunk payload; no copy happens.
struct ParsedPlane<'a> {
    label: PlaneLabel,
    width: usize,
    height: usize,
    table: HuffmanTable,
    /// Per-slice raw bytes (length == `num_slices`).
    slice_bytes: Vec<&'a [u8]>,
}

struct ParsedFrame<'a> {
    planes: Vec<ParsedPlane<'a>>,
    frame_info: u32,
}

/// Walk the chunk payload, validating per-plane structure and
/// building Huffman tables; produces a [`ParsedFrame`] that the
/// slice-decode stage can fan out (serial or parallel).
fn parse_payload<'a>(cfg: &StreamConfig, chunk_payload: &'a [u8]) -> Result<ParsedFrame<'a>> {
    let num_slices = cfg.num_slices();
    if num_slices == 0 {
        return Err(Error::InvalidSliceCount);
    }
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let frame_info_off = chunk_payload.len() - 4;

    let mut offset = 0usize;
    let mut planes = Vec::with_capacity(cfg.fourcc.plane_count());

    for plane_idx in 0..cfg.fourcc.plane_count() {
        let (pw, ph) = cfg.fourcc.plane_dim(plane_idx, cfg.width, cfg.height);
        let pw = pw as usize;
        let ph = ph as usize;

        // 256-byte Huffman descriptor.
        if offset + 256 > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: 256,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut code_length = [0u8; 256];
        code_length.copy_from_slice(&chunk_payload[offset..offset + 256]);
        offset += 256;

        // Slice-end offsets table.
        let table_bytes = num_slices * 4;
        if offset + table_bytes > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: table_bytes,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut slice_end_offsets = Vec::with_capacity(num_slices);
        for s in 0..num_slices {
            let v = u32::from_le_bytes(
                chunk_payload[offset + 4 * s..offset + 4 * s + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            slice_end_offsets.push(v);
        }
        offset += table_bytes;

        // Monotonicity + word alignment validation per spec/02 §5 +
        // spec/05 §4.1.
        let mut prev = 0usize;
        for &v in &slice_end_offsets {
            if v < prev {
                return Err(Error::NonMonotonicSliceOffsets);
            }
            if v % 4 != 0 {
                return Err(Error::SliceNotWordAligned(v));
            }
            prev = v;
        }
        let slice_data_total = *slice_end_offsets.last().unwrap();

        if offset + slice_data_total > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: slice_data_total,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let slice_data = &chunk_payload[offset..offset + slice_data_total];
        offset += slice_data_total;

        // Split slice_data into per-slice borrowed subslices.
        let mut slice_bytes: Vec<&'a [u8]> = Vec::with_capacity(num_slices);
        let mut prev_off = 0usize;
        for &end in &slice_end_offsets {
            slice_bytes.push(&slice_data[prev_off..end]);
            prev_off = end;
        }

        let table = HuffmanTable::build(&code_length)?;
        planes.push(ParsedPlane {
            label: PlaneLabel::for_fourcc(cfg.fourcc, plane_idx),
            width: pw,
            height: ph,
            table,
            slice_bytes,
        });
    }

    if offset != frame_info_off {
        return Err(Error::ChunkTooShort {
            offset,
            needed: frame_info_off - offset,
            have: 0,
        });
    }
    let frame_info = u32::from_le_bytes(
        chunk_payload[frame_info_off..frame_info_off + 4]
            .try_into()
            .unwrap(),
    );
    Ok(ParsedFrame { planes, frame_info })
}

/// Finish the decode: dispatch each plane's slice decode + inverse
/// predict (serial or parallel), apply RGB inverse-decorrelation,
/// build the [`DecodedFrame`].
fn finish_decode(
    cfg: &StreamConfig,
    parsed: ParsedFrame<'_>,
    parallel: bool,
) -> Result<DecodedFrame> {
    let num_slices = cfg.num_slices();
    let predictor = Predictor::from_frame_info(parsed.frame_info);

    let mut decoded_planes: Vec<DecodedPlane> = Vec::with_capacity(parsed.planes.len());
    for plane in parsed.planes {
        let pw = plane.width;
        let ph = plane.height;
        let mut samples = vec![0u8; pw * ph];

        if parallel && num_slices > 1 {
            decode_plane_parallel(
                &plane.table,
                &plane.slice_bytes,
                num_slices,
                pw,
                ph,
                predictor,
                &mut samples,
            )?;
        } else {
            decode_plane_serial(
                &plane.table,
                &plane.slice_bytes,
                num_slices,
                pw,
                ph,
                predictor,
                &mut samples,
            )?;
        }

        decoded_planes.push(DecodedPlane {
            label: plane.label,
            width: pw as u32,
            height: ph as u32,
            samples,
        });
    }

    if cfg.fourcc.is_rgb_family() {
        let g_clone = decoded_planes[0].samples.clone();
        let (_, rest) = decoded_planes.split_first_mut().unwrap();
        let (b_plane, rest2) = rest.split_first_mut().unwrap();
        let r_plane = &mut rest2[0];
        predict::inverse_decorrelate_rgb(&g_clone, &mut b_plane.samples, &mut r_plane.samples);
    }

    Ok(DecodedFrame {
        fourcc: cfg.fourcc,
        width: cfg.width,
        height: cfg.height,
        predictor,
        planes: decoded_planes,
        frame_info: parsed.frame_info,
    })
}

/// Strict variant of [`finish_decode`]: serial path only, every slice
/// decoded via [`HuffmanTable::decode_slice_strict`] so trailing
/// padding is verified zero (`spec/05` §4.3 / §8). The `plane_idx`
/// passed into the per-plane decoder lets a [`Error::NonZeroPadding`]
/// name where the violation sits.
fn finish_decode_strict(cfg: &StreamConfig, parsed: ParsedFrame<'_>) -> Result<DecodedFrame> {
    let num_slices = cfg.num_slices();
    let predictor = Predictor::from_frame_info(parsed.frame_info);

    let mut decoded_planes: Vec<DecodedPlane> = Vec::with_capacity(parsed.planes.len());
    for (plane_idx, plane) in parsed.planes.into_iter().enumerate() {
        let pw = plane.width;
        let ph = plane.height;
        let mut samples = vec![0u8; pw * ph];

        decode_plane_serial_strict(
            &plane.table,
            &plane.slice_bytes,
            num_slices,
            pw,
            ph,
            predictor,
            plane_idx,
            &mut samples,
        )?;

        decoded_planes.push(DecodedPlane {
            label: plane.label,
            width: pw as u32,
            height: ph as u32,
            samples,
        });
    }

    if cfg.fourcc.is_rgb_family() {
        let g_clone = decoded_planes[0].samples.clone();
        let (_, rest) = decoded_planes.split_first_mut().unwrap();
        let (b_plane, rest2) = rest.split_first_mut().unwrap();
        let r_plane = &mut rest2[0];
        predict::inverse_decorrelate_rgb(&g_clone, &mut b_plane.samples, &mut r_plane.samples);
    }

    Ok(DecodedFrame {
        fourcc: cfg.fourcc,
        width: cfg.width,
        height: cfg.height,
        predictor,
        planes: decoded_planes,
        frame_info: parsed.frame_info,
    })
}

/// Serial-path per-plane decode: decode each slice, then inverse-predict
/// directly into the per-slice row range of `out`.
fn decode_plane_serial(
    table: &HuffmanTable,
    slice_bytes: &[&[u8]],
    num_slices: usize,
    pw: usize,
    ph: usize,
    predictor: Predictor,
    out: &mut [u8],
) -> Result<()> {
    debug_assert_eq!(slice_bytes.len(), num_slices);
    debug_assert_eq!(out.len(), pw * ph);
    for (s, sb) in slice_bytes.iter().enumerate().take(num_slices) {
        let r_start = (ph * s) / num_slices;
        let r_end = (ph * (s + 1)) / num_slices;
        let rows = r_end - r_start;
        let n_pixels = rows * pw;
        if n_pixels == 0 {
            continue;
        }
        let residuals = table.decode_slice(sb, n_pixels)?;
        let slice_out = &mut out[r_start * pw..r_end * pw];
        predict::apply_slice(predictor, slice_out, pw, rows, &residuals);
    }
    Ok(())
}

/// Strict variant of [`decode_plane_serial`]: each slice is decoded via
/// [`HuffmanTable::decode_slice_strict`], which verifies the slice's
/// trailing word-boundary padding is zero (`spec/05` §4.3). `plane_idx`
/// is threaded through only to locate a [`Error::NonZeroPadding`].
#[allow(clippy::too_many_arguments)]
fn decode_plane_serial_strict(
    table: &HuffmanTable,
    slice_bytes: &[&[u8]],
    num_slices: usize,
    pw: usize,
    ph: usize,
    predictor: Predictor,
    plane_idx: usize,
    out: &mut [u8],
) -> Result<()> {
    debug_assert_eq!(slice_bytes.len(), num_slices);
    debug_assert_eq!(out.len(), pw * ph);
    for (s, sb) in slice_bytes.iter().enumerate().take(num_slices) {
        let r_start = (ph * s) / num_slices;
        let r_end = (ph * (s + 1)) / num_slices;
        let rows = r_end - r_start;
        let n_pixels = rows * pw;
        if n_pixels == 0 {
            continue;
        }
        let residuals = table.decode_slice_strict(sb, n_pixels, plane_idx, s)?;
        let slice_out = &mut out[r_start * pw..r_end * pw];
        predict::apply_slice(predictor, slice_out, pw, rows, &residuals);
    }
    Ok(())
}

/// Parallel-path per-plane decode: dispatch slices across a
/// `std::thread::scope` thread pool. The output buffer is split
/// row-wise (`split_at_mut`) so every thread owns a disjoint
/// `slice_rows * pw` mutable strip. Errors propagate via the join
/// — the first failing slice wins.
fn decode_plane_parallel(
    table: &HuffmanTable,
    slice_bytes: &[&[u8]],
    num_slices: usize,
    pw: usize,
    ph: usize,
    predictor: Predictor,
    out: &mut [u8],
) -> Result<()> {
    debug_assert_eq!(slice_bytes.len(), num_slices);
    debug_assert_eq!(out.len(), pw * ph);

    // Compute per-slice (r_start, rows) and split the output buffer
    // into disjoint row strips, one per slice.
    let mut strip_specs: Vec<(usize, usize)> = Vec::with_capacity(num_slices);
    for s in 0..num_slices {
        let r_start = (ph * s) / num_slices;
        let r_end = (ph * (s + 1)) / num_slices;
        strip_specs.push((r_start, r_end - r_start));
    }

    // Build a Vec of disjoint mutable strips by walking `out` and
    // splitting off each slice's `rows * pw` bytes in turn.
    let mut strips: Vec<&mut [u8]> = Vec::with_capacity(num_slices);
    let mut remaining: &mut [u8] = out;
    for &(_, rows) in &strip_specs {
        let take = rows * pw;
        let (head, tail) = remaining.split_at_mut(take);
        strips.push(head);
        remaining = tail;
    }
    debug_assert!(remaining.is_empty());

    // Bound the thread fanout. `available_parallelism` is the official
    // way to do this in std without a thread-pool crate; we cap at
    // `num_slices` because more threads than tasks is pointless.
    let par = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(num_slices)
        .max(1);

    // Use std::thread::scope so non-'static borrows of `slice_bytes` +
    // `table` are sound. Errors are collected by index then merged.
    let errors: std::sync::Mutex<Option<(usize, Error)>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        // Pair (strip, slice_idx, rows) for thread dispatch.
        let mut work: Vec<(&mut [u8], usize, usize)> = strips
            .into_iter()
            .enumerate()
            .map(|(s, strip)| (strip, s, strip_specs[s].1))
            .collect();

        // Chunk slices across `par` threads in round-robin order to
        // keep the work distribution roughly balanced even when slice
        // sizes vary.
        let chunk_size = work.len().div_ceil(par);
        let mut chunks: Vec<Vec<(&mut [u8], usize, usize)>> = Vec::with_capacity(par);
        for _ in 0..par {
            chunks.push(Vec::new());
        }
        for (i, item) in work.drain(..).enumerate() {
            let bucket = i / chunk_size.max(1);
            chunks[bucket.min(par - 1)].push(item);
        }

        let errors_ref = &errors;
        for chunk in chunks {
            scope.spawn(move || {
                for (strip, s_idx, rows) in chunk {
                    let n_pixels = rows * pw;
                    if n_pixels == 0 {
                        continue;
                    }
                    let res = table.decode_slice(slice_bytes[s_idx], n_pixels);
                    match res {
                        Ok(residuals) => {
                            predict::apply_slice(predictor, strip, pw, rows, &residuals);
                        }
                        Err(e) => {
                            let mut guard = errors_ref.lock().unwrap();
                            let take = guard
                                .as_ref()
                                .map(|(prev, _)| s_idx < *prev)
                                .unwrap_or(true);
                            if take {
                                *guard = Some((s_idx, e));
                            }
                            // Continue draining the chunk so other
                            // threads can finish; the first-failing
                            // slice's error still wins.
                        }
                    }
                }
            });
        }
    });

    if let Some((_, e)) = errors.into_inner().unwrap() {
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
    use crate::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

    fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
        let flags = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
        let extradata = Extradata {
            encoder_version: 0x0100_00f0,
            source_format_tag: *b"YV12",
            frame_info_size: 4,
            flags,
        };
        StreamConfig::new(fc, w, h, extradata).unwrap()
    }

    #[test]
    fn decode_synthesised_uly0_constant_frame() {
        // Build a constant 16×16 ULY0 plane via the encoder and roundtrip.
        let cfg = cfg_for(Fourcc::Uly0, 16, 16, 1);
        let y = vec![123u8; 16 * 16];
        let u = vec![64u8; 8 * 8];
        let v = vec![200u8; 8 * 8];
        let frame = EncodedFrame {
            fourcc: cfg.fourcc,
            width: 16,
            height: 16,
            predictor: Predictor::Left,
            num_slices: 1,
            planes: vec![
                PlaneInput { samples: y.clone() },
                PlaneInput { samples: u.clone() },
                PlaneInput { samples: v.clone() },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        assert_eq!(decoded.fourcc, Fourcc::Uly0);
        assert_eq!(decoded.predictor, Predictor::Left);
        assert_eq!(decoded.planes.len(), 3);
        assert_eq!(decoded.planes[0].samples, y);
        assert_eq!(decoded.planes[1].samples, u);
        assert_eq!(decoded.planes[2].samples, v);
    }
}
