//! Frame-layout inspector for Ut Video chunk payloads.
//!
//! This module exposes a **decode-free** view of the per-frame byte
//! layout: given a [`StreamConfig`] and one chunk-payload buffer, it
//! returns the trailing `frame_info` dword, the selected predictor,
//! and the per-plane byte extents of the Huffman descriptor + the
//! slice-end-offset table + each slice's bit-stream range. No Huffman
//! decode runs and no residual buffer is allocated.
//!
//! ## Why a separate inspector path
//!
//! The full decoder ([`crate::decode_frame`]) does the same byte-walk
//! before kicking off Huffman + inverse-predict, but it only surfaces
//! the trailing `frame_info` dword on the [`crate::DecodedFrame`]
//! output — the per-plane descriptor / slice-table / slice-data byte
//! offsets are consumed internally and never reach the caller. A
//! container indexer / diagnostic tool / pre-decode statistics pass
//! often needs exactly those offsets:
//!
//! - A muxer-side indexer that wants `(predictor, slice_count,
//!   per_plane_compressed_size)` for every frame in a clip to drive
//!   bit-budget planning, without paying the Huffman-decode cost.
//! - A diagnostic that wants to point at "which plane went bad" when
//!   a downstream consumer reports a corrupt frame — the inspector's
//!   error path carries `plane_idx` on every malformed-structure
//!   variant; the full decoder ([`crate::decode_frame`]) currently
//!   surfaces the byte offset relative to the chunk payload only.
//! - A test harness that wants to round-trip wire-format invariants
//!   (offsets monotonic, word-aligned, descriptor-byte sum-rule from
//!   `spec/05` §2.2) without re-implementing the byte walk.
//!
//! The inspector path is also a clean place to put a single source of
//! truth for "what's the wire size of plane k of an arbitrary
//! ([`Fourcc`], width, height, num_slices, descriptor) tuple"
//! — useful when a caller wants to *pre-compute* an upper or lower
//! bound on a frame's encoded size before encoding runs.
//!
//! ## Spec anchors
//!
//! All extents map directly onto `spec/02` §§1, 2, 4, 5 + the
//! trailing 4-byte `frame_info` dword (`spec/02` §6 + §6.1 for the
//! predictor field). The same parse rules the full decoder uses —
//! monotonic non-decreasing slice-end offsets (`spec/02` §5),
//! 4-byte word-alignment of every slice-end value (`spec/05` §4.1),
//! and the total-length identity `payload = Σ plane_size + 4` — apply
//! verbatim. The inspector reports the precise error variant the
//! full decoder would surface at the same point in the walk.

use crate::error::{Error, Result};
use crate::fourcc::{Predictor, StreamConfig};

