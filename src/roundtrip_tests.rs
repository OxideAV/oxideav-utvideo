//! End-to-end self-roundtrip suite — encode every FOURCC × every
//! predictor with a deterministic synthetic image and verify the
//! in-crate decoder reconstructs every plane byte-for-byte.
//!
//! The encoder is the in-crate test-only synthesiser (`encoder.rs`)
//! mirroring the wire format the decoder consumes; the goal is to
//! pin **decoder correctness against its own encoder** across every
//! supported FOURCC × predictor × slice-count combination. FFmpeg
//! byte-equality is not in scope (round 1 deliberately defers
//! that to a later round once a fixture corpus lands in `tables/`).

#![cfg(test)]

use crate::decoder::decode_frame;
use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
use crate::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

fn deterministic_plane(seed: u64, n: usize) -> Vec<u8> {
    // Cheap LCG; deterministic across platforms.
    let mut state = seed
        .wrapping_mul(2862933555777941757)
        .wrapping_add(3037000493);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 56) as u8);
    }
    out
}

fn build_planes(fc: Fourcc, w: u32, h: u32) -> Vec<PlaneInput> {
    (0..fc.plane_count())
        .map(|i| {
            let (pw, ph) = fc.plane_dim(i, w, h);
            let n = (pw as usize) * (ph as usize);
            PlaneInput {
                samples: deterministic_plane(0x5555 + i as u64, n),
            }
        })
        .collect()
}

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let flags = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *b"YV12",
        frame_info_size: 4,
        flags,
    };
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

fn run_roundtrip(fc: Fourcc, w: u32, h: u32, predictor: Predictor, slices: usize) {
    let planes = build_planes(fc, w, h);
    let cfg = cfg_for(fc, w, h, slices);
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor,
        num_slices: slices,
        planes: planes.clone(),
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    assert_eq!(decoded.fourcc, fc);
    assert_eq!(decoded.predictor, predictor);
    assert_eq!(decoded.planes.len(), fc.plane_count());
    for (i, want) in planes.iter().enumerate() {
        if decoded.planes[i].samples != want.samples {
            panic!(
                "plane {i} mismatch on FourCC={:?} predictor={:?} slices={slices} ({}×{})",
                fc, predictor, w, h
            );
        }
    }
}

#[test]
fn roundtrip_uly0_all_predictors_single_slice() {
    for p in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run_roundtrip(Fourcc::Uly0, 16, 16, p, 1);
    }
}

#[test]
fn roundtrip_uly2_all_predictors_single_slice() {
    for p in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run_roundtrip(Fourcc::Uly2, 16, 16, p, 1);
    }
}

#[test]
fn roundtrip_uly4_all_predictors_single_slice() {
    for p in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run_roundtrip(Fourcc::Uly4, 16, 16, p, 1);
    }
}

#[test]
fn roundtrip_ulrg_all_predictors_single_slice() {
    for p in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run_roundtrip(Fourcc::Ulrg, 16, 16, p, 1);
    }
}

#[test]
fn roundtrip_ulra_all_predictors_single_slice() {
    for p in [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ] {
        run_roundtrip(Fourcc::Ulra, 16, 16, p, 1);
    }
}

#[test]
fn roundtrip_uly0_multi_slice_all_predictors() {
    // 16x16 -> per-FOURCC slice counts that the wire formula carries
    // exactly: 1, 2, 4, 8.
    for slices in [1usize, 2, 4, 8] {
        for p in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            run_roundtrip(Fourcc::Uly0, 16, 16, p, slices);
        }
    }
}

#[test]
fn roundtrip_uly2_multi_slice_all_predictors() {
    // ULY2 only requires even width; height can be anything.
    for slices in [1usize, 2, 3, 7] {
        for p in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            run_roundtrip(Fourcc::Uly2, 16, 17, p, slices);
        }
    }
}

#[test]
fn roundtrip_ulrg_multi_slice_all_predictors() {
    for slices in [1usize, 2, 4, 8] {
        for p in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            run_roundtrip(Fourcc::Ulrg, 16, 16, p, slices);
        }
    }
}

#[test]
fn roundtrip_ulra_multi_slice_all_predictors() {
    for slices in [1usize, 2, 4, 8] {
        for p in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            run_roundtrip(Fourcc::Ulra, 16, 16, p, slices);
        }
    }
}

#[test]
fn roundtrip_solid_colour_compresses_to_single_symbol() {
    // Solid-red ULRG with -pred left should yield single-symbol-zero
    // B and R difference planes (constant == 0 after decorrelation).
    let cfg = cfg_for(Fourcc::Ulrg, 16, 16, 1);
    let g = vec![0u8; 16 * 16];
    let b = vec![0u8; 16 * 16];
    let r = vec![253u8; 16 * 16];
    let frame = EncodedFrame {
        fourcc: Fourcc::Ulrg,
        width: 16,
        height: 16,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput { samples: g.clone() },
            PlaneInput { samples: b.clone() },
            PlaneInput { samples: r.clone() },
        ],
    };
    let bytes = encode_frame(&frame).unwrap();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    assert_eq!(decoded.planes[0].samples, g);
    assert_eq!(decoded.planes[1].samples, b);
    assert_eq!(decoded.planes[2].samples, r);
}

#[test]
fn roundtrip_high_entropy_uly4() {
    // High-entropy noise — exercises full Huffman path with deeper trees.
    run_roundtrip(Fourcc::Uly4, 64, 48, Predictor::Median, 4);
    run_roundtrip(Fourcc::Uly4, 64, 48, Predictor::Gradient, 4);
}

#[test]
fn roundtrip_non_square_dimensions() {
    run_roundtrip(Fourcc::Uly2, 32, 50, Predictor::Left, 7);
    run_roundtrip(Fourcc::Ulra, 33, 17, Predictor::Median, 1);
    run_roundtrip(Fourcc::Uly4, 13, 11, Predictor::Gradient, 1);
}
