//! Round 291 — decode-free `is_kraft_complete` predicate on
//! [`inspect::PlaneLayout`] + the `all_planes_kraft_complete` frame-level
//! roll-up on [`inspect::FrameLayout`].
//!
//! Round 275 surfaced the per-length `code_length_histogram` and the
//! integer `kraft_numerator` (`spec/05` §2.2 step 3) on `PlaneLayout`,
//! so a container indexer can compute the `2^max`-scaled Kraft sum
//! decode-free. Round 291 closes the obvious next step: a typed
//! **completeness predicate** that folds the numerator + the
//! single-symbol flag into the `bool` a caller actually wants — "does
//! this plane's 256-byte descriptor form a complete prefix code, i.e.
//! would `HuffmanTable::build` accept it?" — without standing up a
//! `HuffmanTable`.
//!
//! Crucially, [`peek_frame`] is a pure byte-walk and does **not** reject
//! a Kraft-incomplete descriptor (unlike `decode_frame`, which trips
//! `Error::KraftViolation` at `HuffmanTable::build` time). So the
//! predicate is the decode-free oracle for "is this frame decode-ready?"
//! that the inspector path otherwise lacked.
//!
//! Eight properties, all computed decode-free from the on-wire 256-byte
//! Huffman descriptor (`spec/02` §4, `spec/05` §2.1, §2.2 step 3, §6.1):
//!
//! - **`encoder_frames_are_all_kraft_complete`** — every plane of every
//!   encoder-produced frame (all FOURCCs × all predictors) reports
//!   `true`; the frame-level roll-up agrees.
//! - **`single_symbol_plane_is_kraft_complete`** — the `spec/05` §6.1
//!   single-symbol path reports `true` (degenerate complete code) even
//!   though its histogram is empty.
//! - **`predicate_matches_decode_outcome`** — on a representative spread,
//!   `all_planes_kraft_complete()` is `true` exactly when `decode_frame`
//!   succeeds.
//! - **`kraft_incomplete_descriptor_reports_false`** — a mutated
//!   descriptor with Kraft sum `< 1` reports `false` from the inspector
//!   while `decode_frame` trips `KraftViolation`.
//! - **`kraft_excess_descriptor_reports_false`** — a mutated descriptor
//!   with Kraft sum `> 1` reports `false` likewise.
//! - **`all_unused_descriptor_reports_false`** — an all-`255` (no active
//!   byte, not single-symbol) descriptor reports `false`.
//! - **`predicate_agrees_with_kraft_numerator_identity`** — for every
//!   non-single-symbol plane, `is_kraft_complete()` iff
//!   `kraft_numerator() == 2^max_code_length`.
//! - **`single_length_descriptor_is_kraft_complete`** — a `spec/05`
//!   §6.3 two-value checkerboard (one length tier saturating Kraft)
//!   reports `true` on every plane.
//!
//! All wire layout / error conditions derived from
//! `docs/video/utvideo/spec/02` + `docs/video/utvideo/spec/05`; no
//! external library source, no web. The xorshift content source is a
//! self-contained PRNG with no codec provenance.

use oxideav_utvideo::decoder::decode_frame;
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::error::Error;
use oxideav_utvideo::inspect::peek_frame;
use oxideav_utvideo::{Extradata, Fourcc, Predictor, StreamConfig};

/// Build a [`StreamConfig`] for `(fc, w, h, slices)` directly from the
/// 16-byte extradata field layout (`spec/01` §§4.1 / 4.3 / 4.4), parsed
/// via [`Extradata::parse`] — the wire-format constructor path.
fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    assert!((1..=256).contains(&slices), "slices must be 1..=256");
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&0x0100_00f0u32.to_le_bytes());
    bytes[4..8].copy_from_slice(fc.as_bytes());
    bytes[8..12].copy_from_slice(&4u32.to_le_bytes());
    let flags: u32 = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
    bytes[12..16].copy_from_slice(&flags.to_le_bytes());
    let extradata = Extradata::parse(&bytes).unwrap();
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

fn xorshift_samples(seed: u32, n: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        v.push((state & 0xff) as u8);
    }
    v
}