/// One slice's wire-format byte extent within the chunk payload,
/// plus the typed slice-header fields the partitioning rule
/// (`spec/02` §5.2) derives from `(plane_height, num_slices,
/// slice_index)` alone.
///
/// The two layers are independent (`spec/02` §5.2 final paragraph):
/// `start` / `end` carry the **compressed-byte** range pulled
/// straight from the per-plane `slice_end_offsets` table, while
/// `row_start` / `row_end` / `pixel_count` carry the **decoded-pixel**
/// extent computed from the wiki partitioning formula. Both layers
/// are decode-free — no Huffman state is needed to populate either —
/// but they answer different questions ("which bytes carry this
/// slice's bit-stream?" vs. "which plane rows does this slice
/// produce, and how many residual symbols will the Huffman pass
/// emit?").
///
/// `start <= end` always. A zero-length slice (`start == end`) is
/// legal — it arises when `num_slices > plane_height` and the
/// per-slice `floor(ph*(s+1)/N) - floor(ph*s/N)` row count collapses
/// to zero rows (`spec/02` §5.1 — empty bit-stream allowed).
///
/// `row_start <= row_end <= plane_height` always. `pixel_count` is
/// `(row_end - row_start) * plane_width` per the
/// `decode_slice_residuals(n_pixels)` argument shape in `spec/05`
/// §6 (the per-slice Huffman pass emits exactly `pixel_count`
/// residual bytes, including the trailing pad bits the bit reader
/// consumes from the word-aligned slice tail per `spec/05` §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceLayout {
    /// Absolute byte offset of this slice's first bit-stream byte.
    pub start: usize,
    /// Absolute byte offset just past this slice's last bit-stream byte.
    pub end: usize,
    /// First plane row this slice produces (inclusive), per the
    /// `spec/02` §5.2 partitioning rule
    /// `row_start = floor((plane_height * slice_index) / num_slices)`.
    pub row_start: u32,
    /// One past the last plane row this slice produces, per the
    /// `spec/02` §5.2 partitioning rule
    /// `row_end = floor((plane_height * (slice_index + 1)) / num_slices)`.
    /// Equal to `row_start` for empty slices
    /// (`num_slices > plane_height`).
    pub row_end: u32,
    /// Number of residual symbols this slice's Huffman pass emits:
    /// `(row_end - row_start) * plane_width`. Matches the
    /// `n_pixels` argument shape of
    /// [`crate::huffman::HuffmanTable::decode_slice`] (`spec/05` §6,
    /// behavioural pseudocode).
    pub pixel_count: u32,
}

impl SliceLayout {
    /// Byte length of this slice's bit-stream. `end - start`.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True iff `len() == 0`. Useful for the empty-slice edge case
    /// surfaced by `num_slices > plane_height`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Number of plane rows this slice produces: `row_end - row_start`.
    /// Equal to `pixel_count / plane_width` when `plane_width > 0`.
    #[inline]
    pub fn row_count(&self) -> u32 {
        self.row_end - self.row_start
    }
}

/// One plane's wire-format byte extents within the chunk payload.
///
/// The four sub-regions are laid out contiguously in that order:
///
/// ```text
/// descriptor: [descriptor_start .. descriptor_start + 256)
/// slice_end_offset_table: [end_offsets_start .. end_offsets_start + 4*num_slices)
/// slice data: [slice_data_start .. slice_data_start + slice_data_total)
/// ```
///
/// where `slice_data_total = slices[num_slices - 1].end - slice_data_start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Wire-format plane index (0..plane_count). Plane 0 is the luma
    /// plane (YUV) or G plane (RGB[A]) per `spec/02` §3.
    pub plane_idx: usize,
    /// Plane width in samples (per-FourCC chroma-subsampled for U/V).
    pub width: u32,
    /// Plane height in samples (per-FourCC chroma-subsampled for U/V).
    pub height: u32,
    /// Absolute byte offset of this plane's 256-byte Huffman code-length
    /// descriptor (`spec/02` §4).
    pub descriptor_start: usize,
    /// Absolute byte offset of the slice-end-offset table
    /// (`spec/02` §5). Length is `4 * num_slices` bytes.
    pub end_offsets_start: usize,
    /// Absolute byte offset of the first slice's bit-stream
    /// (`spec/02` §5 + `spec/05` §4).
    pub slice_data_start: usize,
    /// Per-slice byte extents (length `num_slices`). The end-offsets
    /// table is parsed and converted to absolute payload offsets here.
    pub slices: Vec<SliceLayout>,
    /// Whether the Huffman descriptor encodes a single-symbol plane
    /// (sentinel: code-length 0 appears for exactly one symbol, all
    /// other code-lengths are 255). `spec/05` §6.1. The plane carries
    /// zero slice-data bytes when this is true.
    pub is_single_symbol: bool,
    /// Decode-free count of symbols carrying an explicit code length
    /// in this plane's 256-byte descriptor — i.e. the count of
    /// `code_length[s]` entries with value in the active range
    /// `1..=254` per `spec/05` §2.1 (entries with value `0` are the
    /// single-symbol sentinel from `spec/05` §6.1, entries with value
    /// `255` are the unused-symbol sentinel from `spec/05` §2.1).
    ///
    /// Range: `0..=256`. Two well-formed shapes the decoder accepts:
    ///
    /// - `0` — paired with `is_single_symbol == true`, the plane
    ///   carries a single repeated symbol and `slice_data_total == 0`
    ///   (`spec/05` §6.1). The lone `code_length[s] == 0` entry is
    ///   the sentinel, NOT counted as an active code.
    /// - `2..=256` — the plane has a multi-symbol canonical Huffman
    ///   codebook satisfying the Kraft equality
    ///   `Σ 2^(-code_length[s]) == 1` over the active set
    ///   (`spec/05` §2.1 + §2.2).
    ///
    /// `1` is structurally rejectable at build time (a single
    /// non-sentinel length cannot satisfy Kraft equality on a
    /// non-trivial alphabet, `spec/05` §2.2), but `peek_frame`
    /// remains a decode-free byte-walk and surfaces the raw count
    /// here — `HuffmanTable::build` is what enforces Kraft
    /// (`spec/05` §2.2 step 3, surfaced as `Error::KraftViolation`).
    pub active_symbol_count: u32,
}

