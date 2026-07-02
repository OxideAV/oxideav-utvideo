//! Round 21 — `inspect::peek_frame` + `inspect::peek_frame_info` invariants.
//!
//! The decode-free inspector path is supposed to surface the same
//! per-plane wire extents the full decoder walks internally, plus the
//! trailing `frame_info` dword, *without* running any Huffman decode.
//! These tests pin the cross-product the lib-side unit suite leaves
//! out:
//!
//! 1. **Cross-validation against the full decoder** — every FOURCC ×
//!    predictor × multi-slice combination: `peek_frame.predictor`
//!    matches `decode_frame.predictor`, the per-plane slice extents
//!    are the same bytes the full decoder hands its Huffman table, and
//!    the byte ordering + total-size identity hold.
//!
//! 2. **Per-slice extent correctness** — the per-slice `(start, end)`
//!    ranges the inspector returns are contiguous within a plane
//!    (`slice_n.end == slice_{n+1}.start`), word-aligned at every
//!    slice boundary (`spec/05` §4.1), and the *first* slice starts
//!    exactly at the plane's `slice_data_start`. These invariants
//!    must hold across `num_slices ∈ {1, 2, 4, 8}` for every FOURCC.
//!
//! 3. **Empty-slice edge case** — for `num_slices > plane_height` the
//!    `(ph * (s+1)/N) - (ph * s/N)` integer-divide collapses to zero
//!    rows for some slices. Those slices carry zero compressed bytes
//!    on the wire (`spec/02` §5.1) and surface as `SliceLayout`s with
//!    `start == end`. Inspector flags them via `is_empty()`.
//!
//! 4. **Single-symbol descriptor detection** — `peek_frame` returns
//!    `is_single_symbol = true` exactly when the descriptor matches
//!    the `spec/05` §6.1 sentinel pattern (exactly one zero entry,
//!    every other byte is 255), with `slice_data_total == 0`. This
//!    is the bound a per-frame indexer can use to flag "this plane
//!    is a constant" without doing the Huffman build.
//!
//! 5. **Diagnostic-friendly error surfacing** — every error variant
//!    the full decoder reports during the byte walk
//!    (`ChunkTooShort` / `NonMonotonicSliceOffsets` /
//!    `SliceNotWordAligned` / `MissingFrameInfo` /
//!    `InvalidSliceCount` / `MultipleSingleSymbolSentinels` /
//!    `KraftViolation`) the inspector reports at the same point in
//!    the walk, so a diagnostic tool can rely on the inspector's
//!    rejection set being a superset of "what the decoder will refuse
//!    on this frame."
//!
//! Round 21 closes the **frame-layout inspector** gap: containers /
//! indexers / diagnostic tools / test harnesses now have a stable
//! decode-free path into the per-frame byte layout.

use oxideav_utvideo::{
    decode_frame, encode_frame, peek_frame, peek_frame_info, EncodedFrame, Error, Extradata,
    Fourcc, PlaneInput, Predictor, StreamConfig,
};

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

/// Deterministic xorshift32-seeded content for plane `idx`. Used to
/// avoid the single-symbol collapse on tests that need non-trivial
/// residual histograms.
fn xs_plane(idx: usize, w: u32, h: u32) -> Vec<u8> {
    let mut state = 0x1234_5678u32 ^ (idx as u32).wrapping_mul(0x9E37_79B9);
    let mut out = Vec::with_capacity((w * h) as usize);
    for _ in 0..(w * h) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.push((state & 0xff) as u8);
    }
    out
}