fn encoded_high_entropy(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
    let plane_count = fc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for idx in 0..plane_count {
        let (pw, ph) = fc.plane_dim(idx, w, h);
        let seed = 0x1234_5678u32 ^ (idx as u32).wrapping_mul(0x9E37_79B9);
        planes.push(PlaneInput {
            samples: xorshift_samples(seed, (pw * ph) as usize),
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

fn encoded_constant(fc: Fourcc, w: u32, h: u32, slices: usize, values: &[u8]) -> Vec<u8> {
    let plane_count = fc.plane_count();
    assert_eq!(values.len(), plane_count);
    let mut planes = Vec::with_capacity(plane_count);
    for (idx, &v) in values.iter().enumerate() {
        let (pw, ph) = fc.plane_dim(idx, w, h);
        planes.push(PlaneInput {
            samples: vec![v; (pw * ph) as usize],
        });
    }
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        // Predictor::None preserves a constant residual stream and drives
        // the `spec/05` §6.1 single-symbol path on every plane.
        predictor: Predictor::None,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

const ALL_FOURCCS: [Fourcc; 5] = [
    Fourcc::Ulrg,
    Fourcc::Ulra,
    Fourcc::Uly0,
    Fourcc::Uly2,
    Fourcc::Uly4,
];

const ALL_PREDICTORS: [Predictor; 4] = [
    Predictor::None,
    Predictor::Left,
    Predictor::Gradient,
    Predictor::Median,
];

#[test]
fn encoder_frames_are_all_kraft_complete() {
    // Every plane the encoder emits carries a Kraft-complete descriptor
    // (`HuffmanTable::build` would reject otherwise — `spec/05` §2.2
    // step 3), including the `spec/05` §6.1 single-symbol planes that
    // appear when a plane collapses to a constant residual stream.
    for &fc in &ALL_FOURCCS {
        for &pred in &ALL_PREDICTORS {
            let (w, h) = (80, 60);
            let cfg = cfg_for(fc, w, h, 2);
            let bytes = encoded_high_entropy(fc, w, h, 2, pred);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                assert!(
                    p.is_kraft_complete(),
                    "{fc:?} {pred:?} plane {} not Kraft-complete (hist {:?}, num {}, max {})",
                    p.plane_idx,
                    p.code_length_histogram,
                    p.kraft_numerator(),
                    p.max_code_length,
                );
            }
            assert!(
                layout.all_planes_kraft_complete(),
                "{fc:?} {pred:?} frame roll-up not Kraft-complete",
            );
        }
    }
}

#[test]
fn single_symbol_plane_is_kraft_complete() {
    // A constant-content frame + Predictor::None drives the `spec/05`
    // §6.1 single-symbol path on every plane: the histogram is empty but
    // the lone codelen-0 sentinel is a degenerate complete prefix code,
    // so the predicate reports `true` (and the frame decode-ready).
    let fc = Fourcc::Uly4;
    let (w, h) = (16, 16);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_constant(fc, w, h, 1, &[42, 137, 200]);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        assert!(
            p.is_single_symbol,
            "plane {} not single-symbol",
            p.plane_idx
        );
        assert!(
            p.code_length_histogram.is_empty(),
            "plane {} single-symbol histogram not empty",
            p.plane_idx
        );
        assert!(
            p.is_kraft_complete(),
            "single-symbol plane {} not Kraft-complete",
            p.plane_idx
        );
    }
    assert!(layout.all_planes_kraft_complete());
    // And the frame actually decodes (the predicate is the decode-ready
    // oracle).
    assert!(decode_frame(&cfg, &bytes).is_ok());
}

#[test]
fn predicate_matches_decode_outcome() {
    // On well-formed encoder output the predicate is `true` and decode
    // succeeds; both sides of the equivalence are exercised on the
    // mutated descriptors in the negative tests below.
    for &fc in &ALL_FOURCCS {
        let (w, h) = (48, 32);
        let cfg = cfg_for(fc, w, h, 2);
        let bytes = encoded_high_entropy(fc, w, h, 2, Predictor::Left);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        let complete = layout.all_planes_kraft_complete();
        let decodes = decode_frame(&cfg, &bytes).is_ok();
        assert_eq!(
            complete, decodes,
            "{fc:?}: kraft-complete {complete} != decode-ok {decodes}",
        );
        assert!(complete, "{fc:?}: encoder frame must be complete");
    }
}

#[test]
fn kraft_incomplete_descriptor_reports_false() {
    // Mutate plane 0's descriptor to a single codelen-1 entry (Kraft sum
    // = 1/2 < 1, `spec/05` §2.2 step 3). `peek_frame` accepts it (it is a
    // byte-walk and never builds a table); the predicate reports `false`,
    // and `decode_frame` trips `KraftViolation`.
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    // Plane 0 descriptor occupies bytes [0..256] for a single-slice ULY4
    // frame (`spec/02` §§2, 4).
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    mutated[42] = 1;
    let layout = peek_frame(&cfg, &mutated).unwrap();
    assert!(
        !layout.planes[0].is_kraft_complete(),
        "incomplete plane 0 must report false (num {}, max {})",
        layout.planes[0].kraft_numerator(),
        layout.planes[0].max_code_length,
    );
    assert!(
        !layout.all_planes_kraft_complete(),
        "frame roll-up must report false",
    );
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::KraftViolation),
        "expected KraftViolation from decode_frame, got {err:?}",
    );
}

#[test]
fn kraft_excess_descriptor_reports_false() {
    // Three codelen-1 entries: Kraft sum = 3/2 > 1 (`spec/05` §2.2
    // step 3 over-subscribed tree). Inspector reports `false`;
    // `decode_frame` trips `KraftViolation`.
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    mutated[10] = 1;
    mutated[20] = 1;
    mutated[30] = 1;
    let layout = peek_frame(&cfg, &mutated).unwrap();
    assert!(
        !layout.planes[0].is_kraft_complete(),
        "excess plane 0 must report false (num {}, max {})",
        layout.planes[0].kraft_numerator(),
        layout.planes[0].max_code_length,
    );
    assert!(!layout.all_planes_kraft_complete());
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::KraftViolation),
        "expected KraftViolation, got {err:?}",
    );
}

