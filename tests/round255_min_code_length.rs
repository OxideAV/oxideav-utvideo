//! Round 255 — typed `min_code_length` accessor on
//! [`inspect::PlaneLayout`].
//!
//! Pins six properties for the new field, all computed decode-free
//! from the on-wire 256-byte Huffman descriptor (`spec/02` §4,
//! `spec/05` §2.1, `spec/05` §6.1, `spec/05` §2.2):
//!
//! - **`single_symbol_plane_reports_zero_min_code_length`** — a
//!   constant-content plane drives the `spec/05` §6.1 single-symbol
//!   path; the only non-`255` descriptor byte is the `0` sentinel,
//!   so no descriptor entry falls in the active range `1..=254` and
//!   the typed min collapses to 0.
//! - **`high_entropy_plane_reports_min_in_active_range`** — a
//!   high-entropy plane produces a multi-symbol Kraft codebook
//!   (`spec/05` §2.2 step 3) with at least one active code-length
//!   byte in `1..=254`; the typed min therefore lands in that range
//!   across every FOURCC.
//! - **`min_code_length_matches_descriptor_byte_scan`** — the field
//!   equals an independent rescan of the descriptor bytes via
//!   [`peek_frame`]'s reported `descriptor_start` offset (`spec/02`
//!   §4): `min(b in descriptor if 1..=254 contains b, else 0)`.
//! - **`min_code_length_not_greater_than_max_code_length`** — the
//!   round-250 max accessor and the round-255 min accessor satisfy
//!   the trivial typed invariant `min_code_length <=
//!   max_code_length`, joint with their `0`-collapse semantics on
//!   the single-symbol path.
//! - **`min_code_length_respects_kraft_upper_bound_on_active_count`** —
//!   Kraft equality (`spec/05` §2.2 step 3) over `K >= 2` active
//!   symbols requires the smallest code-length contribution
//!   `2^-min_code_length` to be `>= 1 / K`, giving the typed upper
//!   bound `min_code_length <= floor(log2(K))`. Couples the round-244
//!   active-count accessor to the round-255 min-length accessor.
//! - **`min_code_length_at_least_one_when_active_count_positive`** —
//!   the active range is `1..=254` per `spec/05` §2.1, so any plane
//!   that reports `active_symbol_count >= 1` must report
//!   `min_code_length >= 1`. Decode-free wire-format invariant.

use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::inspect::peek_frame;
use oxideav_utvideo::{Extradata, Fourcc, Predictor, StreamConfig};

/// Build a [`StreamConfig`] for `(fc, w, h, slices)` by synthesising a
/// 16-byte extradata block per `spec/01` §§4.1 / 4.3 / 4.4 directly
/// from the field layout, then parsing it with [`Extradata::parse`].
/// This deliberately uses the wire-format constructor path (no
/// reference-encoder builder helper) — the round wants a clean
/// `spec/01` §4 wire-derivation chain to avoid the parent crate's
/// pending-Hat-2-scrub `Extradata` builder name.
fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    assert!((1..=256).contains(&slices), "slices must be 1..=256");
    let source_tag = fc.as_bytes();
    let mut bytes = [0u8; 16];
    // encoder_version: `spec/01` §4.1 constant observed across the
    // behavioural corpus.
    bytes[0..4].copy_from_slice(&0x0100_00f0u32.to_le_bytes());
    // source_format_tag: `spec/01` §2.2; identical to the FourCC.
    bytes[4..8].copy_from_slice(source_tag);
    // frame_info_size: `spec/01` §4.3 — invariant 4.
    bytes[8..12].copy_from_slice(&4u32.to_le_bytes());
    // flags: `spec/01` §4.4 — bit 0 = Huffman, bits 24..31 = slices-1.
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
        // Predictor::None preserves a constant residual stream and
        // drives the `spec/05` §6.1 single-symbol path on every plane.
        predictor: Predictor::None,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

#[test]
fn single_symbol_plane_reports_zero_min_code_length() {
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
        assert_eq!(
            p.min_code_length, 0,
            "single-symbol plane {} min = {}, want 0",
            p.plane_idx, p.min_code_length
        );
    }
}

#[test]
fn high_entropy_plane_reports_min_in_active_range() {
    // A multi-symbol Kraft codebook has at least one active
    // (`spec/05` §2.1: byte in `1..=254`) code length; the typed
    // min therefore reports a value in that range. The minimum
    // possible value is `1` (a two-symbol alphabet with two length-1
    // codes satisfying Kraft exactly).
    for &fc in &[
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        let (w, h) = (64, 32);
        let cfg = cfg_for(fc, w, h, 2);
        let bytes = encoded_high_entropy(fc, w, h, 2, Predictor::Left);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(
                !p.is_single_symbol,
                "{:?} plane {} unexpectedly single-symbol",
                fc, p.plane_idx
            );
            assert!(
                (1..=254).contains(&p.min_code_length),
                "{:?} plane {} min = {}, want 1..=254",
                fc,
                p.plane_idx,
                p.min_code_length
            );
        }
    }
}

