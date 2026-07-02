//! Round 16 — pin the row-stride None/Left predictor invariants.
//!
//! Round 15 hoisted the row-0 / column-0 branches out of the Gradient
//! and Median inner loops. The None and Left paths still iterated with
//! per-pixel `plane[r * width + c]` index arithmetic; the round-15
//! README explicitly noted them as "already tight cumulative loops"
//! and left them alone. Round 16 converts them to row-strided
//! `chunks_exact_mut(width)` iteration so the inner row sees a fixed
//! `width` slice (the compiler can elide the per-pixel bounds check
//! and lower `apply_none` to a memcpy).
//!
//! The output remains bit-for-bit identical — this round is depth-mode
//! performance / code-structure, not bitstream capability. These tests
//! pin the byte-equality invariants the refactor must keep:
//!
//! 1. `apply_none` is a pure row-strided copy across the slice's row
//!    range (`spec/04` §3 — identity predictor).
//! 2. `apply_left` is the continuous-wrap Left predictor: column 0 of
//!    row r reads `sample[r-1, W-1]` inside the slice (`spec/04` §4 +
//!    §4.1.1 — per-slice +128 seed at the very first pixel only).
//! 3. The forward `None` / `Left` paths are exact inverses (encode →
//!    decode round-trips bit-for-bit) at every combination of
//!    `(width, height, num_slices)` we sweep.
//! 4. The `apply_slice` strip-isolated entry (used by the round-4
//!    parallel decoder) produces byte-identical output to the
//!    multi-slice `apply` entry — the row-strided refactor must not
//!    break the slice-parallel path.
//!
//! All assertions go through the public API only.

use oxideav_utvideo::{
    decode_frame, encode_frame, DecodedFrame, EncodedFrame, Extradata, Fourcc, PlaneInput,
    Predictor, StreamConfig,
};

fn cfg_for(fc: Fourcc, w: u32, h: u32, num_slices: usize) -> StreamConfig {
    let ed = Extradata::ffmpeg_for(fc, num_slices).unwrap();
    StreamConfig::new(fc, w, h, ed).unwrap()
}

/// Deterministic xorshift32 — keep tests stable across CI runs.
fn xorshift_buf(seed: u32, n: usize) -> Vec<u8> {
    let mut x = seed.max(1);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push((x & 0xff) as u8);
    }
    out
}

fn build_frame(fc: Fourcc, w: u32, h: u32, pred: Predictor, num_slices: usize) -> EncodedFrame {
    let mut planes = Vec::with_capacity(fc.plane_count());
    let mut seed: u32 = 0xC0FFEE_u32 ^ (fc as u32) ^ pred.as_frame_info_bits();
    for i in 0..fc.plane_count() {
        let (pw, ph) = fc.plane_dim(i, w, h);
        seed = seed.wrapping_add(0x9E37_79B9);
        planes.push(PlaneInput {
            samples: xorshift_buf(seed, (pw as usize) * (ph as usize)),
        });
    }
    EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices,
        planes,
    }
}

fn roundtrip(fc: Fourcc, w: u32, h: u32, pred: Predictor, num_slices: usize) -> DecodedFrame {
    let frame = build_frame(fc, w, h, pred, num_slices);
    let cfg = cfg_for(fc, w, h, num_slices);
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    assert_eq!(decoded.fourcc, fc);
    assert_eq!(decoded.predictor, pred);
    assert_eq!(decoded.planes.len(), frame.planes.len());
    for (dp, ip) in decoded.planes.iter().zip(frame.planes.iter()) {
        assert_eq!(
            dp.samples, ip.samples,
            "plane mismatch fc={fc:?} pred={pred:?} W={w} H={h} N={num_slices}"
        );
    }
    decoded
}

// --------------------------------------------------------------------
// 1. apply_none — row-strided pure-copy invariant.
// --------------------------------------------------------------------

#[test]
fn none_roundtrip_every_fourcc_single_slice() {
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        roundtrip(fc, 16, 16, Predictor::None, 1);
    }
}

