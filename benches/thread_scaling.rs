//! Criterion benchmark for worker-budget scaling (round 420).
//!
//! The `decode` / `encode` benches sweep *slice counts* with a budget
//! of one worker per slice; this bench holds the stream shape fixed —
//! 1280×720 ULY4, 8 slices, Gradient predictor, mid-entropy content —
//! and sweeps the **caller-granted worker budget** through
//! `W ∈ {1, 2, 4, max}` (`max` = host parallelism, derived HERE, on
//! the caller side — the codec itself never queries the host). This is
//! the measurement that keeps the slice-parallel win visible under the
//! threading contract: `W = 1` is the contract's serial default, and
//! each row shows what a bigger budget buys on the same bytes.
//!
//! Both `*_with_workers` surfaces are driven (threshold-gated dispatch,
//! which this frame size crosses), so the numbers reflect exactly what
//! a framework executor gets by granting the budget through
//! `set_execution_context`.
//!
//! Run with:
//!     cargo bench -p oxideav-utvideo --bench thread_scaling

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::decoder::decode_frame_with_workers;
use oxideav_utvideo::encoder::encode_frame_with_workers;
use oxideav_utvideo::{EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const NUM_SLICES: usize = 8;

fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

/// Smooth gradient base + high-frequency noise: mid-entropy residuals
/// after Gradient prediction, the realistic regime for a real frame.
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

fn make_frame() -> EncodedFrame {
    let fc = Fourcc::Uly4;
    let planes: Vec<PlaneInput> = (0..fc.plane_count())
        .map(|p| {
            let (pw, ph) = fc.plane_dim(p, WIDTH, HEIGHT);
            PlaneInput {
                samples: build_plane(pw as usize, ph as usize, p),
            }
        })
        .collect();
    EncodedFrame {
        fourcc: fc,
        width: WIDTH,
        height: HEIGHT,
        predictor: Predictor::Gradient,
        num_slices: NUM_SLICES,
        planes,
    }
}

/// Budget sweep: 1 / 2 / 4 / host-max (deduplicated and sorted so the
/// criterion rows stay monotonic when the host has <= 4 cores).
fn budgets() -> Vec<usize> {
    let host = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let mut v = vec![1usize, 2, 4, host];
    v.sort_unstable();
    v.dedup();
    v
}

fn bench_decode_thread_scaling(c: &mut Criterion) {
    let frame = make_frame();
    let bytes = encode_frame_with_workers(&frame, 1).expect("encode");
    let extradata =
        Extradata::canonical_extradata_for(Fourcc::Uly4, NUM_SLICES).expect("extradata");
    let cfg = StreamConfig::new(Fourcc::Uly4, WIDTH, HEIGHT, extradata).expect("config");

    let mut g = c.benchmark_group("decode_thread_scaling");
    g.throughput(Throughput::Bytes((WIDTH as u64) * (HEIGHT as u64) * 3));
    for workers in budgets() {
        g.bench_with_input(
            BenchmarkId::new("workers", workers),
            &workers,
            |b, &workers| {
                b.iter(|| {
                    decode_frame_with_workers(&cfg, criterion::black_box(&bytes), workers)
                        .expect("decode")
                });
            },
        );
    }
    g.finish();
}

fn bench_encode_thread_scaling(c: &mut Criterion) {
    let frame = make_frame();
    let mut g = c.benchmark_group("encode_thread_scaling");
    g.throughput(Throughput::Bytes((WIDTH as u64) * (HEIGHT as u64) * 3));
    for workers in budgets() {
        g.bench_with_input(
            BenchmarkId::new("workers", workers),
            &workers,
            |b, &workers| {
                b.iter(|| {
                    encode_frame_with_workers(criterion::black_box(&frame), workers)
                        .expect("encode")
                });
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_decode_thread_scaling,
    bench_encode_thread_scaling,
);
criterion_main!(benches);