#[test]
fn min_code_length_matches_descriptor_byte_scan() {
    let fc = Fourcc::Uly4;
    let (w, h) = (48, 36);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
        let rescan: u8 = descriptor
            .iter()
            .copied()
            .filter(|&b| (1..=254).contains(&b))
            .min()
            .unwrap_or(0);
        assert_eq!(
            p.min_code_length, rescan,
            "plane {} min {} != rescan {}",
            p.plane_idx, p.min_code_length, rescan
        );
    }
}

#[test]
fn min_code_length_not_greater_than_max_code_length() {
    // Across the FOURCCs / predictors, on every well-formed plane the
    // round-250 max accessor and the round-255 min accessor satisfy
    // `min_code_length <= max_code_length`. The single-symbol path
    // (`spec/05` §6.1) collapses both to `0` so the invariant holds
    // trivially there; high-entropy planes carry the active-range
    // band `1..=254`.
    let cases = [
        (Fourcc::Uly2, 64u32, 64u32, 4usize, Predictor::Gradient),
        (Fourcc::Uly4, 32, 32, 2, Predictor::Left),
        (Fourcc::Ulrg, 16, 16, 1, Predictor::Median),
        (Fourcc::Ulra, 32, 32, 2, Predictor::Gradient),
        (Fourcc::Uly0, 64, 32, 2, Predictor::Left),
    ];
    for (fc, w, h, slices, pred) in cases {
        let cfg = cfg_for(fc, w, h, slices);
        let bytes = encoded_high_entropy(fc, w, h, slices, pred);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(
                p.min_code_length <= p.max_code_length,
                "{:?} plane {} min = {} > max = {}",
                fc,
                p.plane_idx,
                p.min_code_length,
                p.max_code_length
            );
        }
    }
    // Single-symbol case: both collapse to 0.
    let fc = Fourcc::Ulrg;
    let cfg = cfg_for(fc, 16, 16, 1);
    let bytes = encoded_constant(fc, 16, 16, 1, &[7, 11, 200]);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        assert_eq!(p.min_code_length, 0);
        assert_eq!(p.max_code_length, 0);
    }
}

#[test]
fn min_code_length_respects_kraft_upper_bound_on_active_count() {
    // Kraft equality (`spec/05` §2.2 step 3) over an alphabet of K
    // active symbols requires the smallest term `2^-min_code_length`
    // to be `>= 1 / K`, giving the typed upper bound
    // `min_code_length <= floor(log2(K))`. Couples the round-244
    // active-count accessor to the round-255 min-length accessor:
    // high-entropy planes with `active_symbol_count == K >= 2` must
    // report `min_code_length <= floor(log2(K))`.
    let fc = Fourcc::Uly4;
    let (w, h) = (64, 32);
    let cfg = cfg_for(fc, w, h, 2);
    let bytes = encoded_high_entropy(fc, w, h, 2, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let k = p.active_symbol_count;
        if k < 2 {
            continue;
        }
        // floor(log2(k)) for k in 2..=256.
        let mut upper = 0u32;
        let mut v = k;
        while v > 1 {
            upper += 1;
            v >>= 1;
        }
        assert!(
            u32::from(p.min_code_length) <= upper,
            "plane {} active = {}, min = {}, Kraft upper bound = {}",
            p.plane_idx,
            k,
            p.min_code_length,
            upper
        );
    }
}

#[test]
fn min_code_length_at_least_one_when_active_count_positive() {
    // `spec/05` §2.1: an active descriptor byte sits in `1..=254`.
    // The decode-free implication is that any plane reporting
    // `active_symbol_count >= 1` must also report `min_code_length
    // >= 1`. Conversely, the `0`-collapse path is exclusive to
    // `active_symbol_count == 0` (every other byte is the §6.1
    // sentinel `0` or the §2.1 `255` sentinel).
    let cases = [
        (Fourcc::Uly2, 64u32, 64u32, 4usize, Predictor::Gradient),
        (Fourcc::Uly4, 32, 32, 2, Predictor::Left),
        (Fourcc::Ulrg, 16, 16, 1, Predictor::Median),
        (Fourcc::Ulra, 32, 32, 2, Predictor::Gradient),
        (Fourcc::Uly0, 64, 32, 2, Predictor::Left),
    ];
    for (fc, w, h, slices, pred) in cases {
        let cfg = cfg_for(fc, w, h, slices);
        let bytes = encoded_high_entropy(fc, w, h, slices, pred);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            if p.active_symbol_count >= 1 {
                assert!(
                    p.min_code_length >= 1,
                    "{:?} plane {} active = {}, min = {} (want >= 1)",
                    fc,
                    p.plane_idx,
                    p.active_symbol_count,
                    p.min_code_length
                );
            } else {
                assert_eq!(
                    p.min_code_length, 0,
                    "{:?} plane {} active = 0 but min = {}",
                    fc, p.plane_idx, p.min_code_length
                );
            }
        }
    }
}