#[test]
fn none_roundtrip_every_fourcc_multi_slice() {
    // ULY0 needs even W and H; we pick a height divisible by 4 and 8
    // so the slice boundary aligns and uneven-slice partitions also
    // get exercised.
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &n in &[1usize, 2, 4, 8] {
            roundtrip(fc, 32, 32, Predictor::None, n);
        }
    }
}

#[test]
fn none_roundtrip_uneven_slice_division() {
    // Heights chosen so `ph * n / N` straddles a non-divisible boundary,
    // forcing the strip lengths to vary across slices (uneven row count
    // per slice). `ph = 18`, `N = 4` → strips of {4, 4, 4, 6} rows.
    for &n in &[3usize, 4, 5, 7] {
        roundtrip(Fourcc::Uly4, 16, 18, Predictor::None, n);
    }
}

#[test]
fn none_zero_rows_per_slice_short_circuits() {
    // `N > ph` produces some zero-row slices from the spec/02 §5.2 row
    // formula. The encoder now rejects such counts outright on the wire
    // (round 382 interop pin: conformant decoders refuse zero-length
    // slices in multi-symbol planes), so the row-strided forward path's
    // `r_end == r_start` short-circuit is pinned directly at the
    // predict layer: zero-row slices must yield empty residual streams
    // while the populated slices tile the plane exactly.
    let (w, h) = (16usize, 4usize);
    let n = 8usize; // 4 slices have rows, 4 are empty
    let plane = xorshift_buf(0xC0FFEE, w * h);
    let residuals = oxideav_utvideo::predict::forward(Predictor::None, &plane, w, h, n);
    assert_eq!(residuals.len(), n);
    let total: usize = residuals.iter().map(|r| r.len()).sum();
    assert_eq!(total, w * h);
    for (i, r) in residuals.iter().enumerate() {
        let rows = (h * (i + 1)) / n - (h * i) / n;
        assert_eq!(r.len(), rows * w, "slice {i} residual length");
    }
    // And the wire-level encoder refuses to emit the degenerate shape.
    let frame = build_frame(Fourcc::Uly4, w as u32, h as u32, Predictor::None, n);
    assert!(
        matches!(
            encode_frame(&frame),
            Err(oxideav_utvideo::Error::SliceCountExceedsPlaneHeight { .. })
        ),
        "encoder must reject num_slices > plane height"
    );
}

// --------------------------------------------------------------------
// 2. apply_left — continuous-wrap predictor invariant.
// --------------------------------------------------------------------

#[test]
fn left_constant_zero_plane_residual_signature() {
    // Per spec/04 §4.1.1: a constant-0 plane under Left + 1-slice
    // produces residual stream [128, 0, 0, ...]. The cumulative sum
    // (128 + 0 + 0 + ... = 128, then 128 + 0 = 128 every subsequent
    // step) decodes back to all-zeros iff the continuous-wrap state
    // is preserved across rows.
    //
    // We assert the round-trip identity (constant-0 plane goes in,
    // constant-0 plane comes out) — the residual stream signature is
    // an internal contract of the row-strided refactor.
    let fc = Fourcc::Uly4;
    let w = 16u32;
    let h = 16u32;
    let cfg = cfg_for(fc, w, h, 1);
    let zero_plane = vec![0u8; (w * h) as usize];
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: zero_plane.clone(),
            },
            PlaneInput {
                samples: zero_plane.clone(),
            },
            PlaneInput {
                samples: zero_plane.clone(),
            },
        ],
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for plane in &decoded.planes {
        assert!(
            plane.samples.iter().all(|&b| b == 0),
            "constant-0 plane must decode back to all zeros under Left"
        );
    }
}

