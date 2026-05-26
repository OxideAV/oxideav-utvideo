//! Criterion benchmarks for the Ut Video encoder hot paths.
//!
//! Mirrors the `decode` bench coverage:
//!   - **encode_ulrg_1080p_single**: full-frame ULRG encode at 1920×1080
//!     with a single slice (serial). Dominated by RGB forward
//!     decorrelation + Gradient forward-predict + per-plane
//!     package-merge length build + slice bit-pack.
//!   - **encode_uly2_1080p_single**: same at YUV 4:2:2 (no RGB
//!     decorrelation pass).
//!   - **encode_parallel_scaling**: `bench_with_input` over slice
//!     counts `N ∈ {1, 2, 4, 8}` at 1280×720 ULY4 with the Gradient
//!     predictor; the Amdahl-bounded ceiling (per-plane Huffman length
//!     build remains single-threaded) is visible in the curve.
//!
//! Run with:
//!     cargo bench -p oxideav-utvideo --bench encode

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::encoder::{encode_frame, encode_frame_parallel, encode_frame_serial};
use oxideav_utvideo::encoder::{EncodedFrame, PlaneInput};
use oxideav_utvideo::fourcc::{Fourcc, Predictor};

fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

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

/// Build a fresh `EncodedFrame` (the encoder consumes by reference but
/// per-iteration plane construction adds a tiny `Vec::clone` cost; we
/// build once and clone into each iteration to keep the inner loop
/// hot on the encode path itself).
fn make_frame(fc: Fourcc, w: u32, h: u32, num_slices: usize, pred: Predictor) -> EncodedFrame {
    let plane_count = fc.plane_count();
    let planes: Vec<PlaneInput> = (0..plane_count)
        .map(|p| {
            let (pw, ph) = fc.plane_dim(p, w, h);
            PlaneInput {
                samples: build_plane(pw as usize, ph as usize, p),
            }
        })
        .collect();
    EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices,
        planes,
    }
}

fn bench_encode_ulrg_1080p_single(c: &mut Criterion) {
    let frame = make_frame(Fourcc::Ulrg, 1920, 1080, 1, Predictor::Gradient);
    let mut g = c.benchmark_group("encode_ulrg_1080p_single");
    g.throughput(Throughput::Bytes((1920 * 1080 * 3) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("ULRG/gradient/1920x1080/1slice"),
        |b| {
            b.iter(|| encode_frame(criterion::black_box(&frame)).expect("encode"));
        },
    );
    g.finish();
}

fn bench_encode_uly2_1080p_single(c: &mut Criterion) {
    let frame = make_frame(Fourcc::Uly2, 1920, 1080, 1, Predictor::Gradient);
    let mut g = c.benchmark_group("encode_uly2_1080p_single");
    g.throughput(Throughput::Bytes((1920 * 1080 * 2) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("ULY2/gradient/1920x1080/1slice"),
        |b| {
            b.iter(|| encode_frame(criterion::black_box(&frame)).expect("encode"));
        },
    );
    g.finish();
}

fn bench_encode_parallel_scaling(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_parallel_scaling");
    g.throughput(Throughput::Bytes((1280 * 720 * 3) as u64));
    for &num_slices in &[1usize, 2, 4, 8] {
        let frame = make_frame(Fourcc::Uly4, 1280, 720, num_slices, Predictor::Gradient);
        g.bench_with_input(
            BenchmarkId::new("serial", num_slices),
            &frame,
            |b, frame| {
                b.iter(|| encode_frame_serial(criterion::black_box(frame)).expect("encode"));
            },
        );
        g.bench_with_input(
            BenchmarkId::new("parallel", num_slices),
            &frame,
            |b, frame| {
                b.iter(|| encode_frame_parallel(criterion::black_box(frame)).expect("encode"));
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_encode_ulrg_1080p_single,
    bench_encode_uly2_1080p_single,
    bench_encode_parallel_scaling,
);
criterion_main!(benches);
