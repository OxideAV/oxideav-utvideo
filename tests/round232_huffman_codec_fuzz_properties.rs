//! Round 232 — stable-CI mirror of the `huffman_codec` cargo-fuzz
//! target.
//!
//! `cargo-fuzz` requires a nightly toolchain (libFuzzer's sanitizer-
//! coverage flags are `-Z`-gated), so the regular CI matrix never
//! builds the `fuzz/` binary crate. This stable-Rust test gives the
//! same logical coverage on a deterministic seed corpus, so a
//! regression in the Huffman builder, the `BitWriter` / `BitReader`
//! symmetry, the LUT vs. slow-path agreement, or the single-symbol
//! idempotency surfaces in the regular `cargo test` lane rather than
//! waiting for the daily fuzz run.
//!
//! The three properties under test are documented at the top of
//! `fuzz/fuzz_targets/huffman_codec.rs`:
//!
//! 1. **Panic-free build.** `HuffmanTable::build` returns a `Result`
//!    on any 256-byte descriptor — no panic / abort / overflow / OOM.
//!
//! 2. **Panic-free `decode_slice`.** When `build` succeeds, calling
//!    `decode_slice(slice_data, n_pixels)` returns a `Result` on any
//!    `(slice_data, n_pixels)` pair. Malformed input surfaces as
//!    `Error::SliceTruncated` or `Error::HuffmanDecodeFailure` —
//!    never a panic.
//!
//! 3. **Roundtrip on a synthesised valid codebook.** A
//!    uniform-length-`k` descriptor over exactly `2^k` active symbols
//!    is Kraft-valid by construction; encoding a fuzz-derived symbol
//!    sequence through `BitWriter::write_code(code_for(sym))` and
//!    decoding via `decode_slice` recovers the input bit-exactly.
//!
//! Plus two deterministic-only properties the libFuzzer target can't
//! easily enumerate:
//!
//! 4. **Single-symbol descriptor every byte.** For every
//!    `sym ∈ 0..=255`, build a single-symbol descriptor (sentinel
//!    `0` at `sym`, `255` everywhere else) and assert
//!    `decode_slice(&[], n_pixels)` returns `n_pixels` copies of
//!    `sym` for `n_pixels ∈ {0, 1, 17, 4096}`. Single-symbol is a
//!    public spec corner case (`spec/05` §6.1) and the only
//!    invariant the fuzz harness asserts at random.
//!
//! 5. **Multiple-zero-sentinel rejection.** Any descriptor with two
//!    or more `0` entries is structurally malformed (the single-
//!    symbol case allows EXACTLY one `0`); `build` must surface
//!    `Error::MultipleSingleSymbolSentinels` and never crash.

use oxideav_utvideo::{
    huffman::{BitWriter, HuffmanTable},
    Error,
};

/// Cheap xorshift32 — deterministic, no `rand` dep.
fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

/// Re-implementation of the fuzz target's per-input loop, so an input
/// can be driven through the same three properties from a stable
/// `cargo test` lane.
fn drive_one_input(data: &[u8]) {
    if data.len() < 258 {
        return;
    }
    let header = &data[..2];
    let descriptor_bytes = &data[2..258];
    let slice_tail = &data[258..];

    let mut descriptor = [0u8; 256];
    descriptor.copy_from_slice(descriptor_bytes);

    // Property 1: panic-free build.
    let raw_build = HuffmanTable::build(&descriptor);

    // Property 2: panic-free decode_slice.
    if let Ok(table) = &raw_build {
        let max_pixels = 4096usize;
        let max_slice_bytes = 8192usize;
        let n_pixels = ((header[0] as usize) | ((header[1] as usize) << 8)) % (max_pixels + 1);
        let slice_len = slice_tail.len().min(max_slice_bytes) & !3;
        let slice_data = &slice_tail[..slice_len];
        let _ = table.decode_slice(slice_data, n_pixels);

        // Single-symbol idempotency.
        if let Some(sym) = table.single_symbol {
            let decoded = table
                .decode_slice(slice_data, n_pixels)
                .expect("single-symbol decode is infallible");
            assert_eq!(decoded.len(), n_pixels);
            for &b in &decoded {
                assert_eq!(b, sym);
            }
        }
    }

    // Property 3: synthesised-valid-codebook roundtrip.
    let k = ((header[0] >> 4) % 8) + 1;
    let active = 1usize << k;
    let mut rt_descriptor = [255u8; 256];
    for sym in &mut rt_descriptor[..active] {
        *sym = k;
    }
    let table = HuffmanTable::build(&rt_descriptor)
        .expect("uniform-length 2^k alphabet is Kraft-valid by construction");

    let want = slice_tail.len().min(4096);
    let mut symbols = Vec::with_capacity(want);
    for &b in &slice_tail[..want] {
        symbols.push((b as usize % active) as u8);
    }

    let mut bw = BitWriter::new();
    for &s in &symbols {
        let (code, len) = table.code_for(s).expect("symbol must be in codebook");
        bw.write_code(code, len);
    }
    let packed = bw.finish();
    let decoded = table
        .decode_slice(&packed, symbols.len())
        .expect("decoder must accept encoder bytes");
    assert_eq!(decoded, symbols);
}

