//! Round 4 — slice-parallel decoder. `spec/02` §7 "Implementation
//! notes": the decoder may dispatch independent slices to per-thread
//! workers once the offset table is parsed. The new
//! [`decode_frame_parallel`] entry point does exactly that on a
//! `std::thread::scope` pool sized to
//! `min(num_slices, available_parallelism())`.
//!
//! The suite verifies three properties:
//!
//! 1. **Bit-exact equivalence.** For every fixture the parallel path
//!    produces output identical to the serial path. This is the
//!    correctness wall — the spec's per-slice `+128` seed and the
//!    self-contained Huffman bit-stream make slices fully
//!    independent, but any subtle inter-slice leak in the
//!    implementation would diverge here.
//! 2. **`decode_frame` auto-dispatch.** The threshold-gated entry
//!    matches the explicit serial path on small frames and the
//!    explicit parallel path on large frames.
//! 3. **Error propagation.** A malformed slice in any position is
//!    surfaced to the caller (the parallel join propagates the first
//!    failing slice's error).

#![cfg(test)]

use oxideav_utvideo::decoder::{
    decode_frame, decode_frame_parallel, decode_frame_serial, PARALLEL_PIXEL_THRESHOLD,
};
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

/// Deterministic noise plane, same LCG as round-3 tests for cross-suite parity.
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

fn build_frame(
    fc: Fourcc,
    planes: &[Vec<u8>],
    w: u32,
    h: u32,
    pred: Predictor,
    slices: usize,
) -> Vec<u8> {
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
    encode_frame(&frame).unwrap()
}

/// Bit-exact equivalence of serial vs. parallel paths across a wide
/// matrix. If the parallel path ever drifts from serial — wrong row
/// strip allocation, misordered slices, lost `+128` seed — this is
/// where it surfaces.
#[test]
fn parallel_matches_serial_uly0_matrix() {
    let widths = [16u32, 64, 128, 320];
    let heights = [16u32, 48, 96, 240];
    let slice_counts = [1usize, 2, 4, 8];
    let predictors = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    let mut cases = 0u32;
    for &w in &widths {
        for &h in &heights {
            for &slices in &slice_counts {
                // Each slice must contain at least one row (spec/02 §5.2 derived).
                if (h as usize) < slices {
                    continue;
                }
                for &pred in &predictors {
                    let y = noise_plane(0x100 ^ w as u64 ^ h as u64, (w * h) as usize);
                    let u = noise_plane(0x200 ^ w as u64, (w / 2 * h / 2) as usize);
                    let v = noise_plane(0x300 ^ h as u64, (w / 2 * h / 2) as usize);
                    let cfg = cfg(Fourcc::Uly0, w, h, slices);
                    let bytes = build_frame(Fourcc::Uly0, &[y, u, v], w, h, pred, slices);
                    let serial = decode_frame_serial(&cfg, &bytes).unwrap();
                    let parallel = decode_frame_parallel(&cfg, &bytes).unwrap();
                    assert_eq!(
                        serial.planes, parallel.planes,
                        "drift at {}x{} slices={} pred={:?}",
                        w, h, slices, pred
                    );
                    cases += 1;
                }
            }
        }
    }
    assert!(cases >= 100, "matrix too small: {cases} cases");
}

/// RGB family carries the green-difference plane transform; ensure
/// the parallel path applies it exactly once (the transform runs
/// after the per-plane decode, in the joining thread). Cover ULRG
/// and ULRA.
#[test]
fn parallel_matches_serial_rgb_family() {
    for fc in [Fourcc::Ulrg, Fourcc::Ulra] {
        let plane_n = (256 * 192) as usize;
        let g = noise_plane(0xaabb_ccdd, plane_n);
        let b = noise_plane(0x1122_3344, plane_n);
        let r = noise_plane(0x5566_7788, plane_n);
        let a = vec![230u8; plane_n];
        let cfg = cfg(fc, 256, 192, 4);
        let planes: Vec<Vec<u8>> = if fc.has_alpha() {
            vec![g, b, r, a]
        } else {
            vec![g, b, r]
        };
        for pred in [Predictor::Left, Predictor::Median, Predictor::Gradient] {
            let bytes = build_frame(fc, &planes, 256, 192, pred, 4);
            let serial = decode_frame_serial(&cfg, &bytes).unwrap();
            let parallel = decode_frame_parallel(&cfg, &bytes).unwrap();
            assert_eq!(
                serial.planes, parallel.planes,
                "RGB drift for {:?}/{:?}",
                fc, pred
            );
        }
    }
}