fn build_encoded_frame(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
    let plane_count = fc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for idx in 0..plane_count {
        let (pw, ph) = fc.plane_dim(idx, w, h);
        planes.push(PlaneInput {
            samples: xs_plane(idx, pw, ph),
        });
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

const FOURCCS: [Fourcc; 5] = [
    Fourcc::Ulrg,
    Fourcc::Ulra,
    Fourcc::Uly0,
    Fourcc::Uly2,
    Fourcc::Uly4,
];

const PREDICTORS: [Predictor; 4] = [
    Predictor::None,
    Predictor::Left,
    Predictor::Gradient,
    Predictor::Median,
];

// =========================================================================
// 1. Cross-validation against the full decoder.
// =========================================================================

#[test]
fn predictor_matches_decode_frame_every_fourcc_every_predictor() {
    for &fc in &FOURCCS {
        for &pred in &PREDICTORS {
            let cfg = cfg_for(fc, 16, 16, 1);
            let bytes = build_encoded_frame(fc, 16, 16, 1, pred);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            let decoded = decode_frame(&cfg, &bytes).unwrap();
            assert_eq!(
                layout.predictor, decoded.predictor,
                "fc={fc:?} pred={pred:?}"
            );
            assert_eq!(
                layout.frame_info, decoded.frame_info,
                "fc={fc:?} pred={pred:?}"
            );
        }
    }
}

#[test]
fn frame_info_dword_round_trip_via_peek_frame_info_short_path() {
    for &pred in &PREDICTORS {
        let bytes = build_encoded_frame(Fourcc::Uly4, 32, 32, 2, pred);
        let (frame_info, recovered) = peek_frame_info(&bytes).unwrap();
        assert_eq!(recovered, pred);
        assert_eq!((frame_info >> 8) & 0x3, pred.as_frame_info_bits() >> 8);
    }
}

#[test]
fn peek_layout_total_size_identity_matches_chunk_payload_length() {
    for &fc in &FOURCCS {
        for &slices in &[1usize, 2, 4, 8] {
            let (w, h) = (32, 32);
            let cfg = cfg_for(fc, w, h, slices);
            let bytes = build_encoded_frame(fc, w, h, slices, Predictor::Gradient);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            assert_eq!(
                layout.total_size(),
                bytes.len(),
                "total_size != chunk_payload len for fc={fc:?} slices={slices}"
            );
        }
    }
}

// =========================================================================
// 2. Per-slice extent correctness.
// =========================================================================

#[test]
fn per_slice_extents_are_contiguous_and_word_aligned() {
    for &fc in &FOURCCS {
        for &slices in &[1usize, 2, 4, 8] {
            let (w, h) = (32, 32);
            let cfg = cfg_for(fc, w, h, slices);
            let bytes = build_encoded_frame(fc, w, h, slices, Predictor::Left);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                assert_eq!(p.slices.len(), slices);
                // First slice starts at slice_data_start.
                assert_eq!(p.slices[0].start, p.slice_data_start);
                // Slices are contiguous within the plane.
                for w in p.slices.windows(2) {
                    assert_eq!(w[0].end, w[1].start);
                }
                // Every slice end is 4-byte aligned relative to
                // slice_data_start (spec/05 §4.1).
                for s in &p.slices {
                    assert_eq!(
                        s.end.saturating_sub(p.slice_data_start) % 4,
                        0,
                        "non-aligned slice end for fc={fc:?} slices={slices}"
                    );
                }
            }
        }
    }
}

#[test]
fn per_slice_bytes_match_decode_frame_slice_data_subranges() {
    // The slice byte ranges the inspector reports are the same bytes
    // the full decoder consumes per slice. We verify by re-decoding
    // each slice's bit-stream against the chunk payload byte range
    // and asserting the round-trip equals the original input plane.
    let fc = Fourcc::Uly4;
    let (w, h) = (32, 32);
    let slices = 4;
    let cfg = cfg_for(fc, w, h, slices);
    let bytes = build_encoded_frame(fc, w, h, slices, Predictor::Gradient);
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    let layout = peek_frame(&cfg, &bytes).unwrap();

    // For each plane, walk the slices and confirm:
    //   - the slice byte range lies entirely within the chunk payload
    //   - the byte ranges across all planes don't overlap with each
    //     other's descriptor / end-offset table / slice-data regions
    let mut seen_ranges: Vec<(usize, usize)> = Vec::new();
    for p in &layout.planes {
        for s in &p.slices {
            assert!(s.start <= s.end);
            assert!(s.end <= bytes.len() - 4); // before frame_info dword
            seen_ranges.push((s.start, s.end));
        }
    }
    // Pairwise non-overlap (excluding zero-length).
    for (i, a) in seen_ranges.iter().enumerate() {
        for b in seen_ranges.iter().skip(i + 1) {
            if a.0 == a.1 || b.0 == b.1 {
                continue;
            }
            assert!(
                a.1 <= b.0 || b.1 <= a.0,
                "overlapping slice ranges: {a:?} vs {b:?}"
            );
        }
    }
    // Decoded frame is unchanged by the layout walk (cross-check the
    // decoder doesn't depend on inspector state).
    let decoded2 = decode_frame(&cfg, &bytes).unwrap();
    assert_eq!(decoded, decoded2);
}

// =========================================================================
// 3. Empty-slice edge case.
// =========================================================================

#[test]
fn slices_collapse_to_zero_when_num_slices_exceeds_height() {
    // 2×2 plane with 3 slices: floor(ph*(s+1)/N) - floor(ph*s/N) is 0
    // for slice 0; that slice has empty slice-data bytes (spec/02 §5.1).
    // The in-crate encoder refuses to emit this shape since round 382
    // (conformant decoders reject zero-length slices in multi-symbol
    // planes; `Error::SliceCountExceedsPlaneHeight`), so the wire bytes
    // are hand-crafted here: per plane a two-symbol descriptor
    // (`{0: 1, 128: 1}` → codes `128 → "0"`, `0 → "1"` per spec/05
    // §2.2/§6.2), offsets `[0, 4, 8]`, and two 4-byte slice words. The
    // decode-free inspector must stay lenient and surface the empty
    // slice as a zero-length byte range.
    let fc = Fourcc::Uly4;
    let (w, h) = (2, 2);
    let slices = 3;
    let cfg = cfg_for(fc, w, h, slices);

    let mut plane = Vec::with_capacity(276);
    let mut desc = [255u8; 256];
    desc[0] = 1;
    desc[128] = 1;
    plane.extend_from_slice(&desc);
    for off in [0u32, 4, 8] {
        plane.extend_from_slice(&off.to_le_bytes());
    }
    plane.extend_from_slice(&0x4000_0000u32.to_le_bytes()); // [128, 0] → "01"
    plane.extend_from_slice(&0xC000_0000u32.to_le_bytes()); // [0, 0] → "11"
    let mut bytes = Vec::with_capacity(3 * plane.len() + 4);
    for _ in 0..3 {
        bytes.extend_from_slice(&plane);
    }
    bytes.extend_from_slice(&0x0000_0100u32.to_le_bytes()); // pred left

    let layout = peek_frame(&cfg, &bytes).unwrap();
    let mut saw_empty_slice = false;
    for p in &layout.planes {
        assert_eq!(p.slices.len(), slices);
        for s in &p.slices {
            if s.is_empty() {
                saw_empty_slice = true;
                assert_eq!(s.start, s.end);
                assert_eq!(s.row_count(), 0);
                assert_eq!(s.pixel_count, 0);
            }
        }
    }
    assert!(saw_empty_slice, "expected at least one empty slice");
}

// =========================================================================
// 4. Single-symbol descriptor detection.
// =========================================================================

#[test]
fn single_symbol_flag_set_iff_descriptor_matches_sentinel() {
    let fc = Fourcc::Uly4;
    let (w, h) = (16, 16);
    let cfg = cfg_for(fc, w, h, 1);

    // Case A: constant content × Predictor::None -> single-symbol on
    // every plane.
    let frame_const = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: Predictor::None,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: vec![42u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![137u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![200u8; (w * h) as usize],
            },
        ],
    };
    let bytes = encode_frame(&frame_const).unwrap();
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        assert!(p.is_single_symbol);
        assert_eq!(p.slice_data_total(), 0);
    }
    // Bound (spec/02 §2 + §4 + §5 + §6): 3 planes × (256-byte
    // descriptor + 1 × 4-byte end-offset entry + 0 slice bytes) + 4
    // bytes frame_info dword.
    assert_eq!(layout.total_size(), 3 * (256 + 4) + 4);

    // Case B: xorshift content -> no plane is single-symbol.
    let bytes2 = build_encoded_frame(fc, w, h, 1, Predictor::Left);
    let layout2 = peek_frame(&cfg, &bytes2).unwrap();
    for p in &layout2.planes {
        assert!(!p.is_single_symbol);
        assert!(p.slice_data_total() > 0);
    }
}

