//! Criterion benchmarks for the Ut Video decoder hot paths.
//!
//! All inputs are synthesised on the fly (no committed binary fixtures)
//! using `encode_frame` against a deterministic xorshift32 image
//! pattern, so the benches stay self-contained and run without `docs/`.
//!
//! Coverage:
//!   - **decode_ulrg_1080p_single**: full-frame ULRG decode at 1920×1080
//!     with a single slice (serial path, no parallel fan-out). The
//!     dominant cost is the per-plane Huffman LUT walk + Gradient
//!     inverse predict + RGB inverse decorrelation.
//!   - **decode_uly2_1080p_single**: same at YUV 4:2:2. Smaller chroma
//!     planes vs. ULRG plus no RGB decorrelation pass.
//!   - **decode_parallel_scaling**: `bench_with_input` over slice counts
//!     `N ∈ {1, 2, 4, 8}` at 1280×720 ULY4 with the Gradient predictor;
//!     shows the slice-parallel speedup table in criterion output.
//!
//! Run with:
//!     cargo bench -p oxideav-utvideo --bench decode

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::decoder::{decode_frame, decode_frame_parallel, decode_frame_serial};
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

/// Cheap deterministic xorshift32 — fills plane data with non-zero,
/// non-trivially-compressible bytes so the Huffman decoder takes the
/// general path (multi-symbol LUT + occasional slow-path) rather than
/// the single-symbol short-circuit.
fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

/// Build a deterministic "natural-ish" plane: smooth gradient base +
/// xorshift32 high-frequency noise. After Gradient prediction the
/// residual histogram is concentrated near zero but non-degenerate —
/// the realistic mid-entropy regime for a real-world frame.
fn build_plane(width: usize, height: usize, plane: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let mut state: u32 = 0xdead_beef ^ (plane as u32).wrapping_mul(0x9e37_79b9);
    for r in 0..height {
        for c in 0..width {
            let base = ((r as u32).wrapping_add(c as u32) >> 1) & 0xff;
            let noise = xorshift_byte(&mut state) as u32 & 0x0f;
            out[r * width + c] = (base.wrapping_add(noise) & 0xff) as u8;
        }
    }
    out
}

/// Build a `StreamConfig` matching the FFmpeg 7.1.2 extradata layout
/// for `(fc, num_slices)` (`spec/01` §4.4.3 — flag bit 0 set, slice
/// count high byte = `num_slices - 1`).
fn stream_cfg(fc: Fourcc, w: u32, h: u32, num_slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, num_slices).expect("ffmpeg_for");
    StreamConfig::new(fc, w, h, extradata).expect("StreamConfig::new")
}

/// Synthesise + encode one frame ready for the decode bench. Returns
/// `(stream_config, encoded_bytes)` so the same config flows back into
/// `decode_frame`.
fn synth_encoded_frame(
    fc: Fourcc,
    w: u32,
    h: u32,
    num_slices: usize,
    pred: Predictor,
) -> (StreamConfig, Vec<u8>) {
    let cfg = stream_cfg(fc, w, h, num_slices);
    let plane_count = fc.plane_count();
    let planes: Vec<PlaneInput> = (0..plane_count)
        .map(|p| {
            let (pw, ph) = fc.plane_dim(p, w, h);
            PlaneInput {
                samples: build_plane(pw as usize, ph as usize, p),
            }
        })
        .collect();
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices,
        planes,
    };
    let bytes = encode_frame(&frame).expect("encode_frame");
    (cfg, bytes)
}

fn bench_decode_ulrg_1080p_single(c: &mut Criterion) {
    let (cfg, frame) = synth_encoded_frame(Fourcc::Ulrg, 1920, 1080, 1, Predictor::Gradient);
    let mut g = c.benchmark_group("decode_ulrg_1080p_single");
    // 3 planes × 1920×1080 samples on the wire.
    g.throughput(Throughput::Bytes((1920 * 1080 * 3) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("ULRG/gradient/1920x1080/1slice"),
        |b| {
            b.iter(|| decode_frame(&cfg, criterion::black_box(&frame)).expect("decode"));
        },
    );
    g.finish();
}

fn bench_decode_uly2_1080p_single(c: &mut Criterion) {
    let (cfg, frame) = synth_encoded_frame(Fourcc::Uly2, 1920, 1080, 1, Predictor::Gradient);
    let mut g = c.benchmark_group("decode_uly2_1080p_single");
    // ULY2 (YUV 4:2:2) total samples = 2 * W * H on the wire.
    g.throughput(Throughput::Bytes((1920 * 1080 * 2) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("ULY2/gradient/1920x1080/1slice"),
        |b| {
            b.iter(|| decode_frame(&cfg, criterion::black_box(&frame)).expect("decode"));
        },
    );
    g.finish();
}

fn bench_decode_parallel_scaling(c: &mut Criterion) {
    // 1280×720 ULY4 — large enough to cross PARALLEL_PIXEL_THRESHOLD
    // (64 Ki px) for `N > 1` so the auto-dispatch actually exercises
    // the parallel path. `bench_with_input` produces a row of slice
    // counts in criterion output so the scaling is one chart line.
    let mut g = c.benchmark_group("decode_parallel_scaling");
    g.throughput(Throughput::Bytes((1280 * 720 * 3) as u64));
    for &num_slices in &[1usize, 2, 4, 8] {
        let (cfg, frame) =
            synth_encoded_frame(Fourcc::Uly4, 1280, 720, num_slices, Predictor::Gradient);
        // Serial (always single-threaded) measurement.
        g.bench_with_input(
            BenchmarkId::new("serial", num_slices),
            &(cfg, frame.clone()),
            |b, (cfg, frame)| {
                b.iter(|| decode_frame_serial(cfg, criterion::black_box(frame)).expect("decode"));
            },
        );
        // Parallel-path measurement. For `N == 1` the parallel path
        // also runs single-threaded by construction (no fan-out
        // possible); the entry is kept so the criterion output has
        // a 1-slice baseline against the parallel dispatcher's
        // fixed overhead.
        g.bench_with_input(
            BenchmarkId::new("parallel", num_slices),
            &(cfg, frame),
            |b, (cfg, frame)| {
                b.iter(|| decode_frame_parallel(cfg, criterion::black_box(frame)).expect("decode"));
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_decode_ulrg_1080p_single,
    bench_decode_uly2_1080p_single,
    bench_decode_parallel_scaling,
);
criterion_main!(benches);