impl PlaneLayout {
    /// Total wire bytes occupied by this plane:
    /// `256 + 4 * num_slices + slice_data_total`.
    pub fn total_size(&self) -> usize {
        let slice_total = self
            .slices
            .last()
            .map(|s| s.end.saturating_sub(self.slice_data_start))
            .unwrap_or(0);
        256 + 4 * self.slices.len() + slice_total
    }

    /// Sum of slice bit-stream byte lengths (the `slice_data_total`
    /// from `spec/02` §5). Always a multiple of 4 for a well-formed
    /// plane (`spec/05` §4.1).
    pub fn slice_data_total(&self) -> usize {
        self.slices
            .last()
            .map(|s| s.end.saturating_sub(self.slice_data_start))
            .unwrap_or(0)
    }

    /// Total residual-symbol count across every slice of this plane.
    /// Equal to `width * height` for any well-formed
    /// [`PlaneLayout`] — the per-slice partitioning rule
    /// (`spec/02` §5.2) covers `[0, plane_height)` with no overlap
    /// and no gap, so `Σ slice.pixel_count == plane_width *
    /// plane_height`. Useful as a typed cross-check against the
    /// header-derived plane size before any Huffman pass runs.
    pub fn total_pixels(&self) -> u64 {
        self.slices.iter().map(|s| u64::from(s.pixel_count)).sum()
    }

    /// Number of bits in the unused-symbol set: `256 -
    /// active_symbol_count - 1` when [`is_single_symbol`] is true,
    /// `256 - active_symbol_count` otherwise. Mirrors the count of
    /// `code_length[s] == 255` sentinel entries in the descriptor
    /// per `spec/05` §2.1 (the "symbol unused, no code is assigned"
    /// bullet). Useful as a typed cross-check: an entropy-coding
    /// audit can confirm
    /// `active_symbol_count + unused_symbol_count + (single? 1 : 0)
    /// == 256` against the on-wire descriptor without a second pass
    /// over the byte slice.
    ///
    /// [`is_single_symbol`]: PlaneLayout::is_single_symbol
    pub fn unused_symbol_count(&self) -> u32 {
        let single = if self.is_single_symbol { 1 } else { 0 };
        256 - self.active_symbol_count - single
    }
}

/// Decode-free per-frame layout view of a chunk payload.
///
/// Produced by [`peek_frame`]. Carries the per-plane byte extents,
/// the trailing `frame_info` dword, and the predictor decoded from
/// bits 8..9 of `frame_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLayout {
    /// Per-plane wire extents in on-wire plane order (`spec/02` §3).
    pub planes: Vec<PlaneLayout>,
    /// Trailing 4-byte `frame_info` dword (`spec/02` §6). Bits 8..9
    /// carry the predictor; the other bits surface here verbatim for
    /// diagnostics.
    pub frame_info: u32,
    /// Predictor selected by `frame_info` bits 8..9 (`spec/02` §6.1).
    pub predictor: Predictor,
    /// Number of slices per plane (from `cfg`, surfaced here for
    /// convenience).
    pub num_slices: usize,
}

