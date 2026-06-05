//! Criterion microbenches for the per-slice spatial-predictor
//! primitives.
//!
//! The decoder's per-slice hot path is
//! [`predict::apply_slice`] — one of four mode branches (None / Left /
//! Gradient / Median) over a single slice's row strip with the
//! universal `+128` first-pixel seed (`spec/04` §§3, 4, 7, 5). The
//! encoder's mirror is [`predict::forward_slice`]. Both run once per
//! `(plane, slice)` on every frame, so even a small per-byte cost
//! shows up linearly in full-frame throughput. The full-frame
//! `decode` / `encode` benches (`benches/decode.rs`, `benches/encode.rs`)
//! observe these costs only as part of a much larger pipeline
//! (Huffman + RGB decorrelate + plane fan-out + allocator); this
//! bench isolates them.
//!
//! ## What's measured
//!
//! - **`predict_inverse_slice`**: `apply_slice(pred, strip, w, rows,
//!   residuals)` for each of `Predictor ∈ {None, Left, Gradient,
//!   Median}` at `(w, rows) ∈ {(64, 64), (256, 256), (1920, 1080)}`.
//!   `bench_with_input` reports per-byte throughput so the four
//!   predictors are directly comparable on the same axis.
//!
//! - **`predict_forward_slice`**: `forward_slice(pred, plane, w,
//!   r_start=0, r_end=rows)` over the same size + predictor matrix,
//!   measuring the encoder-side residual production cost.
//!
//! - **`predict_choose_predictor`**: `choose_predictor(plane, w, h)`
//!   over the same sizes. The encoder calls this once per plane to
//!   pick the predictor for the upcoming frame (`spec/04` §1 +
//!   round-18 content-adaptive heuristic). It samples the first
//!   `HEURISTIC_SAMPLE_ROWS` rows and runs an inner-loop
//!   forward-predict + zero-run histogram; the per-plane cost is
//!   bounded and constant in `plane_height` but linear in `width`.
//!
//! ## Input distribution
//!
//! Two plane shapes drive the cost surface:
//!
//! - **"natural-ish"** — smooth gradient base + 4-bit xorshift32
//!   high-frequency noise (matches the `decode` / `encode` benches'
//!   `build_plane`). Hits the realistic mid-entropy regime where
//!   Gradient and Median diverge from None / Left.
//!
//! - **"flat"** — constant 0x80 byte. Drives every inverse-predictor
//!   into its degenerate cumulative path (Left + Gradient + Median
//!   all collapse to the row-0 +128 seed propagating across the slice)
//!   and pins the lower bound of the per-byte cost. Useful as a sanity
//!   check that the more expensive interior median doesn't add
//!   overhead on degenerate content.
//!
//! Each shape × size × predictor combination is a separate
//! `BenchmarkId`, so criterion's summary table reads cleanly.
//!
//! ## What this bench is **not**
//!
//! - Not the RGB inter-plane decorrelation (covered by
//!   `benches/rgb_decorrelate.rs`).
//! - Not the Huffman decode kernel (covered by `benches/huffman_lut.rs`).
//! - Not the full pipeline (covered by `benches/decode.rs` +
//!   `benches/encode.rs`).
//!
//! Run with:
//!
//! ```text
//!   cargo bench -p oxideav-utvideo --bench predict_slice
//! ```

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::fourcc::Predictor;
use oxideav_utvideo::predict::{apply_slice, choose_predictor, forward, forward_slice};

/// Cheap deterministic xorshift32. Matches the helper in the existing
/// decode / encode benches so plane content is comparable across the
/// three bench files.
fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

/// Natural-ish plane: row+col gradient base + 4-bit xorshift32 noise.
/// Forward-predictor residual histogram is concentrated near zero but
/// non-degenerate — realistic mid-entropy regime.
fn build_plane_natural(width: usize, height: usize, seed: u32) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let mut state = seed;
    for r in 0..height {
        for c in 0..width {
            let base = ((r as u32).wrapping_add(c as u32) >> 1) & 0xff;
            let noise = xorshift_byte(&mut state) as u32 & 0x0f;
            out[r * width + c] = (base.wrapping_add(noise) & 0xff) as u8;
        }
    }
    out
}

/// Flat plane (every byte = 0x80). Inverse predictors collapse to the
/// row-0 seed propagating; forward predictors emit an all-zero residual
/// after the +128 seed. Pins the degenerate lower-bound cost.
fn build_plane_flat(width: usize, height: usize) -> Vec<u8> {
    vec![0x80u8; width * height]
}

/// Synthesise the `(strip, residuals)` pair for an inverse-predictor
/// bench iteration. The residual stream is the bit-exact output of
/// `forward_slice(pred, plane, w, 0, rows)`; feeding it back through
/// `apply_slice` reconstructs `plane` exactly, so the bench measures
/// the same input distribution the real decoder sees per slice.
fn inverse_inputs(pred: Predictor, plane: &[u8], width: usize, rows: usize) -> (Vec<u8>, Vec<u8>) {
    // `forward` returns one Vec<u8> per slice; with `num_slices = 1`
    // the whole plane is one slice and `out[0]` is the residual stream
    // matching `apply_slice` on a `rows`-row strip.
    let residuals = forward(pred, plane, width, rows, 1)
        .into_iter()
        .next()
        .unwrap();
    let strip = vec![0u8; width * rows];
    (strip, residuals)
}

