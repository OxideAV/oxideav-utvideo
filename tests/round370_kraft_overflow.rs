//! Round 370 — decode-free inspector accessors must stay panic-free on
//! out-of-corpus large code-length descriptors.
//!
//! The `spec/05` §7.2 *wire-format* upper bound is large: a descriptor
//! byte may carry any code length in `1..=254` (only `0` and `255` are
//! sentinels). `spec/05` §6.2 reports max code lengths of 8–9 from
//! realistic input, so every in-corpus descriptor keeps
//! `max_code_length` tiny — but the inspector is a decode-free byte-walk
//! that does **not** reject a malformed descriptor, so it must survive
//! a hostile or corrupt byte declaring a code length up to 254.
//!
//! Two former panics are pinned here, both reachable from
//! `peek_frame(...).planes[p]` accessor calls on such a descriptor:
//!
//! 1. `PlaneLayout::kraft_numerator` computed `count << (max - len)` in
//!    a `u128`; a tier with `max - len >= 128` overflowed the 128-bit
//!    shift ("attempt to shift left with overflow"). It now uses checked
//!    shifts/adds and saturates to `u128::MAX` on overflow.
//! 2. `PlaneLayout::is_kraft_complete` compared `kraft_numerator()`
//!    against `1u128 << max_code_length`, which is itself
//!    unrepresentable once `max_code_length >= 128`. It now tests Kraft
//!    equality by a bottom-up binary-tree node merge that needs no
//!    `2^max` materialisation and is exact for any `max` in `1..=254`.
//!
//! Wall: built only from `docs/video/utvideo/spec/{02,05}` field rules;
//! no external library source, no web. The xorshift content source is a
//! self-contained PRNG with no codec provenance.

use oxideav_utvideo::decoder::decode_frame;
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::inspect::peek_frame;
use oxideav_utvideo::{Extradata, Fourcc, Predictor, StreamConfig};

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
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

/// Inject a single large code-length byte into plane 0's descriptor so
/// `max_code_length` exceeds the 128-bit shift range, then exercise every
/// numeric accessor for panic-freedom. The descriptor is no longer a
/// valid prefix code, so `is_kraft_complete()` must report `false` and
/// the full decoder must reject the frame — without ever panicking on the
/// way.
fn assert_large_codelen_accessors_panic_free(large_len: u8) {
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    // Plane 0 descriptor occupies bytes [0..256] for a single-slice ULY4
    // frame (`spec/02` §§2, 4). Force a clean single active byte at the
    // hostile length so `max_code_length == large_len`.
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    mutated[7] = large_len;

    let layout = peek_frame(&cfg, &mutated).unwrap();
    let p0 = &layout.planes[0];

    // These must not panic for any `large_len` in the wire range.
    assert_eq!(
        p0.max_code_length, large_len,
        "max_code_length should reflect the injected byte",
    );
    let num = p0.kraft_numerator();
    let complete = p0.is_kraft_complete();
    let _ = p0.unused_symbol_count();
    let _ = layout.all_planes_kraft_complete();

    // A lone short/long active byte (Kraft sum 2^-large_len, far below 1)
    // is incomplete, so the predicate is false regardless of overflow.
    assert!(
        !complete,
        "single-active-byte descriptor at len {large_len} is Kraft-incomplete",
    );

    // For lengths that keep `2^max` representable the numerator is exact
    // (a single 2^(max-len) == 2^0 == 1 term); for lengths driving the
    // term past the 128-bit range it saturates to u128::MAX. Either way
    // it must never equal the (also-saturated) completeness target, which
    // the predicate confirms above.
    if large_len < 128 {
        assert_eq!(num, 1, "single len-{large_len} byte: numerator is 2^0 == 1");
    } else {
        // 2^(max-len) with max==len is still 1 here (single byte at the
        // max tier), so this stays exact even past 128 — the saturation
        // path is exercised by the multi-tier case below.
        assert_eq!(num, 1, "single max-tier byte: 2^0 == 1, no overflow");
    }

    // The full decoder must reject the malformed descriptor cleanly
    // (no panic, a typed error).
    assert!(
        decode_frame(&cfg, &mutated).is_err(),
        "decode_frame must reject the large-codelen descriptor",
    );
}

#[test]
fn large_codelen_single_byte_accessors_panic_free() {
    // Sweep the wire range, including the 128-bit shift boundary and the
    // §7.2 maximum (254).
    for &len in &[8u8, 16, 64, 100, 127, 128, 129, 200, 253, 254] {
        assert_large_codelen_accessors_panic_free(len);
    }
}

#[test]
fn large_codelen_multi_tier_numerator_saturates() {
    // A two-tier descriptor whose deep tier drives `max - len >= 128`
    // forces the `kraft_numerator` accumulation past the 128-bit range.
    // It must saturate to u128::MAX rather than panic, and the
    // completeness predicate (node-merge, no `2^max`) must still run.
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    // One short byte (len 1) and one very long byte (len 200): the short
    // tier contributes 2^(200-1) == 2^199, well past u128.
    mutated[3] = 1;
    mutated[9] = 200;

    let layout = peek_frame(&cfg, &mutated).unwrap();
    let p0 = &layout.planes[0];
    assert_eq!(p0.max_code_length, 200);
    assert_eq!(
        p0.kraft_numerator(),
        u128::MAX,
        "the 2^199 term must saturate the numerator",
    );
    // Kraft sum here is 2^-1 + 2^-200 != 1 — incomplete — and the
    // node-merge predicate reports false without touching `2^200`.
    assert!(!p0.is_kraft_complete());
    assert!(!layout.all_planes_kraft_complete());
    assert!(decode_frame(&cfg, &mutated).is_err());
}

#[test]
fn deep_complete_descriptor_reports_true_without_overflow() {
    // A genuinely Kraft-complete descriptor whose max code length exceeds
    // the 128-bit boundary: lengths {1, 2, 3, ..., 200, 200}. The
    // unit-fraction chain 2^-1 + 2^-2 + ... + 2^-199 + 2·2^-200 telescopes
    // to exactly 1, so the node-merge predicate must report `true` even
    // though `2^200` is unrepresentable. The numerator saturates.
    let fc = Fourcc::Uly4;
    let (w, h) = (8, 8);
    let cfg = cfg_for(fc, w, h, 1);
    let bytes = encoded_high_entropy(fc, w, h, 1, Predictor::Left);
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    // Symbols 0..=198 carry lengths 1..=199; symbols 199 and 200 both
    // carry length 200 (the doubled deepest tier that closes Kraft).
    for (sym, len) in (1u16..=199u16).enumerate() {
        mutated[sym] = len as u8;
    }
    mutated[199] = 200;
    mutated[200] = 200;

    let layout = peek_frame(&cfg, &mutated).unwrap();
    let p0 = &layout.planes[0];
    assert_eq!(p0.max_code_length, 200);
    assert_eq!(p0.min_code_length, 1);
    assert!(
        p0.is_kraft_complete(),
        "the telescoping unit-fraction chain is Kraft-complete",
    );
    // The numerator overflows u128 (the len-1 tier alone is 2^199) and
    // saturates; completeness is decided by the node merge, not the
    // numerator identity.
    assert_eq!(p0.kraft_numerator(), u128::MAX);
}