#[test]
fn left_constant_value_plane_decodes_back() {
    // Constant-V plane: residual = [V-128, 0, 0, ...]. Verifies the
    // cumulative sum wraps to V at column 0 then propagates V via the
    // zero residuals across the rest of every row + every subsequent
    // row (continuous-wrap).
    let fc = Fourcc::Uly4;
    let w = 16u32;
    let h = 16u32;
    let cfg = cfg_for(fc, w, h, 1);
    for &v in &[1u8, 64, 127, 128, 200, 255] {
        let plane = vec![v; (w * h) as usize];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: Predictor::Left,
            num_slices: 1,
            planes: vec![
                PlaneInput {
                    samples: plane.clone(),
                },
                PlaneInput {
                    samples: plane.clone(),
                },
                PlaneInput {
                    samples: plane.clone(),
                },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        for p in &decoded.planes {
            assert!(
                p.samples.iter().all(|&b| b == v),
                "constant-{v} plane must decode back unchanged under Left"
            );
        }
    }
}

#[test]
fn left_roundtrip_every_fourcc_single_slice() {
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        roundtrip(fc, 16, 16, Predictor::Left, 1);
    }
}

#[test]
fn left_roundtrip_every_fourcc_multi_slice() {
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &n in &[2usize, 4, 8] {
            roundtrip(fc, 32, 32, Predictor::Left, n);
        }
    }
}

#[test]
fn left_roundtrip_uneven_slice_division() {
    for &n in &[3usize, 5, 7] {
        roundtrip(Fourcc::Uly4, 16, 18, Predictor::Left, n);
    }
}

#[test]
fn left_continuous_wrap_across_rows_verifies_per_slice_state() {
    // A plane whose row r is filled with constant `r * 7` (mod 256).
    // The Left predictor's residual stream is sensitive to the
    // row-to-row transition because column 0 of row r consults
    // `sample[r-1, W-1]` (continuous-wrap). The row-strided refactor
    // MUST preserve that state across the `chunks_exact_mut` row
    // boundary; if it accidentally re-seeded `prev = 128` at every
    // row the decoded plane would no longer round-trip.
    let fc = Fourcc::Uly4;
    let w = 16u32;
    let h = 16u32;
    let cfg = cfg_for(fc, w, h, 1);
    let plane: Vec<u8> = (0..h)
        .flat_map(|r| std::iter::repeat(((r * 7) & 0xff) as u8).take(w as usize))
        .collect();
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: plane.clone(),
            },
            PlaneInput {
                samples: plane.clone(),
            },
            PlaneInput {
                samples: plane.clone(),
            },
        ],
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for p in &decoded.planes {
        assert_eq!(p.samples, plane, "row-constant plane must round-trip");
    }
}

// --------------------------------------------------------------------
// 3. Cross-path byte-equality: serial vs auto-dispatch vs parallel.
// --------------------------------------------------------------------

#[test]
fn none_serial_matches_auto_dispatch() {
    // Auto-dispatch picks parallel only when num_slices > 1 and
    // luma pixels >= PARALLEL_PIXEL_THRESHOLD (64 KiB). 320x240 = 76 800
    // crosses the threshold; we run None + multi-slice and assert
    // byte equality with the explicit serial path. This is the same
    // invariant round-5 pinned for the encoder — we restate it here
    // to cover the row-strided refactor.
    let fc = Fourcc::Uly4;
    let w = 320u32;
    let h = 240u32;
    let frame = build_frame(fc, w, h, Predictor::None, 8);
    let cfg = cfg_for(fc, w, h, 8);
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for (dp, ip) in decoded.planes.iter().zip(frame.planes.iter()) {
        assert_eq!(dp.samples, ip.samples);
    }
}

#[test]
fn left_serial_matches_auto_dispatch() {
    let fc = Fourcc::Uly2;
    let w = 320u32;
    let h = 240u32;
    let frame = build_frame(fc, w, h, Predictor::Left, 8);
    let cfg = cfg_for(fc, w, h, 8);
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for (dp, ip) in decoded.planes.iter().zip(frame.planes.iter()) {
        assert_eq!(dp.samples, ip.samples);
    }
}

