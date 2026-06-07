//! Round 250 — typed `max_code_length` accessor on
//! [`inspect::PlaneLayout`].
//!
//! Pins six properties for the new field, all computed decode-free
//! from the on-wire 256-byte Huffman descriptor (`spec/02` §4,
//! `spec/05` §2.1, `spec/05` §6.1, `spec/05` §7):
//!
//! - **`single_symbol_plane_reports_zero_max_code_length`** — a
//!   constant-content plane drives the `spec/05` §6.1 single-symbol
//!   path; the only non-`255` descriptor byte is the `0` sentinel,
//!   so no descriptor entry falls in the active range `1..=254` and
//!   the typed max collapses to 0.
//! - **`high_entropy_plane_reports_max_in_active_range`** — a
//!   high-entropy plane produces a multi-symbol Kraft codebook
//!   (`spec/05` §2.2 step 3) with at least one active code-length
//!   byte in `1..=254`; the typed max therefore lands in that
//!   range across every FOURCC.
//! - **`max_code_length_matches_descriptor_byte_scan`** — the
//!   field equals an independent rescan of the descriptor bytes
//!   via [`peek_frame`]'s reported `descriptor_start` offset
//!   (`spec/02` §4): `max(b in descriptor if 1..=254 contains b,
//!   else 0)`.
//! - **`max_code_length_respects_spec_05_7_1_empirical_bound`** —
//!   on the behavioural-corpus-like fixtures produced here the
//!   typed max stays at or below the `spec/05` §7.1 empirically
//!   observed maximum of `16` bits. This is an empirical sanity
//!   check on the encoder side, not a wire-format-strict bound
//!   (the `spec/05` §7.2 upper bound is `254`).
//! - **`max_code_length_bounds_codebook_index_size`** — `2 *
//!   max_code_length`-bit table-walk decoders need a buffer no
//!   larger than `2_usize.pow(max_code_length as u32)` entries.
//!   For the corpus-like fixtures here the resulting upper bound
//!   stays ≤ `2^16` per `spec/05` §7.3 — confirming a flat 64 KiB
//!   table is sufficient.
//! - **`max_code_length_geq_kraft_lower_bound_for_active_count`** —
//!   for a canonical Huffman code with `active_symbol_count >= 2`,
//!   Kraft equality (`spec/05` §2.2 step 3) gives
//!   `max_code_length >= ceil(log2(active_symbol_count))`. The
//!   round-244 active-count accessor and the round-250 max-length
//!   accessor are linked by this typed lower bound.

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
        // Predictor::None preserves a constant residual stream and
        // drives the `spec/05` §6.1 single-symbol path on every plane.
        predictor: Predictor::None,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

#[test]
fn single_symbol_plane_reports_zero_max_code_length() {
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
            p.max_code_length, 0,
            "single-symbol plane {} max = {}, want 0",
            p.plane_idx, p.max_code_length
        );
    }
}

#[test]
fn high_entropy_plane_reports_max_in_active_range() {
    // A multi-symbol Kraft codebook has at least one active
    // (`spec/05` §2.1: byte in `1..=254`) code length; the typed
    // max therefore reports a value in that range. The minimum
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
                (1..=254).contains(&p.max_code_length),
                "{:?} plane {} max = {}, want 1..=254",
                fc,
                p.plane_idx,
                p.max_code_length
            );
        }
    }
}

#[test]
fn max_code_length_matches_descriptor_byte_scan() {
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
            .max()
            .unwrap_or(0);
        assert_eq!(
            p.max_code_length, rescan,
            "plane {} max {} != rescan {}",
            p.plane_idx, p.max_code_length, rescan
        );
    }
}

#[test]
fn max_code_length_respects_spec_05_7_1_empirical_bound() {
    // `spec/05` §7.1: across the behavioural corpus the maximum
    // observed code length is 16 bits. Our encoder produces
    // realistic-distribution fixtures here, so the typed max should
    // stay ≤ 16 — this is an empirical sanity check, not a
    // wire-format-strict bound (`spec/05` §7.2 caps at 254).
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
                p.max_code_length <= 16,
                "{:?} plane {} max = {}, exceeds spec/05 §7.1 empirical bound (16)",
                fc,
                p.plane_idx,
                p.max_code_length
            );
        }
    }
}

#[test]
fn max_code_length_bounds_codebook_index_size() {
    // `spec/05` §7.3: a flat decode table needs `2^max_code_length`
    // entries; for `max_code_length <= 16` the resulting 64 KiB table
    // is acceptable. Cross-check the typed accessor against this
    // size bound on a representative spread of fixtures.
    let cases = [
        (Fourcc::Uly2, 64u32, 64u32, 4usize, Predictor::Gradient),
        (Fourcc::Uly4, 48, 48, 2, Predictor::Left),
        (Fourcc::Ulrg, 32, 32, 1, Predictor::Median),
    ];
    for (fc, w, h, slices, pred) in cases {
        let cfg = cfg_for(fc, w, h, slices);
        let bytes = encoded_high_entropy(fc, w, h, slices, pred);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let entries: usize = 1usize << p.max_code_length;
            assert!(
                entries <= 1 << 16,
                "{:?} plane {} table entries = {}, exceeds 64 KiB bound",
                fc,
                p.plane_idx,
                entries
            );
        }
    }
}

#[test]
fn max_code_length_geq_kraft_lower_bound_for_active_count() {
    // Kraft equality (`spec/05` §2.2 step 3) over an alphabet of K
    // active symbols requires the longest code length to be at least
    // `ceil(log2(K))` bits. Couples the round-244 active-count
    // accessor to the round-250 max-length accessor: high-entropy
    // planes with `active_symbol_count == K >= 2` must report
    // `max_code_length >= ceil(log2(K))`.
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
        // ceil(log2(k)) for k in 2..=256.
        let mut lower = 0u32;
        let mut v = k - 1;
        while v > 0 {
            lower += 1;
            v >>= 1;
        }
        assert!(
            u32::from(p.max_code_length) >= lower,
            "plane {} active = {}, max = {}, Kraft lower bound = {}",
            p.plane_idx,
            k,
            p.max_code_length,
            lower
        );
    }
}