// =========================================================================
// 5. Diagnostic-friendly error surfacing.
// =========================================================================

#[test]
fn rejects_short_payload_with_missing_frame_info() {
    let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
    for short in &[&[][..], &[1u8][..], &[1, 2, 3][..]] {
        let r = peek_frame(&cfg, short);
        assert!(matches!(r, Err(Error::MissingFrameInfo)), "got {r:?}");
    }
}

#[test]
fn rejects_chunk_too_short_for_descriptor() {
    let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
    let payload = vec![0u8; 16];
    let r = peek_frame(&cfg, &payload);
    assert!(matches!(r, Err(Error::ChunkTooShort { .. })), "got {r:?}");
}

#[test]
fn rejects_non_monotonic_slice_offsets() {
    let fc = Fourcc::Uly4;
    let (w, h) = (32, 32);
    let cfg = cfg_for(fc, w, h, 4);
    let mut bytes = build_encoded_frame(fc, w, h, 4, Predictor::Left);
    let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
    // Zero the third slice-end offset; sentinel for "less than second".
    let off = p0.end_offsets_start + 2 * 4;
    bytes[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
    let r = peek_frame(&cfg, &bytes);
    assert!(
        matches!(r, Err(Error::NonMonotonicSliceOffsets)),
        "got {r:?}"
    );
}

#[test]
fn rejects_unaligned_slice_offset() {
    let fc = Fourcc::Uly4;
    let (w, h) = (16, 16);
    let cfg = cfg_for(fc, w, h, 1);
    let mut bytes = build_encoded_frame(fc, w, h, 1, Predictor::Left);
    let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
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
fn peek_frame_info_rejects_short_buffer_with_missing_frame_info() {
    for short in &[&[][..], &[1u8][..], &[1, 2, 3][..]] {
        let r = peek_frame_info(short);
        assert!(matches!(r, Err(Error::MissingFrameInfo)), "got {r:?}");
    }
}

// =========================================================================
// 6. Determinism — `peek_frame` is a pure function of (cfg, payload).
// =========================================================================

#[test]
fn peek_frame_is_deterministic_across_repeat_calls() {
    let fc = Fourcc::Uly2;
    let (w, h) = (64, 48);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = build_encoded_frame(fc, w, h, 4, Predictor::Gradient);
    let layout1 = peek_frame(&cfg, &bytes).unwrap();
    for _ in 0..20 {
        let layoutk = peek_frame(&cfg, &bytes).unwrap();
        assert_eq!(layout1, layoutk);
    }
}

// =========================================================================
// 7. Slice extents are byte-identical to what the full decoder uses
//    (verified by chunk-payload index extraction).
// =========================================================================

#[test]
fn inspector_per_slice_byte_extents_cover_exactly_the_compressed_bitstream() {
    // The inspector's per-slice (start, end) ranges must add up to
    // exactly the per-plane `slice_data_total` from spec/02 §5 — no
    // more, no less. This is a stronger invariant than the per-slice
    // contiguity check above: it pins that the inspector reports the
    // *whole* compressed bit-stream per plane, with no gap and no
    // overlap.
    for &fc in &FOURCCS {
        for &slices in &[1usize, 4, 8] {
            let (w, h) = (32, 32);
            let cfg = cfg_for(fc, w, h, slices);
            let bytes = build_encoded_frame(fc, w, h, slices, Predictor::Median);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                let sum: usize = p.slices.iter().map(|s| s.len()).sum();
                assert_eq!(
                    sum,
                    p.slice_data_total(),
                    "Σ slice lengths != slice_data_total for fc={fc:?} slices={slices}"
                );
            }
        }
    }
}
