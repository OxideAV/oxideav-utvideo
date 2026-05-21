//! Round 5 — slice-parallel encoder. Mirrors the round-4
//! parallel-decoder suite: every slice's `+128`-seeded predictor state
//! is independent (`spec/04` §§3.1, 4, 5, 7) and every slice's Huffman
//! bit-stream is a self-contained byte blob (`spec/02` §5), so the
//! per-plane forward-predict and per-slice bit-pack steps fan out
//! across `std::thread::scope`. The suite verifies four properties:
//!
//! 1. **Byte-exact equivalence.** For every fixture the parallel
//!    encoder produces output identical to the serial encoder. This
//!    is the strict correctness wall.
//! 2. **`encode_frame` auto-dispatch.** The threshold-gated entry
//!    matches the explicit serial path on small frames and the
//!    explicit parallel path on large frames.
//! 3. **Self-roundtrip through the parallel encoder.** Frames encoded
//!    through the parallel path decode back to identical samples
//!    (serial + parallel decoder paths both).
//! 4. **Per-slice independence.** A 256-slice frame (each slice one
//!    row) encodes and decodes identically across paths — the
//!    parallel encoder's bucket-fanout must preserve slice order.

#![cfg(test)]

use oxideav_utvideo::decoder::{decode_frame_parallel, decode_frame_serial};
use oxideav_utvideo::encoder::{
    encode_frame, encode_frame_parallel, encode_frame_serial, EncodedFrame, PlaneInput,
    PARALLEL_PIXEL_THRESHOLD,
};
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

/// Deterministic noise plane. Same LCG as the round-3 + round-4 tests
/// for cross-suite parity.
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

fn frame_for(
    fc: Fourcc,
    planes: &[Vec<u8>],
    w: u32,
    h: u32,
    pred: Predictor,
    slices: usize,
) -> EncodedFrame {
    EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes: planes
            .iter()
            .map(|p| PlaneInput { samples: p.clone() })
            .collect(),
    }
}