/// Inverse-predictor bench across the 4 predictors × 3 sizes × 2
/// shapes. Each `BenchmarkId` reports per-byte throughput.
fn bench_inverse_slice(c: &mut Criterion) {
    let sizes: &[(usize, usize)] = &[(64, 64), (256, 256), (1920, 1080)];
    let predictors = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    let mut g = c.benchmark_group("predict_inverse_slice");

    for (shape_label, plane_fn) in [
        (
            "natural",
            build_plane_natural as fn(usize, usize, u32) -> Vec<u8>,
        ),
        (
            "flat",
            (|w: usize, h: usize, _seed: u32| build_plane_flat(w, h))
                as fn(usize, usize, u32) -> Vec<u8>,
        ),
    ] {
        for &(w, rows) in sizes {
            let plane = plane_fn(w, rows, 0xdead_beef);
            for &pred in &predictors {
                let (strip_init, residuals) = inverse_inputs(pred, &plane, w, rows);
                g.throughput(Throughput::Bytes((w * rows) as u64));
                let id_label = format!("{shape_label}/{pred:?}/{w}x{rows}");
                g.bench_with_input(
                    BenchmarkId::from_parameter(id_label),
                    &(strip_init, residuals, w, rows, pred),
                    |b, (strip_init, residuals, w, rows, pred)| {
                        b.iter_batched(
                            || strip_init.clone(),
                            |mut strip| {
                                apply_slice(
                                    *pred,
                                    criterion::black_box(&mut strip),
                                    *w,
                                    *rows,
                                    criterion::black_box(residuals),
                                );
                            },
                            criterion::BatchSize::LargeInput,
                        );
                    },
                );
            }
        }
    }
    g.finish();
}

/// Forward-predictor bench across the same 4 predictors × 3 sizes × 2
/// shapes.
fn bench_forward_slice(c: &mut Criterion) {
    let sizes: &[(usize, usize)] = &[(64, 64), (256, 256), (1920, 1080)];
    let predictors = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    let mut g = c.benchmark_group("predict_forward_slice");

    for (shape_label, plane_fn) in [
        (
            "natural",
            build_plane_natural as fn(usize, usize, u32) -> Vec<u8>,
        ),
        (
            "flat",
            (|w: usize, h: usize, _seed: u32| build_plane_flat(w, h))
                as fn(usize, usize, u32) -> Vec<u8>,
        ),
    ] {
        for &(w, rows) in sizes {
            let plane = plane_fn(w, rows, 0xfeed_face);
            for &pred in &predictors {
                g.throughput(Throughput::Bytes((w * rows) as u64));
                let id_label = format!("{shape_label}/{pred:?}/{w}x{rows}");
                g.bench_with_input(
                    BenchmarkId::from_parameter(id_label),
                    &(plane.clone(), w, rows, pred),
                    |b, (plane, w, rows, pred)| {
                        b.iter(|| {
                            let r = forward_slice(*pred, criterion::black_box(plane), *w, 0, *rows);
                            criterion::black_box(r)
                        });
                    },
                );
            }
        }
    }
    g.finish();
}

/// Heuristic-selection bench. The encoder calls `choose_predictor`
/// once per plane; cost is bounded by `HEURISTIC_SAMPLE_ROWS * width *
/// 4 predictors`, which is constant in `plane_height` and linear in
/// `width`. The bench covers the same sizes as the per-slice benches
/// so the per-plane heuristic overhead can be compared apples-to-apples
/// against the per-plane forward-predict cost.
fn bench_choose_predictor(c: &mut Criterion) {
    let sizes: &[(usize, usize)] = &[(64, 64), (256, 256), (1920, 1080)];
    let mut g = c.benchmark_group("predict_choose_predictor");

    for (shape_label, plane_fn) in [
        (
            "natural",
            build_plane_natural as fn(usize, usize, u32) -> Vec<u8>,
        ),
        (
            "flat",
            (|w: usize, h: usize, _seed: u32| build_plane_flat(w, h))
                as fn(usize, usize, u32) -> Vec<u8>,
        ),
    ] {
        for &(w, h) in sizes {
            let plane = plane_fn(w, h, 0xcafe_f00d);
            g.throughput(Throughput::Bytes((w * h) as u64));
            let id_label = format!("{shape_label}/{w}x{h}");
            g.bench_with_input(
                BenchmarkId::from_parameter(id_label),
                &(plane, w, h),
                |b, (plane, w, h)| {
                    b.iter(|| choose_predictor(criterion::black_box(plane), *w, *h));
                },
            );
        }
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_inverse_slice,
    bench_forward_slice,
    bench_choose_predictor,
);
criterion_main!(benches);
