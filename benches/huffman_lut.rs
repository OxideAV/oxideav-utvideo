//! Criterion microbench for the canonical-Huffman LUT decode path.
//!
//! `HuffmanTable::decode_slice` is the per-pixel decode kernel. Round 3
//! added a 12-bit prefix LUT (`2^12 = 4096` entries) that resolves any
//! code of length `<= LUT_BITS` in one peek-bits + load + shift. This
//! bench isolates that kernel:
//!
//!   - Build a synthetic 256-entry code-length descriptor with a known
//!     max-codelen.
//!   - Pre-pack a slice bit-stream of `n_pixels` symbols using the same
//!     `BitWriter` the encoder uses on the production path.
//!   - Measure `HuffmanTable::decode_slice(bytes, n_pixels)`.
//!
//! Two regimes:
//!   - **lut_pure**: `max_len = 12` (== `LUT_BITS`), every code resolves
//!     on the LUT fast path. Best case for the LUT.
//!   - **lut_with_fallback**: `max_len = 14 > LUT_BITS`, so the two
//!     longest tiers fall through to the slow-path length-tier binary
//!     search. Realistic for high-entropy frames per `spec/02` §4.2's
//!     16-bit empirical max-codelen note.
//!
//! `bench_with_input` over `n_pixels ∈ {4096, 16384, 65536, 262144}`
//! shows decode-rate scaling and pins per-symbol decode cost.
//!
//! Run with:
//!     cargo bench -p oxideav-utvideo --bench huffman_lut

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_utvideo::huffman::{BitWriter, HuffmanTable};

/// Build a Kraft-complete code-length descriptor whose max code length
/// is exactly `max_len`. Layout:
///
/// - 1 symbol at each length `1..max_len-1` (taking `1 - 2^-(max_len-1)`
///   of the Kraft budget),
/// - 2 symbols at length `max_len` (closing Kraft equality:
///   `2 * 2^-max_len = 2^-(max_len-1)`).
///
/// Total symbols used = `max_len`. Remaining slots are sentinel (255).
fn build_descriptor(max_len: u8) -> [u8; 256] {
    assert!((2..=16).contains(&max_len));
    let mut d = [255u8; 256];
    // One symbol per length 1..(max_len-1)
    for (i, l) in (1..max_len).enumerate() {
        d[i] = l;
    }
    // Two symbols at the longest length.
    let last_idx = (max_len - 1) as usize;
    d[last_idx] = max_len;
    d[last_idx + 1] = max_len;
    d
}

/// Pre-pack a slice bit-stream of `n_pixels` symbols drawn from a
/// deterministic cycle over the descriptor's used symbols. The cycle
/// guarantees every code-length tier (including the slow-path tiers
/// in `lut_with_fallback`) is hit roughly proportional to its weight,
/// matching realistic decode workload mix.
fn pack_stream(table: &HuffmanTable, n_pixels: usize, used_syms: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    for i in 0..n_pixels {
        let s = used_syms[i % used_syms.len()];
        let (c, l) = table.code_for(s).expect("descriptor symbol");
        bw.write_code(c, l);
    }
    bw.finish()
}

fn build_table_and_stream(max_len: u8, n_pixels: usize) -> (HuffmanTable, Vec<u8>) {
    let d = build_descriptor(max_len);
    let table = HuffmanTable::build(&d).expect("HuffmanTable::build");
    // Used symbols are positions 0..(max_len-1) (one per length) + the
    // doubled position (max_len-1) which is also used. Round to the
    // sym positions actually populated.
    let used_syms: Vec<u8> = (0..max_len).collect();
    let stream = pack_stream(&table, n_pixels, &used_syms);
    (table, stream)
}

fn bench_lut_pure(c: &mut Criterion) {
    // max_len = 12 → every code on the LUT fast path.
    let mut g = c.benchmark_group("huffman_lut_pure_max12");
    for &n_pixels in &[4096usize, 16384, 65536, 262144] {
        let (table, stream) = build_table_and_stream(12, n_pixels);
        g.throughput(Throughput::Elements(n_pixels as u64));
        g.bench_with_input(
            BenchmarkId::from_parameter(n_pixels),
            &(table, stream, n_pixels),
            |b, (table, stream, n_pixels)| {
                b.iter(|| {
                    table
                        .decode_slice(criterion::black_box(stream), *n_pixels)
                        .expect("decode_slice")
                });
            },
        );
    }
    g.finish();
}

fn bench_lut_with_fallback(c: &mut Criterion) {
    // max_len = 14 → top two tiers (lengths 13 and 14) fall back to
    // the slow-path length-tier search.
    let mut g = c.benchmark_group("huffman_lut_fallback_max14");
    for &n_pixels in &[4096usize, 16384, 65536, 262144] {
        let (table, stream) = build_table_and_stream(14, n_pixels);
        g.throughput(Throughput::Elements(n_pixels as u64));
        g.bench_with_input(
            BenchmarkId::from_parameter(n_pixels),
            &(table, stream, n_pixels),
            |b, (table, stream, n_pixels)| {
                b.iter(|| {
                    table
                        .decode_slice(criterion::black_box(stream), *n_pixels)
                        .expect("decode_slice")
                });
            },
        );
    }
    g.finish();
}

criterion_group!(benches, bench_lut_pure, bench_lut_with_fallback);
criterion_main!(benches);