impl FrameLayout {
    /// Total compressed bit-stream byte count across all planes
    /// (`spec/02` §§2, 5). Excludes the 256-byte descriptors, the
    /// per-plane slice-end-offset tables, and the trailing
    /// `frame_info` dword.
    pub fn total_slice_data_bytes(&self) -> usize {
        self.planes.iter().map(|p| p.slice_data_total()).sum()
    }

    /// Total compressed bytes attributable to the codec wire format
    /// (i.e. the chunk payload length). Identity:
    /// `total_size == Σ plane_total_size + 4`. Useful as a cross-check
    /// against the chunk-payload byte count the demuxer hands us.
    pub fn total_size(&self) -> usize {
        self.planes.iter().map(|p| p.total_size()).sum::<usize>() + 4
    }
}

/// Lightweight peek at the trailing 4-byte `frame_info` dword + the
/// predictor it selects.
///
/// This does NOT validate any per-plane structure — the caller MUST
/// already trust the chunk payload is at least 4 bytes long. Returns
/// `Error::MissingFrameInfo` if the chunk payload is shorter than 4
/// bytes.
///
/// `frame_info` bit layout per `spec/02` §6.1:
///
/// - bits 0..7: reserved (FFmpeg writes 0).
/// - bits 8..9: predictor mode (0 = none, 1 = left, 2 = gradient,
///   3 = median).
/// - bits 10..31: reserved.
pub fn peek_frame_info(chunk_payload: &[u8]) -> Result<(u32, Predictor)> {
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let n = chunk_payload.len();
    let frame_info = u32::from_le_bytes(chunk_payload[n - 4..n].try_into().unwrap());
    Ok((frame_info, Predictor::from_frame_info(frame_info)))
}

