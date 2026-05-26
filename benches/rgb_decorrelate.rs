//! Criterion microbench for the RGB inter-plane decorrelation
//! primitives `predict::forward_decorrelate_rgb` (encoder side) and
//! `predict::inverse_decorrelate_rgb` (decoder side).
//!
//! `spec/04` §6 defines the ULRG / ULRA transform:
//!
//! - Encode: `B' = (B - G + 128) mod 256`, `R' = (R - G + 128) mod 256`
//! - Decode: `B  = (B' + G - 128) mod 256`, `R  = (R' + G - 128) mod 256`
//!
//! G stays as-is in both directions. Both kernels are a tight per-pixel
//! 3-byte load + 2-byte wrapping arithmetic + 2-byte store. They run
//! once per full frame on the RGB-family FOURCCs (ULRG / ULRA), so they
//! add a measurable additive constant to the full-frame decode/encode
//! curve.
//!
//! `bench_with_input` over `n_samples ∈ {64K, 256K, 1M, 2_073_600}`
//! (the last = 1920×1080) shows linear scaling and pins the per-byte
//! kernel rate.
//!
//! Run with:
//!     cargo bench -p oxideav-utvideo --bench rgb_decorrelate

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::predict::{forward_decorrelate_rgb, inverse_decorrelate_rgb};

fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

fn build_byte_plane(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(xorshift_byte(&mut state));
    }
    out
}

fn bench_forward_decorrelate(c: &mut Criterion) {
    let mut g = c.benchmark_group("rgb_forward_decorrelate");
    for &n in &[65_536usize, 262_144, 1_048_576, 1920 * 1080] {
        let g_plane = build_byte_plane(n, 0xdead_beef);
        let b_init = build_byte_plane(n, 0xcafe_f00d);
        let r_init = build_byte_plane(n, 0xfeed_face);
        g.throughput(Throughput::Bytes(n as u64));
        g.bench_with_input(
            BenchmarkId::from_parameter(n),
            &(g_plane, b_init, r_init),
            |bencher, (g_plane, b_init, r_init)| {
                bencher.iter_batched(
                    || (b_init.clone(), r_init.clone()),
                    |(mut b, mut r)| {
                        forward_decorrelate_rgb(
                            criterion::black_box(g_plane),
                            criterion::black_box(&mut b),
                            criterion::black_box(&mut r),
                        );
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    g.finish();
}

fn bench_inverse_decorrelate(c: &mut Criterion) {
    let mut g = c.benchmark_group("rgb_inverse_decorrelate");
    for &n in &[65_536usize, 262_144, 1_048_576, 1920 * 1080] {
        let g_plane = build_byte_plane(n, 0xdead_beef);
        let b_init = build_byte_plane(n, 0xcafe_f00d);
        let r_init = build_byte_plane(n, 0xfeed_face);
        g.throughput(Throughput::Bytes(n as u64));
        g.bench_with_input(
            BenchmarkId::from_parameter(n),
            &(g_plane, b_init, r_init),
            |bencher, (g_plane, b_init, r_init)| {
                bencher.iter_batched(
                    || (b_init.clone(), r_init.clone()),
                    |(mut b, mut r)| {
                        inverse_decorrelate_rgb(
                            criterion::black_box(g_plane),
                            criterion::black_box(&mut b),
                            criterion::black_box(&mut r),
                        );
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_forward_decorrelate,
    bench_inverse_decorrelate
);
criterion_main!(benches);