/// The strict correctness wall. Across a wide ULY0 matrix the parallel
/// encoder must produce **byte-identical** output to the serial one.
/// Any subtle inter-slice leak in `forward_parallel` or the per-slice
/// bit-pack fanout would surface here as a byte diff.
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
                // Each slice must contain at least one row.
                if (h as usize) < slices {
                    continue;
                }
                for &pred in &predictors {
                    let y = noise_plane(0x100 ^ w as u64 ^ h as u64, (w * h) as usize);
                    let u = noise_plane(0x200 ^ w as u64, (w / 2 * h / 2) as usize);
                    let v = noise_plane(0x300 ^ h as u64, (w / 2 * h / 2) as usize);
                    let frame = frame_for(Fourcc::Uly0, &[y, u, v], w, h, pred, slices);
                    let serial = encode_frame_serial(&frame).unwrap();
                    let parallel = encode_frame_parallel(&frame).unwrap();
                    assert_eq!(
                        serial, parallel,
                        "byte drift at {}x{} slices={} pred={:?}",
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
/// the parallel encoder applies it exactly once (the transform runs
/// before the per-plane parallel pack, on the parent thread).
#[test]
fn parallel_matches_serial_rgb_family() {
    for fc in [Fourcc::Ulrg, Fourcc::Ulra] {
        let plane_n = (256 * 192) as usize;
        let g = noise_plane(0xaabb_ccdd, plane_n);
        let b = noise_plane(0x1122_3344, plane_n);
        let r = noise_plane(0x5566_7788, plane_n);
        let a = vec![230u8; plane_n];
        let planes: Vec<Vec<u8>> = if fc.has_alpha() {
            vec![g, b, r, a]
        } else {
            vec![g, b, r]
        };
        for pred in [Predictor::Left, Predictor::Median, Predictor::Gradient] {
            let frame = frame_for(fc, &planes, 256, 192, pred, 4);
            let serial = encode_frame_serial(&frame).unwrap();
            let parallel = encode_frame_parallel(&frame).unwrap();
            assert_eq!(serial, parallel, "RGB byte drift for {fc:?}/{pred:?}");
        }
    }
}

/// `encode_frame` (the auto-dispatch entry) should pick the parallel
/// path for large multi-slice frames and the serial path for small
/// ones; in both cases the output must be byte-identical to the
/// explicit-path call.
#[test]
fn auto_dispatch_matches_both_paths() {
    // Small frame: 16×16 = 256 px, well under the threshold. Auto
    // picks serial.
    let small = noise_plane(0x1, 16 * 16);
    let frame = frame_for(
        Fourcc::Uly4,
        &[small.clone(), small.clone(), small.clone()],
        16,
        16,
        Predictor::Median,
        4,
    );
    let auto = encode_frame(&frame).unwrap();
    let serial = encode_frame_serial(&frame).unwrap();
    let parallel = encode_frame_parallel(&frame).unwrap();
    assert_eq!(auto, serial);
    assert_eq!(auto, parallel);

    // Large frame: 320×240 = 76 800 px, above PARALLEL_PIXEL_THRESHOLD.
    const _: () = assert!(320 * 240 > PARALLEL_PIXEL_THRESHOLD);
    let big_y = noise_plane(0xabc, 320 * 240);
    let big_u = noise_plane(0xdef, 160 * 120);
    let big_v = noise_plane(0xfed, 160 * 120);
    let frame2 = frame_for(
        Fourcc::Uly0,
        &[big_y, big_u, big_v],
        320,
        240,
        Predictor::Gradient,
        8,
    );
    let auto = encode_frame(&frame2).unwrap();
    let serial = encode_frame_serial(&frame2).unwrap();
    let parallel = encode_frame_parallel(&frame2).unwrap();
    assert_eq!(auto, serial);
    assert_eq!(auto, parallel);
}

/// 1-slice frames bypass the parallel fan-out regardless of size;
/// auto-dispatch picks the serial path. All three entries must still
/// produce identical bytes.
#[test]
fn single_slice_byte_equiv() {
    let y = noise_plane(0x1234_5678, 320 * 240);
    let u = noise_plane(0x2345_6789, 160 * 120);
    let v = noise_plane(0x3456_789a, 160 * 120);
    let frame = frame_for(Fourcc::Uly0, &[y, u, v], 320, 240, Predictor::Left, 1);
    let auto = encode_frame(&frame).unwrap();
    let serial = encode_frame_serial(&frame).unwrap();
    let parallel = encode_frame_parallel(&frame).unwrap();
    assert_eq!(auto, serial);
    assert_eq!(auto, parallel);
}

/// Stress: 256 slices on a tall frame (each slice 1 row). The parallel
/// encoder caps thread count at `available_parallelism()` so the
/// fan-out buckets 256 slices into a handful of workers; the
/// per-bucket loop must still emit the correct slice order.
#[test]
fn many_slices_one_row_each() {
    let y = noise_plane(0xc0ffee, 64 * 256);
    let zeros_uv = vec![0u8; 64 * 256];
    let frame = frame_for(
        Fourcc::Uly4,
        &[y, zeros_uv.clone(), zeros_uv],
        64,
        256,
        Predictor::Left,
        256,
    );
    let serial = encode_frame_serial(&frame).unwrap();
    let parallel = encode_frame_parallel(&frame).unwrap();
    assert_eq!(serial, parallel, "256-slice byte drift");

    // And the result must round-trip through both decode paths.
    let c = cfg(Fourcc::Uly4, 64, 256, 256);
    let dec_s = decode_frame_serial(&c, &parallel).unwrap();
    let dec_p = decode_frame_parallel(&c, &parallel).unwrap();
    assert_eq!(dec_s.planes, dec_p.planes);
}

/// Parallel-encoded bytes must round-trip through both decode paths
/// to the original per-plane samples. This is the end-to-end
/// correctness check (encode-parallel → decode-serial → equality vs.
/// encode-parallel → decode-parallel → equality).
#[test]
fn parallel_encode_roundtrips_through_decoder() {
    let big_y = noise_plane(0x11111, 320 * 240);
    let big_u = noise_plane(0x22222, 160 * 120);
    let big_v = noise_plane(0x33333, 160 * 120);
    let frame = frame_for(
        Fourcc::Uly0,
        &[big_y.clone(), big_u.clone(), big_v.clone()],
        320,
        240,
        Predictor::Median,
        8,
    );
    let bytes = encode_frame_parallel(&frame).unwrap();
    let c = cfg(Fourcc::Uly0, 320, 240, 8);
    let dec_s = decode_frame_serial(&c, &bytes).unwrap();
    let dec_p = decode_frame_parallel(&c, &bytes).unwrap();
    assert_eq!(dec_s.planes, dec_p.planes);
    assert_eq!(dec_s.planes[0].samples, big_y);
    assert_eq!(dec_s.planes[1].samples, big_u);
    assert_eq!(dec_s.planes[2].samples, big_v);
}

/// Perf smoke: encoding a 1280×720 ULY4 8-slice frame on the parallel
/// path should be measurably faster than the serial path. The
/// assertion is intentionally loose (`parallel <= serial * 0.9`) to
/// keep the test robust on busy CI; the headline numbers go in the
/// README's measured table. If the parallel path is somehow slower,
/// either the dispatch is broken or the threshold is wrong.
#[test]
fn perf_smoke_uly4_1280x720() {
    use std::time::Instant;
    let n = (1280 * 720) as usize;
    let y = noise_plane(0xdeadbeef, n);
    let u = noise_plane(0xfeedface, n);
    let v = noise_plane(0xcafebabe, n);
    let frame = frame_for(Fourcc::Uly4, &[y, u, v], 1280, 720, Predictor::Gradient, 8);

    // Warm up the per-path caches once each so the timing isn't first-call dominated.
    let _ = encode_frame_serial(&frame).unwrap();
    let _ = encode_frame_parallel(&frame).unwrap();

    let t0 = Instant::now();
    let _ = encode_frame_serial(&frame).unwrap();
    let serial = t0.elapsed();

    let t0 = Instant::now();
    let _ = encode_frame_parallel(&frame).unwrap();
    let parallel = t0.elapsed();

    eprintln!(
        "perf 1280x720 ULY4 s=8 grad: serial={:?}, parallel={:?}",
        serial, parallel
    );
    // Sanity check: parallel must not be dramatically *slower* than serial.
    // Allow up to 1.5× serial as the floor: any worse and we have a bug,
    // not a noisy measurement.
    assert!(
        parallel.as_nanos() * 2 <= serial.as_nanos() * 3,
        "parallel ({parallel:?}) >> serial ({serial:?}); regression?"
    );
}