#[test]
fn property_panic_free_on_empty_input() {
    drive_one_input(&[]);
    drive_one_input(&[0u8; 1]);
    drive_one_input(&[0u8; 16]);
    drive_one_input(&[0u8; 257]);
}

#[test]
fn property_panic_free_on_all_zero_descriptor() {
    // 256 zero-byte descriptor → 256 single-symbol sentinels →
    // `MultipleSingleSymbolSentinels` rejected cleanly.
    let mut buf = vec![0u8; 258];
    buf.extend_from_slice(&[0u8; 64]);
    drive_one_input(&buf);
}

#[test]
fn property_panic_free_on_all_sentinel_descriptor() {
    // 256 × 255 → empty codebook (zero active symbols, no zero
    // sentinel). `build` returns an empty `by_length` table; the
    // roundtrip path uses a synthesised descriptor so it is not
    // affected.
    let mut buf = vec![0u8, 0u8];
    buf.extend_from_slice(&[255u8; 256]);
    buf.extend_from_slice(&[0u8; 64]);
    drive_one_input(&buf);
}

#[test]
fn property_panic_free_xorshift_sweep() {
    // 64 deterministic inputs of length 258 + 64 = 322 bytes each.
    // Each iteration drives Property 1 + Property 2 (with a random
    // pixel-count + slice-data tail) and Property 3 (the synthesised
    // roundtrip). Catches an off-by-one in build / decode_slice that
    // the static corpus above might miss.
    for seed in 0..64u32 {
        let mut state = seed.wrapping_mul(0x9e37_79b9) ^ 0xdead_beef;
        let mut buf = Vec::with_capacity(322);
        for _ in 0..322 {
            buf.push(xorshift_byte(&mut state));
        }
        drive_one_input(&buf);
    }
}

#[test]
fn property_panic_free_truncated_slice_tail() {
    // Build a valid uniform-length-3 descriptor (8 active symbols,
    // length 3) and run `decode_slice` against slice tails of every
    // length 0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 32, 64, 128. Some
    // are not 4-byte aligned — `decode_slice` itself does not enforce
    // the alignment (the per-frame walk does), so it must handle the
    // misaligned case as `SliceTruncated` rather than panic.
    let mut descriptor = [255u8; 256];
    for sym in &mut descriptor[..8] {
        *sym = 3;
    }
    let table = HuffmanTable::build(&descriptor).unwrap();
    for &slice_len in &[0usize, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 32, 64, 128] {
        let slice = vec![0xAAu8; slice_len];
        for &n in &[0usize, 1, 8, 64, 4096] {
            let _ = table.decode_slice(&slice, n);
        }
    }
}