/// Decode-free walk over a chunk payload that returns the per-plane
/// + per-slice byte extents and the trailing `frame_info` dword.
///
/// Runs the same parse rules [`crate::decode_frame`] uses (descriptor
/// length, slice-end-offset monotonicity, slice-end word-alignment,
/// total-length identity), surfacing the same `Error` variants the
/// full decoder would, but never builds a `HuffmanTable` and never
/// allocates a residual buffer.
///
/// Complexity is `O(plane_count * num_slices)` — one pass over the
/// chunk payload's descriptor + end-offset tables, no per-pixel work.
///
/// Use cases:
///
/// - Container indexing / pre-decode statistics.
/// - Diagnostic tooling (which plane carries the most compressed
///   bytes; is plane k single-symbol; which slice is empty).
/// - Test harnesses pinning wire-format invariants.
pub fn peek_frame(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<FrameLayout> {
    let num_slices = cfg.num_slices();
    if num_slices == 0 {
        return Err(Error::InvalidSliceCount);
    }
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let frame_info_off = chunk_payload.len() - 4;

    let mut offset = 0usize;
    let plane_count = cfg.fourcc.plane_count();
    let mut planes: Vec<PlaneLayout> = Vec::with_capacity(plane_count);

    for plane_idx in 0..plane_count {
        let (pw, ph) = cfg.fourcc.plane_dim(plane_idx, cfg.width, cfg.height);

        // 256-byte Huffman descriptor.
        let descriptor_start = offset;
        if offset + 256 > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: 256,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let descriptor = &chunk_payload[offset..offset + 256];
        offset += 256;

        // Single-symbol detection per spec/05 §6.1: exactly one entry
        // is 0 (the sentinel symbol) and every other entry is 255
        // (the sentinel "unused").
        let zero_count = descriptor.iter().filter(|&&b| b == 0).count();
        let unused_count = descriptor.iter().filter(|&&b| b == 255).count();
        let is_single_symbol = zero_count == 1 && unused_count == 255;
        // Active-symbol count per spec/05 §2.1: entries with value
        // in the range 1..=254 carry an explicit code length and
        // join the canonical-Huffman alphabet. The complement
        // (zero_count + unused_count) covers both sentinels;
        // descriptor.len() is constant 256, so the subtraction is
        // well-defined and produces a u32-friendly value in 0..=256.
        let active_symbol_count = (256 - zero_count - unused_count) as u32;

        // Slice-end-offsets table.
        let end_offsets_start = offset;
        let table_bytes = num_slices * 4;
        if offset + table_bytes > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: table_bytes,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut end_offsets = Vec::with_capacity(num_slices);
        for s in 0..num_slices {
            let v = u32::from_le_bytes(
                chunk_payload[offset + 4 * s..offset + 4 * s + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            end_offsets.push(v);
        }
        offset += table_bytes;

        // Monotonicity + word-alignment validation per spec/02 §5 +
        // spec/05 §4.1. Surfaces the same `Error` variant the full
        // decoder would.
        let mut prev = 0usize;
        for &v in &end_offsets {
            if v < prev {
                return Err(Error::NonMonotonicSliceOffsets);
            }
            if v % 4 != 0 {
                return Err(Error::SliceNotWordAligned(v));
            }
            prev = v;
        }
        let slice_data_total = *end_offsets.last().unwrap();
        let slice_data_start = offset;

        if offset + slice_data_total > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: slice_data_total,
                have: frame_info_off.saturating_sub(offset),
            });
        }

        // Build per-slice absolute extents + decode-free row range
        // per `spec/02` §5.2:
        //   row_start[s] = floor((plane_height * s) / num_slices)
        //   row_end[s]   = floor((plane_height * (s + 1)) / num_slices)
        // The product `plane_height * num_slices` fits in u64 in every
        // reachable case (plane_height capped by `Hp <= 65535 *
        // chroma-step` per `spec/01` §4.4.1, num_slices `<= 256` per
        // `spec/02` §5.3 — the round-241 max product is well below
        // u64::MAX), but we widen to u64 explicitly so the typed
        // accessor is overflow-safe on the architectural upper bound.
        let mut slices: Vec<SliceLayout> = Vec::with_capacity(num_slices);
        let mut prev_rel = 0usize;
        let ph64 = u64::from(ph);
        let ns64 = num_slices as u64;
        let pw32 = pw;
        for (s_idx, &end_rel) in end_offsets.iter().enumerate() {
            let row_start = (ph64 * s_idx as u64) / ns64;
            let row_end = (ph64 * (s_idx as u64 + 1)) / ns64;
            let row_count = row_end - row_start;
            slices.push(SliceLayout {
                start: slice_data_start + prev_rel,
                end: slice_data_start + end_rel,
                row_start: row_start as u32,
                row_end: row_end as u32,
                pixel_count: row_count as u32 * pw32,
            });
            prev_rel = end_rel;
        }
        offset += slice_data_total;

        planes.push(PlaneLayout {
            plane_idx,
            width: pw,
            height: ph,
            descriptor_start,
            end_offsets_start,
            slice_data_start,
            slices,
            is_single_symbol,
            active_symbol_count,
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

    Ok(FrameLayout {
        planes,
        frame_info,
        predictor: Predictor::from_frame_info(frame_info),
        num_slices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
    use crate::fourcc::{Extradata, Fourcc};

    fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
        let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
        StreamConfig::new(fc, w, h, extradata).unwrap()
    }

    fn encoded_for(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
        let plane_count = fc.plane_count();
        let mut planes = Vec::with_capacity(plane_count);
        for idx in 0..plane_count {
            let (pw, ph) = fc.plane_dim(idx, w, h);
            // Mildly non-trivial content to avoid the single-symbol
            // collapse on every test; xorshift seeded by plane index.
            let mut state = 0x1234_5678u32 ^ (idx as u32).wrapping_mul(0x9E37_79B9);
            let mut samples = Vec::with_capacity((pw * ph) as usize);
            for _ in 0..(pw * ph) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                samples.push((state & 0xff) as u8);
            }
            planes.push(PlaneInput { samples });
        }
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: pred,
            num_slices: slices,
            planes,
        };
        encode_frame(&frame).unwrap()
    }

    #[test]
    fn peek_frame_info_recovers_predictor_from_trailing_dword() {
        for &pred in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            let bytes = encoded_for(Fourcc::Uly2, 16, 16, 1, pred);
            let (frame_info, recovered) = peek_frame_info(&bytes).unwrap();
            assert_eq!(recovered, pred);
            assert_eq!((frame_info >> 8) & 0x3, pred.as_frame_info_bits() >> 8);
        }
    }

    #[test]
    fn peek_frame_info_rejects_short_buffer() {
        for short in &[&[][..], &[1u8][..], &[1, 2, 3][..]] {
            let r = peek_frame_info(short);
            assert!(matches!(r, Err(Error::MissingFrameInfo)));
        }
    }

    #[test]
    fn peek_frame_layout_round_trips_every_fourcc_single_slice() {
        for &fc in &[
            Fourcc::Ulrg,
            Fourcc::Ulra,
            Fourcc::Uly0,
            Fourcc::Uly2,
            Fourcc::Uly4,
        ] {
            let (w, h) = (16, 16);
            let cfg = cfg_for(fc, w, h, 1);
            let bytes = encoded_for(fc, w, h, 1, Predictor::Left);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            // Plane count matches FOURCC.
            assert_eq!(layout.planes.len(), fc.plane_count());
            // Total-size identity: Σ plane_total_size + 4 == payload len.
            assert_eq!(layout.total_size(), bytes.len());
            // num_slices matches.
            assert_eq!(layout.num_slices, 1);
            for (i, p) in layout.planes.iter().enumerate() {
                assert_eq!(p.plane_idx, i);
                let (pw, ph) = fc.plane_dim(i, w, h);
                assert_eq!(p.width, pw);
                assert_eq!(p.height, ph);
                assert_eq!(p.slices.len(), 1);
                // Slice extents are non-empty for these high-entropy planes.
                assert!(!p.slices[0].is_empty());
                // Slice ends on a 4-byte boundary.
                assert_eq!(p.slices[0].end.saturating_sub(p.slice_data_start) % 4, 0);
                // Wire-byte ordering: descriptor < end_offsets < slice_data.
                assert!(p.descriptor_start < p.end_offsets_start);
                assert!(p.end_offsets_start < p.slice_data_start);
                assert!(p.slice_data_start <= p.slices[0].start);
            }
        }
    }

    #[test]
    fn peek_frame_layout_round_trips_multi_slice() {
        let fc = Fourcc::Uly4;
        let (w, h) = (32, 32);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        assert_eq!(layout.num_slices, 4);
        assert_eq!(layout.predictor, Predictor::Gradient);
        for p in &layout.planes {
            assert_eq!(p.slices.len(), 4);
            // Slices are contiguous: end_n == start_{n+1}.
            for w in p.slices.windows(2) {
                assert_eq!(w[0].end, w[1].start);
            }
            // First slice starts at slice_data_start.
            assert_eq!(p.slices[0].start, p.slice_data_start);
        }
        // Total-size identity holds for multi-slice frames too.
        assert_eq!(layout.total_size(), bytes.len());
    }

    #[test]
    fn peek_frame_layout_matches_decode_frame_predictor_and_frame_info() {
        for &fc in &[Fourcc::Ulrg, Fourcc::Uly2, Fourcc::Uly4] {
            for &pred in &[
                Predictor::None,
                Predictor::Left,
                Predictor::Gradient,
                Predictor::Median,
            ] {
                let cfg = cfg_for(fc, 16, 16, 1);
                let bytes = encoded_for(fc, 16, 16, 1, pred);
                let layout = peek_frame(&cfg, &bytes).unwrap();
                let decoded = crate::decode_frame(&cfg, &bytes).unwrap();
                assert_eq!(layout.predictor, decoded.predictor);
                assert_eq!(layout.frame_info, decoded.frame_info);
            }
        }
    }

    #[test]
    fn peek_frame_detects_single_symbol_descriptor() {
        // A constant-content plane encodes via a single-symbol Huffman
        // descriptor (slice_data_total == 0) per spec/05 §6.1.
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![42u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![137u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![200u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            // Predictor::None preserves the constant content as a
            // constant residual stream — every residual byte is the
            // same value, hitting the single-symbol descriptor path
            // (`spec/05` §6.1). Other predictors emit a `+128` seed
            // residual at the per-slice row-0 col-0, breaking the
            // single-symbol invariant.
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        // Every plane is single-symbol -> empty slice + flag set.
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert_eq!(p.slice_data_total(), 0);
            assert!(p.slices[0].is_empty());
        }
        // Total payload is the bound `3 * (256 + 4 * 1) + 4 = 784` bytes
        // (spec/02 §2 + §4 + §5 + §6: 3 planes × (256-byte descriptor +
        // 1 × 4-byte end-offset entry + 0 slice bytes) + trailing 4-byte
        // frame_info dword).
        assert_eq!(layout.total_size(), 3 * (256 + 4) + 4);
    }

    #[test]
    fn peek_frame_rejects_missing_frame_info() {
        let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
        for short in &[&[][..], &[1u8, 2, 3][..]] {
            let r = peek_frame(&cfg, short);
            assert!(matches!(r, Err(Error::MissingFrameInfo)));
        }
    }

    #[test]
    fn peek_frame_rejects_chunk_too_short_for_descriptor() {
        let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
        // Buffer is 16 bytes; 4 reserved for frame_info, leaving 12
        // bytes — fewer than the 256-byte descriptor of plane 0.
        let payload = vec![0u8; 16];
        let r = peek_frame(&cfg, &payload);
        assert!(matches!(r, Err(Error::ChunkTooShort { .. })));
    }

    #[test]
    fn peek_frame_rejects_non_monotonic_slice_offsets() {
        // Build a real encoded frame, then mutate one slice-end offset
        // backwards. peek_frame surfaces NonMonotonicSliceOffsets at
        // the same point the full decoder would.
        let fc = Fourcc::Uly4;
        let (w, h) = (32, 32);
        let cfg = cfg_for(fc, w, h, 4);
        let mut bytes = encoded_for(fc, w, h, 4, Predictor::Left);
        let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
        // Mutate the third slice-end offset to a value below the second.
        let off = p0.end_offsets_start + 2 * 4;
        bytes[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        let r = peek_frame(&cfg, &bytes);
        assert!(
            matches!(r, Err(Error::NonMonotonicSliceOffsets)),
            "got {r:?}"
        );
    }

    #[test]
    fn peek_frame_rejects_unaligned_slice_offset() {
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let mut bytes = encoded_for(fc, w, h, 1, Predictor::Left);
        let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
        // Bump the single end-offset by 1 — no longer a multiple of 4.
        let existing = u32::from_le_bytes(
            bytes[p0.end_offsets_start..p0.end_offsets_start + 4]
                .try_into()
                .unwrap(),
        );
        let bumped = (existing + 1).to_le_bytes();
        bytes[p0.end_offsets_start..p0.end_offsets_start + 4].copy_from_slice(&bumped);
        let r = peek_frame(&cfg, &bytes);
        assert!(matches!(r, Err(Error::SliceNotWordAligned(_))), "got {r:?}");
    }

    #[test]
    fn peek_frame_slice_data_total_matches_plane_compressed_bytes() {
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        // total_slice_data_bytes equals chunk size minus descriptors,
        // end-offset tables, and the trailing frame_info dword.
        let overhead = layout.planes.len() * 256 + layout.planes.len() * 4 * layout.num_slices + 4;
        assert_eq!(layout.total_slice_data_bytes(), bytes.len() - overhead);
    }
}