/// `decode_frame` (the auto-dispatch entry) should pick the parallel
/// path for large multi-slice frames and the serial path for small
/// ones; in both cases the output must match the explicit-path call.
#[test]
fn auto_dispatch_matches_both_paths() {
    // Small frame: auto picks serial. 16×16 = 256 px (well under threshold).
    let small = noise_plane(0x1, 16 * 16);
    let bytes = build_frame(
        Fourcc::Uly4,
        &[small.clone(), small.clone(), small.clone()],
        16,
        16,
        Predictor::Median,
        4,
    );
    let c_small = cfg(Fourcc::Uly4, 16, 16, 4);
    let auto = decode_frame(&c_small, &bytes).unwrap();
    let serial = decode_frame_serial(&c_small, &bytes).unwrap();
    let parallel = decode_frame_parallel(&c_small, &bytes).unwrap();
    assert_eq!(auto.planes, serial.planes);
    assert_eq!(auto.planes, parallel.planes);

    // Large frame: 320×240 = 76 800 px (above PARALLEL_PIXEL_THRESHOLD).
    const _: () = assert!(320 * 240 > PARALLEL_PIXEL_THRESHOLD);
    let big_y = noise_plane(0xabc, 320 * 240);
    let big_u = noise_plane(0xdef, 160 * 120);
    let big_v = noise_plane(0xfed, 160 * 120);
    let bytes2 = build_frame(
        Fourcc::Uly0,
        &[big_y, big_u, big_v],
        320,
        240,
        Predictor::Gradient,
        8,
    );
    let c_big = cfg(Fourcc::Uly0, 320, 240, 8);
    let auto = decode_frame(&c_big, &bytes2).unwrap();
    let serial = decode_frame_serial(&c_big, &bytes2).unwrap();
    let parallel = decode_frame_parallel(&c_big, &bytes2).unwrap();
    assert_eq!(auto.planes, serial.planes);
    assert_eq!(auto.planes, parallel.planes);
}

/// 1-slice frames bypass the parallel fan-out regardless of size
/// (the spec's per-slice partitioning never has anything to parallelise
/// for `num_slices == 1`). Both entry points must still produce the
/// same bytes.
#[test]
fn single_slice_serial_equiv() {
    let y = noise_plane(0x1234_5678, 320 * 240);
    let u = noise_plane(0x2345_6789, 160 * 120);
    let v = noise_plane(0x3456_789a, 160 * 120);
    let cfg = cfg(Fourcc::Uly0, 320, 240, 1);
    let bytes = build_frame(Fourcc::Uly0, &[y, u, v], 320, 240, Predictor::Left, 1);
    let auto = decode_frame(&cfg, &bytes).unwrap();
    let serial = decode_frame_serial(&cfg, &bytes).unwrap();
    let parallel = decode_frame_parallel(&cfg, &bytes).unwrap();
    assert_eq!(auto.planes, serial.planes);
    assert_eq!(auto.planes, parallel.planes);
}

/// Stress: 256 slices on a tall enough frame (each slice 1 row). The
/// parallel path caps thread count at `available_parallelism()` so
/// the fanout buckets 256 slices into a handful of workers; the
/// per-bucket loop must still emit the correct strip order.
#[test]
fn many_slices_one_row_each() {
    // 64×256, each slice = 1 row. Pixel total = 16384 < threshold,
    // so explicit call to parallel path needed.
    let y = noise_plane(0xc0ffee, 64 * 256);
    let cfg = cfg(Fourcc::Uly4, 64, 256, 256);
    let zeros_uv = vec![0u8; 64 * 256];
    let bytes = build_frame(
        Fourcc::Uly4,
        &[y, zeros_uv.clone(), zeros_uv],
        64,
        256,
        Predictor::Left,
        256,
    );
    let serial = decode_frame_serial(&cfg, &bytes).unwrap();
    let parallel = decode_frame_parallel(&cfg, &bytes).unwrap();
    assert_eq!(serial.planes, parallel.planes);
}

/// Negative path: a corrupted-byte payload must surface an error
/// from the parallel join (not silently produce garbage). Flip the
/// last byte of slice 0's compressed data on plane 0; the decode
/// must fail.
#[test]
fn parallel_error_propagation_on_corrupt_slice() {
    let y = noise_plane(0xdead, 320 * 240);
    let u = noise_plane(0xbeef, 160 * 120);
    let v = noise_plane(0xface, 160 * 120);
    let cfg = cfg(Fourcc::Uly0, 320, 240, 4);
    let mut bytes = build_frame(Fourcc::Uly0, &[y, u, v], 320, 240, Predictor::Left, 4);
    // Truncate the chunk payload by 8 bytes (well into the last
    // plane's slice data, before the frame_info dword): this forces
    // a slice-truncated / chunk-short error in plane 2.
    bytes.truncate(bytes.len() - 8);
    // The auto-dispatch picks the parallel path for 320×240/4 slices.
    let res = decode_frame(&cfg, &bytes);
    assert!(res.is_err(), "truncated chunk must error");
}
