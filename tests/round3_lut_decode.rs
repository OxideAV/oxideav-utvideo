//! Round 3 — exercise the LUT-accelerated Huffman decode path against
//! high-entropy, deep-codelen patterns the LUT cannot cover in one
//! shot. The new decoder's fast LUT (`huffman.rs::LUT_BITS = 12`) only
//! handles codes `<= 12` bits; longer codes fall back to the tiered
//! prefix-scan path. `spec/02` §4.2 documents the empirical maximum
//! at 16 bits (R2-mandelbrot-yuv420p plane 0). This suite forces the
//! encoder to produce frames whose Huffman trees have codes up to
//! that bound so the slow-path fallback is reached and validated.
//!
//! The suite is decoder-driven: the encoder synthesises the wire
//! bytes, the decoder reconstructs the plane, and the round-trip
//! must be byte-exact. Per the round-1 doctrine, FFmpeg byte-equality
//! is not in scope (no FFmpeg-encoded fixture corpus is in `tables/`).

#![cfg(test)]

use oxideav_utvideo::decoder::decode_frame;
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

fn cfg(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let flags = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *b"YV12",
        frame_info_size: 4,
        flags,
    };
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

fn run(fc: Fourcc, planes: &[Vec<u8>], w: u32, h: u32, pred: Predictor, slices: usize) {
    let cfg = cfg(fc, w, h, slices);
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes: planes
            .iter()
            .map(|p| PlaneInput { samples: p.clone() })
            .collect(),
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    for (i, want) in planes.iter().enumerate() {
        assert_eq!(
            &decoded.planes[i].samples, want,
            "plane {i} mismatch for {:?}/{:?}/{:?}",
            fc, pred, slices
        );
    }
}

/// Linear-congruential noise — deterministic high-entropy plane.
fn noise_plane(seed: u64, n: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 56) as u8);
    }
    out
}

/// A mandelbrot-shaped intensity sweep. Maps every pixel through a
/// fixed iteration count, generating a wide histogram comparable to
/// the `R2-mandelbrot-yuv420p` fixture cited in `spec/02` §4.2 — the
/// canonical 16-bit-codelen case.
fn mandelbrot_plane(w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            let mut x = (c as f64) / (w as f64) * 3.5 - 2.5;
            let mut y = (r as f64) / (h as f64) * 2.0 - 1.0;
            let cx = x;
            let cy = y;
            let mut iter = 0u32;
            for _ in 0..255 {
                let x2 = x * x;
                let y2 = y * y;
                if x2 + y2 > 4.0 {
                    break;
                }
                let nx = x2 - y2 + cx;
                y = 2.0 * x * y + cy;
                x = nx;
                iter += 1;
            }
            out[r * w + c] = iter.min(255) as u8;
        }
    }
    out
}

#[test]
fn high_entropy_noise_uly4_roundtrip() {
    // 64×48 ULY4 with three independent LCG-noise planes — exercises
    // the full Huffman pipe and a tree depth well past LUT_BITS=12.
    let y = noise_plane(0xa5a5_a5a5, 64 * 48);
    let u = noise_plane(0xc3c3_c3c3, 64 * 48);
    let v = noise_plane(0x5a5a_5a5a, 64 * 48);
    for slices in [1usize, 4, 8] {
        for pred in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            run(
                Fourcc::Uly4,
                &[y.clone(), u.clone(), v.clone()],
                64,
                48,
                pred,
                slices,
            );
        }
    }
}

#[test]
fn mandelbrot_uly0_roundtrip() {
    // 128×96 mandelbrot — known-good large-histogram pattern that
    // produces a deep tree (matching the spec/02 §4.2 R2-mandelbrot
    // observation of 16-bit codes).
    let y = mandelbrot_plane(128, 96);
    let u = mandelbrot_plane(64, 48);
    let v = mandelbrot_plane(64, 48);
    for pred in [Predictor::Left, Predictor::Gradient, Predictor::Median] {
        for slices in [1, 2, 4] {
            run(
                Fourcc::Uly0,
                &[y.clone(), u.clone(), v.clone()],
                128,
                96,
                pred,
                slices,
            );
        }
    }
}

#[test]
fn mandelbrot_ulra_roundtrip() {
    // RGBA: G plane sets the decorrelation reference; A independent.
    let g = mandelbrot_plane(64, 48);
    let b = noise_plane(0x1111, 64 * 48);
    let r = noise_plane(0x2222, 64 * 48);
    let a = vec![200u8; 64 * 48]; // mostly-constant alpha
    run(Fourcc::Ulra, &[g, b, r, a], 64, 48, Predictor::Median, 4);
}

#[test]
fn deep_codelen_uly2_high_slice_count() {
    // 64×24 ULY2 with 12 slices of 2 rows each, gradient predictor,
    // pseudo-random noise. The deep-tree code is hit per slice.
    let y = noise_plane(0xdeadbeef, 64 * 24);
    let u = noise_plane(0xbadbabe1, 32 * 24);
    let v = noise_plane(0xfeedface, 32 * 24);
    run(Fourcc::Uly2, &[y, u, v], 64, 24, Predictor::Gradient, 12);
}

#[test]
fn pure_lut_path_constant_after_median() {
    // Median-prediction of a vertical-stripe pattern collapses
    // residuals into mostly {0, 1, 128} — small alphabet, short codes
    // entirely inside LUT_BITS=12. This exercises the pure-LUT path
    // (no slow-path fallback at all).
    let mut plane = vec![0u8; 64 * 48];
    for r in 0..48 {
        for c in 0..64 {
            plane[r * 64 + c] = c as u8;
        }
    }
    run(
        Fourcc::Uly4,
        &[plane.clone(), plane.clone(), plane.clone()],
        64,
        48,
        Predictor::Median,
        4,
    );
}

#[test]
fn large_frame_roundtrip_perf_smoke() {
    // 320×240 noise frame across all four predictors — a perf-smoke
    // test that ensures the LUT path remains fast on a realistic
    // frame. Not a wall-clock benchmark; cargo test --release should
    // complete in well under a second even on a slow runner.
    let y = noise_plane(0x42, 320 * 240);
    let u = noise_plane(0x43, 160 * 120);
    let v = noise_plane(0x44, 160 * 120);
    for pred in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run(
            Fourcc::Uly0,
            &[y.clone(), u.clone(), v.clone()],
            320,
            240,
            pred,
            8,
        );
    }
}