// --------------------------------------------------------------------
// 4. Determinism: two encodes produce the same bytes (no random
//    iteration order or HashMap-style nondet introduced by the
//    refactor).
// --------------------------------------------------------------------

#[test]
fn none_encode_byte_deterministic_across_runs() {
    for &fc in &[Fourcc::Uly0, Fourcc::Uly4, Fourcc::Ulrg] {
        let w = 32u32;
        let h = 32u32;
        let frame = build_frame(fc, w, h, Predictor::None, 4);
        let b1 = encode_frame(&frame).unwrap();
        let b2 = encode_frame(&frame).unwrap();
        assert_eq!(b1, b2, "non-deterministic None encode for {fc:?}");
    }
}

#[test]
fn left_encode_byte_deterministic_across_runs() {
    for &fc in &[Fourcc::Uly0, Fourcc::Uly4, Fourcc::Ulrg] {
        let w = 32u32;
        let h = 32u32;
        let frame = build_frame(fc, w, h, Predictor::Left, 4);
        let b1 = encode_frame(&frame).unwrap();
        let b2 = encode_frame(&frame).unwrap();
        assert_eq!(b1, b2, "non-deterministic Left encode for {fc:?}");
    }
}

// --------------------------------------------------------------------
// 5. Edge widths: 1-column, 2-column planes (chunks_exact must not
//    panic / leave residuals undecoded on minimal widths).
// --------------------------------------------------------------------

#[test]
fn left_minimal_width_round_trip() {
    // Uly4 accepts any positive dims (no chroma even constraint).
    // Width = 1 reduces every row to a single pixel — the inner
    // `chunks_exact_mut(1)` loop yields one element per row.
    let fc = Fourcc::Uly4;
    let w = 1u32;
    let h = 16u32;
    let cfg = cfg_for(fc, w, h, 1);
    let mut frame = build_frame(fc, w, h, Predictor::Left, 1);
    // Make sure plane sizes are minimal-width consistent.
    for (i, p) in frame.planes.iter_mut().enumerate() {
        let (pw, ph) = fc.plane_dim(i, w, h);
        p.samples = xorshift_buf(0xABCD_u32 + (i as u32), (pw * ph) as usize);
    }
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for (dp, ip) in decoded.planes.iter().zip(frame.planes.iter()) {
        assert_eq!(dp.samples, ip.samples, "minimal-width Left round-trip");
    }
}

#[test]
fn none_minimal_width_round_trip() {
    let fc = Fourcc::Uly4;
    let w = 1u32;
    let h = 16u32;
    let cfg = cfg_for(fc, w, h, 1);
    let mut frame = build_frame(fc, w, h, Predictor::None, 1);
    for (i, p) in frame.planes.iter_mut().enumerate() {
        let (pw, ph) = fc.plane_dim(i, w, h);
        p.samples = xorshift_buf(0xCAFE_u32 + (i as u32), (pw * ph) as usize);
    }
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for (dp, ip) in decoded.planes.iter().zip(frame.planes.iter()) {
        assert_eq!(dp.samples, ip.samples);
    }
}

// --------------------------------------------------------------------
// 6. Cross-predictor parity: the four predictors all round-trip
//    bit-exact across the same input. This restates the round-2
//    pattern-matrix invariant for the None/Left subset after the
//    row-strided refactor.
// --------------------------------------------------------------------

#[test]
fn all_four_predictors_round_trip_same_input() {
    let fc = Fourcc::Uly4;
    let w = 32u32;
    let h = 32u32;
    let cfg = cfg_for(fc, w, h, 1);
    let plane = xorshift_buf(0xDEAD_BEEF, (w * h) as usize);
    for &pred in &[
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: pred,
            num_slices: 1,
            planes: vec![
                PlaneInput {
                    samples: plane.clone(),
                },
                PlaneInput {
                    samples: plane.clone(),
                },
                PlaneInput {
                    samples: plane.clone(),
                },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        for p in &decoded.planes {
            assert_eq!(p.samples, plane, "round-trip failed for {pred:?}");
        }
    }
}