#[test]
fn all_unused_descriptor_reports_false() {
    // An all-`255` descriptor carries no active byte and is NOT
    // single-symbol (`spec/05` §2.1 + §6.1). No prefix code exists, so
    // the predicate reports `false`. `peek_frame` surfaces it decode-free
    // (empty histogram, max_code_length 0); the full decoder would reject
    // it on the slice-data path.
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    let layout = peek_frame(&cfg, &mutated).unwrap();
    let p0 = &layout.planes[0];
    assert!(!p0.is_single_symbol, "all-unused must not be single-symbol");
    assert!(
        p0.code_length_histogram.is_empty(),
        "all-unused histogram must be empty",
    );
    assert!(
        !p0.is_kraft_complete(),
        "all-unused plane must report false",
    );
    assert!(!layout.all_planes_kraft_complete());
    // The full decoder rejects this frame (no symbol to decode the
    // non-empty slice data into).
    assert!(decode_frame(&cfg, &mutated).is_err());
}

#[test]
fn predicate_agrees_with_kraft_numerator_identity() {
    // For every non-single-symbol plane, the predicate is exactly the
    // `kraft_numerator() == 2^max_code_length` identity the round-275
    // doc-comment describes (`spec/05` §2.2 step 3). Single-symbol planes
    // are exempt (empty histogram, flag-recognised).
    for &fc in &ALL_FOURCCS {
        for &pred in &ALL_PREDICTORS {
            let (w, h) = (64, 48);
            let cfg = cfg_for(fc, w, h, 4);
            let bytes = encoded_high_entropy(fc, w, h, 4, pred);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                if p.is_single_symbol {
                    assert!(
                        p.is_kraft_complete(),
                        "{fc:?} {pred:?} single-symbol plane {} must be complete",
                        p.plane_idx,
                    );
                    continue;
                }
                let identity = !p.code_length_histogram.is_empty()
                    && p.kraft_numerator() == 1u128 << p.max_code_length;
                assert_eq!(
                    p.is_kraft_complete(),
                    identity,
                    "{fc:?} {pred:?} plane {}: predicate {} != numerator identity {}",
                    p.plane_idx,
                    p.is_kraft_complete(),
                    identity,
                );
            }
        }
    }
}

#[test]
fn single_length_descriptor_is_kraft_complete() {
    // A two-value checkerboard under Predictor::None produces a
    // single-length descriptor on every plane (`spec/05` §6.2 / §6.3):
    // exactly two active symbols, both at code length 1, so the one
    // length tier saturates Kraft (`2 · 2^0 == 2^1`). The predicate
    // reports `true`.
    let fc = Fourcc::Uly4;
    let (w, h) = (16, 16);
    let cfg = cfg_for(fc, w, h, 1);
    let plane_count = fc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for idx in 0..plane_count {
        let (pw, ph) = fc.plane_dim(idx, w, h);
        let mut samples = Vec::with_capacity((pw * ph) as usize);
        for r in 0..ph {
            for c in 0..pw {
                let v = if (r + c) % 2 == 0 { 0u8 } else { 128u8 };
                samples.push(v);
            }
        }
        planes.push(PlaneInput { samples });
    }
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: Predictor::None,
        num_slices: 1,
        planes,
    };
    let bytes = encode_frame(&frame).unwrap();
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        assert!(
            !p.is_single_symbol,
            "plane {} unexpectedly single",
            p.plane_idx
        );
        assert_eq!(p.code_length_histogram.len(), 1);
        assert!(
            p.is_kraft_complete(),
            "single-length plane {} not Kraft-complete",
            p.plane_idx,
        );
    }
    assert!(layout.all_planes_kraft_complete());
    assert!(decode_frame(&cfg, &bytes).is_ok());
}
