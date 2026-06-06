//! Round 244 — typed `active_symbol_count` accessor on
//! [`inspect::PlaneLayout`].
//!
//! Pins six properties for the new field + the
//! [`PlaneLayout::unused_symbol_count`] convenience method, all
//! computed decode-free from the on-wire descriptor byte slice
//! (`spec/02` §4, `spec/05` §2.1, `spec/05` §6.1):
//!
//! - **`single_symbol_plane_reports_zero_active_symbols`** — a
//!   constant-content plane drives the `code_length[s] == 0`
//!   sentinel path (`spec/05` §6.1), and the active count goes to
//!   zero (the sentinel is NOT itself an active code).
//! - **`high_entropy_plane_reports_at_least_two_active_symbols`** —
//!   high-entropy planes drive Kraft-satisfying multi-symbol
//!   codebooks (`spec/05` §2.2 step 3), which require at least two
//!   non-sentinel entries on a non-trivial alphabet.
//! - **`active_plus_unused_plus_single_equals_256`** — the typed
//!   accumulator identity over the partition of every descriptor
//!   byte by `spec/05` §2.1 (active range `1..=254`) + `spec/05`
//!   §6.1 (single-symbol sentinel `0`) + `spec/05` §2.1 (unused
//!   sentinel `255`).
//! - **`single_symbol_predictor_iff_zero_active_count_for_constant_planes`**
//!   — typed cross-check: a constant-content + Predictor::None
//!   plane is single-symbol iff the active count is 0.
//! - **`active_symbol_count_matches_descriptor_byte_scan`** —
//!   the field equals an independent re-scan of the descriptor
//!   bytes via [`peek_frame`]'s reported `descriptor_start`
//!   offset (`spec/02` §4): exactly the count of descriptor bytes
//!   in `1..=254`.
//! - **`unused_symbol_count_matches_255_byte_scan`** — the
//!   convenience method's return value equals the count of
//!   descriptor bytes equal to `255`.

use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::inspect::peek_frame;
use oxideav_utvideo::{Extradata, Fourcc, Predictor, StreamConfig};

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
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
        // Predictor::None preserves the constant content as a
        // constant residual stream — drives the spec/05 §6.1
        // single-symbol descriptor on every plane.
        predictor: Predictor::None,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

#[test]
fn single_symbol_plane_reports_zero_active_symbols() {
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
            p.active_symbol_count, 0,
            "single-symbol plane {} active count = {}, want 0",
            p.plane_idx, p.active_symbol_count
        );
    }
}

#[test]
fn high_entropy_plane_reports_at_least_two_active_symbols() {
    // High-entropy content via xorshift drives multi-symbol Kraft
    // codebooks (`spec/05` §2.2). Two active symbols is the minimum
    // for a non-trivial canonical-Huffman plane (one-bit codes for
    // a two-symbol alphabet satisfy Kraft exactly).
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
                p.active_symbol_count >= 2,
                "{:?} plane {} active = {}, want >= 2",
                fc,
                p.plane_idx,
                p.active_symbol_count
            );
            assert!(
                p.active_symbol_count <= 256,
                "{:?} plane {} active = {}, want <= 256",
                fc,
                p.plane_idx,
                p.active_symbol_count
            );
        }
    }
}

#[test]
fn active_plus_unused_plus_single_equals_256() {
    // The descriptor byte alphabet partitions into three classes per
    // `spec/05` §2.1 + §6.1: active (1..=254), unused sentinel (255),
    // single-symbol sentinel (0). Their counts must sum to the fixed
    // 256-byte descriptor length on every well-formed plane.
    let cases = [
        // Mixed-entropy: high-entropy planes.
        (
            Fourcc::Uly2,
            64u32,
            64u32,
            4usize,
            Predictor::Gradient,
            false,
        ),
        (Fourcc::Uly4, 32, 32, 2, Predictor::Left, false),
        (Fourcc::Ulrg, 16, 16, 1, Predictor::Median, false),
        // Single-symbol planes.
        (Fourcc::Uly4, 16, 16, 1, Predictor::None, true),
    ];
    for (fc, w, h, slices, pred, want_single) in cases {
        let cfg = cfg_for(fc, w, h, slices);
        let bytes = if want_single {
            let plane_count = fc.plane_count();
            let constants: Vec<u8> = (0..plane_count).map(|i| 17u8 * (i as u8 + 1)).collect();
            encoded_constant(fc, w, h, slices, &constants)
        } else {
            encoded_high_entropy(fc, w, h, slices, pred)
        };
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let single = if p.is_single_symbol { 1 } else { 0 };
            let sum = p.active_symbol_count + p.unused_symbol_count() + single;
            assert_eq!(
                sum,
                256,
                "{:?} plane {} class-sum {} != 256 (active={}, unused={}, single={})",
                fc,
                p.plane_idx,
                sum,
                p.active_symbol_count,
                p.unused_symbol_count(),
                single
            );
        }
    }
}

#[test]
fn single_symbol_predictor_iff_zero_active_count_for_constant_planes() {
    // On a constant-content frame + Predictor::None, every plane
    // takes the spec/05 §6.1 single-symbol path AND reports an
    // active count of zero. The biconditional is the typed
    // cross-check between the flag and the count.
    let fc = Fourcc::Uly4;
    let (w, h) = (24, 24);
    let cfg = cfg_for(fc, w, h, 3);
    let bytes = encoded_constant(fc, w, h, 3, &[7, 99, 211]);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        assert_eq!(
            p.is_single_symbol,
            p.active_symbol_count == 0,
            "{:?} plane {} biconditional broken (flag={}, count={})",
            fc,
            p.plane_idx,
            p.is_single_symbol,
            p.active_symbol_count
        );
    }
}

#[test]
fn active_symbol_count_matches_descriptor_byte_scan() {
    // The typed accessor must equal an independent re-scan of the
    // descriptor bytes via the reported `descriptor_start` offset.
    // Active range per `spec/05` §2.1: byte in `1..=254`.
    let fc = Fourcc::Uly4;
    let (w, h) = (48, 36);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
        let rescan = descriptor
            .iter()
            .filter(|&&b| (1..=254).contains(&b))
            .count() as u32;
        assert_eq!(
            p.active_symbol_count, rescan,
            "plane {} active count {} != re-scan {}",
            p.plane_idx, p.active_symbol_count, rescan
        );
    }
}

#[test]
fn unused_symbol_count_matches_255_byte_scan() {
    // The convenience method must equal the count of descriptor
    // bytes equal to the unused-symbol sentinel (`spec/05` §2.1).
    let fc = Fourcc::Uly2;
    let (w, h) = (64, 32);
    let cfg = cfg_for(fc, w, h, 4);
    let bytes = encoded_high_entropy(fc, w, h, 4, Predictor::Left);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
        let rescan = descriptor.iter().filter(|&&b| b == 255).count() as u32;
        assert_eq!(
            p.unused_symbol_count(),
            rescan,
            "plane {} unused count {} != 255-byte re-scan {}",
            p.plane_idx,
            p.unused_symbol_count(),
            rescan
        );
    }
}
