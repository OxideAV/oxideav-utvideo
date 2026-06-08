//! Round 261 — typed `min_code_length_symbol_count` accessor on
//! [`inspect::PlaneLayout`].
//!
//! Pins six properties for the new field, all computed decode-free
//! from the on-wire 256-byte Huffman descriptor (`spec/02` §4,
//! `spec/05` §2.1, `spec/05` §2.2, `spec/05` §6.1, `spec/05` §6.2,
//! `spec/05` §6.3):
//!
//! - **`single_symbol_plane_reports_zero_count`** — a constant-content
//!   plane drives the `spec/05` §6.1 single-symbol path; no descriptor
//!   entry sits in the active range `1..=254` and the typed
//!   multiplicity collapses to 0 alongside the round-255 min collapse.
//! - **`high_entropy_plane_reports_positive_count`** — a high-entropy
//!   plane produces a multi-symbol Kraft codebook with `>=1` entry at
//!   the shortest length; the typed count therefore reports
//!   `>=1` across every FOURCC.
//! - **`count_matches_descriptor_byte_scan`** — the field equals an
//!   independent rescan of the descriptor bytes via [`peek_frame`]'s
//!   reported `descriptor_start` offset, counting entries equal to the
//!   reported `min_code_length` over the active range `1..=254` per
//!   `spec/05` §2.1.
//! - **`count_bounded_by_active_symbol_count`** — the multiplicity of
//!   one length tier is bounded by the total active count. Couples the
//!   round-244 active accessor to the new accessor.
//! - **`count_respects_kraft_upper_bound_on_min_length`** — Kraft
//!   equality (`spec/05` §2.2 step 3) gives the shortest-tier
//!   contribution `min_code_length_symbol_count * 2^-min_code_length
//!   <= 1`, so `min_code_length_symbol_count <= 2^min_code_length`. The
//!   `spec/05` §6.2 two-symbol `{1, 1}` case saturates this at
//!   `2 == 2^1`.
//! - **`equals_active_count_on_single_length_descriptor`** — when
//!   `min_code_length == max_code_length` and both are non-zero, the
//!   descriptor is a `spec/05` §6.3 / §6.4 single-length descriptor —
//!   every active symbol shares the one tier, so the count saturates
//!   at `active_symbol_count`.

use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::inspect::peek_frame;
use oxideav_utvideo::{Extradata, Fourcc, Predictor, StreamConfig};

/// Build a [`StreamConfig`] for `(fc, w, h, slices)` by synthesising a
/// 16-byte extradata block per `spec/01` §§4.1 / 4.3 / 4.4 directly
/// from the field layout, then parsing it with [`Extradata::parse`].
/// Uses the wire-format constructor path (no reference-encoder builder
/// helper) — keeps the test independent of pending public-API scrubs
/// on the `Extradata::ffmpeg_for` convenience constructor.
fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    assert!((1..=256).contains(&slices), "slices must be 1..=256");
    let source_tag = fc.as_bytes();
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&0x0100_00f0u32.to_le_bytes());
    bytes[4..8].copy_from_slice(source_tag);
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
        // Predictor::None preserves a constant residual stream and
        // drives the `spec/05` §6.1 single-symbol path on every plane.
        predictor: Predictor::None,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

#[test]
fn single_symbol_plane_reports_zero_count() {
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
        assert_eq!(
            p.min_code_length_symbol_count, 0,
            "single-symbol plane {} count = {}, want 0",
            p.plane_idx, p.min_code_length_symbol_count
        );
    }
}

#[test]
fn high_entropy_plane_reports_positive_count() {
    // A multi-symbol Kraft codebook has at least one active descriptor
    // entry at the minimum length, so the typed multiplicity reports
    // `>= 1` across every FOURCC.
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
                p.min_code_length_symbol_count >= 1,
                "{:?} plane {} count = {}, want >= 1",
                fc,
                p.plane_idx,
                p.min_code_length_symbol_count
            );
        }
    }
}

#[test]
fn count_matches_descriptor_byte_scan() {
    let fc = Fourcc::Uly4;
    let (w, h) = (48, 36);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
        let rescan: u32 = if p.min_code_length == 0 {
            0
        } else {
            descriptor
                .iter()
                .copied()
                .filter(|&b| b == p.min_code_length)
                .count() as u32
        };
        assert_eq!(
            p.min_code_length_symbol_count, rescan,
            "plane {} count {} != rescan {}",
            p.plane_idx, p.min_code_length_symbol_count, rescan
        );
    }
}