#[test]
fn property_single_symbol_idempotency_all_bytes() {
    // For every possible single-symbol byte `sym ∈ 0..=255`, build
    // the descriptor + assert decode_slice(&[], n) == vec![sym; n]
    // for several `n` including 0. The byte-walk normally enforces
    // an empty `slice_data` for single-symbol planes
    // (`spec/05` §6.1) — verify the table's own short-circuit returns
    // the correct buffer regardless of what the caller hands it.
    for sym in 0u8..=255 {
        let mut descriptor = [255u8; 256];
        descriptor[sym as usize] = 0;
        let table = HuffmanTable::build(&descriptor).unwrap();
        assert_eq!(table.single_symbol, Some(sym));
        for &n in &[0usize, 1, 17, 4096] {
            let decoded = table.decode_slice(&[], n).unwrap();
            assert_eq!(decoded.len(), n);
            for &b in &decoded {
                assert_eq!(b, sym);
            }
            // Non-empty `slice_data` ignored on the single-symbol
            // path — the table returns `n` copies of `sym` regardless.
            let decoded2 = table.decode_slice(&[1, 2, 3, 4], n).unwrap();
            assert_eq!(decoded2, decoded);
        }
    }
}

#[test]
fn property_multiple_zero_sentinel_rejected() {
    // Two `0` entries in the descriptor → both look like a
    // single-symbol sentinel; build must reject with
    // `MultipleSingleSymbolSentinels`. Vary which pair of symbols
    // is zero to exercise the rejection across the alphabet.
    for &(a, b) in &[
        (0u8, 1u8),
        (0, 255),
        (1, 128),
        (42, 137),
        (100, 200),
        (254, 255),
    ] {
        let mut descriptor = [255u8; 256];
        descriptor[a as usize] = 0;
        descriptor[b as usize] = 0;
        let r = HuffmanTable::build(&descriptor);
        assert!(
            matches!(r, Err(Error::MultipleSingleSymbolSentinels)),
            "expected MultipleSingleSymbolSentinels, got {r:?} for (a={a}, b={b})"
        );
    }
}

#[test]
fn property_uniform_length_roundtrip_every_k() {
    // For every `k ∈ 1..=8`, build the uniform-length-k descriptor
    // (Kraft-valid by construction), encode an xorshift-derived
    // symbol stream of length 1024 mod 2^k, and assert
    // `BitWriter` → `decode_slice` recovers the input exactly. This
    // pins the encode/decode symmetry at every code length the LUT
    // can fast-path resolve (k ≤ LUT_BITS = 12) on a stable lane.
    for k in 1u8..=8 {
        let active = 1usize << k;
        let mut descriptor = [255u8; 256];
        for sym in &mut descriptor[..active] {
            *sym = k;
        }
        let table = HuffmanTable::build(&descriptor).unwrap();

        let mut state = (k as u32).wrapping_mul(0x9e37_79b9) ^ 0xdead_beef;
        let n_symbols = 1024usize;
        let mut symbols = Vec::with_capacity(n_symbols);
        for _ in 0..n_symbols {
            let b = xorshift_byte(&mut state);
            symbols.push((b as usize % active) as u8);
        }
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (code, len) = table.code_for(s).unwrap();
            bw.write_code(code, len);
        }
        let packed = bw.finish();
        // The packed stream must be a multiple of 4 bytes
        // (`BitWriter::finish` pads to a 32-bit boundary).
        assert_eq!(packed.len() % 4, 0);
        let decoded = table.decode_slice(&packed, symbols.len()).unwrap();
        assert_eq!(decoded, symbols, "uniform-length-{k} roundtrip mismatch");
    }
}

#[test]
fn property_skewed_two_symbol_roundtrip() {
    // 2-symbol descriptor: symbols 0 and 255 both at length 1.
    // Encode all 256 byte values mod 2 (so the symbol stream is a
    // 0/255 sequence reflecting low-bit parity of each input byte),
    // assert recovery.
    let mut descriptor = [255u8; 256];
    descriptor[0] = 1;
    descriptor[255] = 1;
    let table = HuffmanTable::build(&descriptor).unwrap();

    let mut symbols = Vec::with_capacity(256);
    for b in 0u8..=255 {
        symbols.push(if b & 1 == 0 { 0u8 } else { 255 });
    }
    let mut bw = BitWriter::new();
    for &s in &symbols {
        let (code, len) = table.code_for(s).unwrap();
        bw.write_code(code, len);
    }
    let packed = bw.finish();
    let decoded = table.decode_slice(&packed, symbols.len()).unwrap();
    assert_eq!(decoded, symbols);
}
