//! Round 275 — typed `code_length_histogram` accessor + `kraft_numerator`
//! convenience on [`inspect::PlaneLayout`].
//!
//! The decode-free `peek_frame` path surfaces four scalar descriptor
//! primitives through rounds 244 / 250 / 255 / 261 — `active_symbol_count`,
//! `max_code_length`, `min_code_length`, and `min_code_length_symbol_count`.
//! Round 275 surfaces the **superset** all four are projections of: the
//! full per-length-tier code-length histogram (`spec/05` §2.2 step 2 — the
//! tiers the canonical-Huffman sort groups symbols into), as an
//! ascending-by-length `Vec<(code_length, count)>` over the active range
//! `1..=254` (`spec/05` §2.1). Plus the integer Kraft numerator
//! (`spec/05` §2.2 step 3) so a container indexer can validate prefix-code
//! completeness decode-free.
//!
//! Six properties, all computed decode-free from the on-wire 256-byte
//! Huffman descriptor (`spec/02` §4, `spec/05` §2.1, §2.2, §6.1, §6.3):
//!
//! - **`single_symbol_plane_reports_empty_histogram`** — a constant-content
//!   plane drives the `spec/05` §6.1 single-symbol path; no descriptor
//!   entry sits in the active range and the histogram is empty.
//! - **`histogram_is_strictly_ascending_with_no_zero_tiers`** — the list is
//!   strictly ascending by length and never records an absent tier as
//!   `(L, 0)`.
//! - **`histogram_projects_scalar_accessors`** — `Σ count` ==
//!   `active_symbol_count`; first/last lengths == min/max; first count ==
//!   `min_code_length_symbol_count`.
//! - **`histogram_matches_descriptor_byte_scan`** — every `(L, n)` pair
//!   equals an independent rescan of the descriptor bytes at length `L`.
//! - **`kraft_numerator_equals_denominator_on_well_formed_descriptors`** —
//!   a Kraft-complete descriptor satisfies `kraft_numerator ==
//!   2^max_code_length` (`spec/05` §2.2 step 3).
//! - **`single_length_descriptor_is_one_tier`** — a descriptor whose
//!   active symbols all share one length (`spec/05` §6.3 / §6.4) compacts
//!   to a single histogram pair saturating `active_symbol_count`.

use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
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

#[test]
fn single_symbol_plane_reports_empty_histogram() {
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
            "single-symbol plane {} histogram = {:?}, want empty",
            p.plane_idx,
            p.code_length_histogram
        );
        // The empty histogram has a zero Kraft numerator.
        assert_eq!(p.kraft_numerator(), 0);
    }
}

#[test]
fn histogram_is_strictly_ascending_with_no_zero_tiers() {
    for &fc in &ALL_FOURCCS {
        let (w, h) = (64, 32);
        let cfg = cfg_for(fc, w, h, 2);
        let bytes = encoded_high_entropy(fc, w, h, 2, Predictor::Left);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let hist = &p.code_length_histogram;
            assert!(!hist.is_empty(), "{fc:?} plane {} empty", p.plane_idx);
            for win in hist.windows(2) {
                assert!(
                    win[0].0 < win[1].0,
                    "{fc:?} plane {} not strictly ascending: {:?}",
                    p.plane_idx,
                    hist
                );
            }
            assert!(
                hist.iter()
                    .all(|&(len, n)| (1..=254).contains(&len) && n > 0),
                "{fc:?} plane {} has out-of-range or zero-count tier: {:?}",
                p.plane_idx,
                hist
            );
        }
    }
}

#[test]
fn histogram_projects_scalar_accessors() {
    for &fc in &ALL_FOURCCS {
        let (w, h) = (48, 36);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let hist = &p.code_length_histogram;
            assert!(!hist.is_empty());
            let total: u32 = hist.iter().map(|&(_, n)| n).sum();
            assert_eq!(
                total, p.active_symbol_count,
                "{fc:?} plane {} Σcount {} != active {}",
                p.plane_idx, total, p.active_symbol_count
            );
            assert_eq!(hist.first().unwrap().0, p.min_code_length);
            assert_eq!(hist.first().unwrap().1, p.min_code_length_symbol_count);
            assert_eq!(hist.last().unwrap().0, p.max_code_length);
        }
    }
}

#[test]
fn histogram_matches_descriptor_byte_scan() {
    let fc = Fourcc::Uly2;
    let (w, h) = (64, 48);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
        // Independent rescan into the same compact ascending form.
        let mut counts = [0u32; 256];
        for &b in descriptor {
            if (1..=254).contains(&b) {
                counts[b as usize] += 1;
            }
        }
        let rescan: Vec<(u8, u32)> = counts
            .iter()
            .enumerate()
            .filter(|&(_, &n)| n > 0)
            .map(|(len, &n)| (len as u8, n))
            .collect();
        assert_eq!(
            p.code_length_histogram, rescan,
            "plane {} histogram {:?} != rescan {:?}",
            p.plane_idx, p.code_length_histogram, rescan
        );
    }
}

#[test]
fn kraft_numerator_equals_denominator_on_well_formed_descriptors() {
    // Every plane the encoder emits carries a Kraft-complete descriptor
    // (`HuffmanTable::build` would reject otherwise — `spec/05` §2.2),
    // so the integer numerator equals the denominator `2^max`.
    for &fc in &ALL_FOURCCS {
        for &pred in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            let (w, h) = (80, 60);
            let cfg = cfg_for(fc, w, h, 2);
            let bytes = encoded_high_entropy(fc, w, h, 2, pred);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                if p.code_length_histogram.is_empty() {
                    // Single-symbol plane: empty histogram, zero numerator.
                    assert_eq!(p.kraft_numerator(), 0);
                    continue;
                }
                assert_eq!(
                    p.kraft_numerator(),
                    1u128 << p.max_code_length,
                    "{fc:?} {pred:?} plane {} Kraft numerator {} != 2^{}",
                    p.plane_idx,
                    p.kraft_numerator(),
                    p.max_code_length
                );
            }
        }
    }
}

#[test]
fn single_length_descriptor_is_one_tier() {
    // A two-value checkerboard under Predictor::None produces a
    // single-length descriptor on every plane (`spec/05` §6.2 / §6.3):
    // every active symbol shares one code length, so the histogram
    // compacts to a single pair whose count saturates active_symbol_count.
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
                // Two distinct values in a checkerboard → exactly two
                // active symbols, both at code length 1 under None.
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
        assert_eq!(
            p.code_length_histogram.len(),
            1,
            "plane {} histogram {:?} not single-tier",
            p.plane_idx,
            p.code_length_histogram
        );
        let (len, count) = p.code_length_histogram[0];
        assert_eq!(len, 1, "plane {} tier length {} != 1", p.plane_idx, len);
        assert_eq!(count, p.active_symbol_count);
        assert_eq!(
            count, 2,
            "plane {} active count {} != 2",
            p.plane_idx, count
        );
        assert_eq!(p.min_code_length, p.max_code_length);
        // Kraft equality holds: 2 · 2^0 == 2^1.
        assert_eq!(p.kraft_numerator(), 1u128 << p.max_code_length);
    }
}