#[test]
fn count_bounded_by_active_symbol_count() {
    // The multiplicity of one length tier is bounded by the total
    // active count — trivially, a single tier cannot contain more
    // symbols than the union of all tiers. Pairs the round-244 typed
    // accessor with the round-261 accessor.
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
                p.min_code_length_symbol_count <= p.active_symbol_count,
                "{:?} plane {} count = {} > active = {}",
                fc,
                p.plane_idx,
                p.min_code_length_symbol_count,
                p.active_symbol_count
            );
        }
    }
}

#[test]
fn count_respects_kraft_upper_bound_on_min_length() {
    // Kraft equality (`spec/05` §2.2 step 3) over the active set
    // requires the shortest-tier contribution
    // `min_code_length_symbol_count * 2^-min_code_length` to be `<= 1`,
    // i.e. `min_code_length_symbol_count <= 2^min_code_length`. The
    // §6.2 two-symbol `{1, 1}` shape saturates this at `2 == 2^1`; the
    // general high-entropy case satisfies it with slack.
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
            if p.min_code_length == 0 {
                // No active tier — count must also be 0, already
                // covered by the dedicated zero-collapse test.
                continue;
            }
            // 2^min_code_length, computed without overflow for any
            // min in 1..=31 (the practical encoder range — the wire
            // format permits up to 254 but ffmpeg-observed code
            // lengths stay below 32 per `spec/05` §7.1).
            let kraft_bound: u64 = 1u64 << p.min_code_length.min(63);
            assert!(
                u64::from(p.min_code_length_symbol_count) <= kraft_bound,
                "{:?} plane {} count = {} > 2^min = {}",
                fc,
                p.plane_idx,
                p.min_code_length_symbol_count,
                kraft_bound
            );
        }
    }
}

#[test]
fn equals_active_count_on_single_length_descriptor() {
    // The `spec/05` §6.3 / §6.4 single-length-descriptor path produces
    // `min_code_length == max_code_length` — every active symbol sits
    // in the one length tier, so `min_code_length_symbol_count`
    // saturates at `active_symbol_count`. The §6.2 two-symbol `{1, 1}`
    // case is one observed instance (two symbols both at length 1).
    //
    // We drive this with a `spec/05` §6.2-shaped plane: an Ulrg frame
    // whose G plane carries exactly two residual values across all
    // pixels (the §6.2 fixture cited in `spec/05` §3.1.2). With
    // `Predictor::None`, a two-value-checkerboard input plane preserves
    // a two-value residual stream — the per-plane Huffman pass sees
    // exactly two active symbols at codelen 1 each.
    let fc = Fourcc::Ulrg;
    let (w, h) = (16, 16);
    let cfg = cfg_for(fc, w, h, 1);
    // Per-plane two-value checkerboards drive the §6.2 `{1, 1}` shape
    // on every plane under `Predictor::None`.
    let mut planes = Vec::with_capacity(fc.plane_count());
    let pairs: [(u8, u8); 3] = [(0, 1), (10, 200), (50, 150)];
    for &(a, b) in pairs.iter().take(fc.plane_count()) {
        let (pw, ph) = fc.plane_dim(0, w, h); // RGB planes are full-res
        let mut samples = Vec::with_capacity((pw * ph) as usize);
        for r in 0..ph {
            for c in 0..pw {
                samples.push(if (r + c) & 1 == 0 { a } else { b });
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
    let mut saw_single_length = false;
    for p in &layout.planes {
        if p.min_code_length == p.max_code_length && p.min_code_length >= 1 {
            // Single-length descriptor → count saturates at active.
            assert_eq!(
                p.min_code_length_symbol_count, p.active_symbol_count,
                "{:?} plane {} single-length descriptor: count {} != active {}",
                fc, p.plane_idx, p.min_code_length_symbol_count, p.active_symbol_count
            );
            saw_single_length = true;
        }
    }
    assert!(
        saw_single_length,
        "expected at least one single-length-descriptor plane on the §6.2 checkerboard fixture"
    );
}
