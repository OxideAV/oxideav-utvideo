#![no_main]

//! Fuzz the per-plane Huffman codebook builder + slice bit-stream
//! decoder directly, below the per-frame byte walk the other targets
//! drive.
//!
//! Round 232: the existing three fuzz targets
//! (`decode_utvideo` / `encode_utvideo_frame` / `inspect_utvideo`)
//! reach [`huffman::HuffmanTable::build`] + [`huffman::HuffmanTable::
//! decode_slice`] only after the per-frame byte walk has accepted the
//! attacker's chunk shape (256-byte descriptor + slice-end-offset
//! table + payload-len identity at `spec/02` §§4, 5). On random bytes
//! the byte walk rejects long before the Huffman layer runs, so the
//! attacker surface inside the Huffman path (Kraft check, canonical
//! code assignment, LUT build, fast-path/slow-path decode, bit-reader
//! tail handling) gets a tiny share of fuzz-budget iterations.
//!
//! This target fuzzes the Huffman layer directly. The input is split
//! into a fixed-shape header + a 256-byte descriptor + a slice-data
//! tail, all fed without going through the frame walk:
//!
//! ```text
//!   byte 0       : n_pixels seed (mapped to 0..=4095)
//!   byte 1       : n_pixels seed continuation (high bits)
//!   bytes 2..258 : 256-byte Huffman code-length descriptor
//!   bytes 258..  : slice_data (truncated to a 4-byte multiple — the
//!                  caller validates word-alignment per `spec/05`
//!                  §4.1; this target fuzzes the decoder *inside*
//!                  that invariant, not the validator that enforces
//!                  it).
//! ```
//!
//! Three properties are pinned on every input:
//!
//! 1. **Panic-free build.** [`HuffmanTable::build`] always returns a
//!    `Result` on any 256-byte descriptor, regardless of how malformed
//!    (over-Kraft sums, every-symbol-length-255 sentinels, multiple
//!    zero-length entries). The construction is meant to surface
//!    structural defects as typed errors — `KraftViolation`,
//!    `MultipleSingleSymbolSentinels`, `Error::InvalidInput` — never
//!    as a panic / overflow / OOM.
//!
//! 2. **Panic-free decode_slice.** When `build` succeeds, calling
//!    [`HuffmanTable::decode_slice`] with any `(slice_data, n_pixels)`
//!    pair must always return a `Result`. The bit reader's word-aligned
//!    fast path, the LUT lookup, and the length-tier slow-path fallback
//!    are all in scope. `slice_data` length and `n_pixels` are not
//!    correlated by construction — the attacker shape is genuinely
//!    "arbitrary residual buffer of arbitrary requested length", which
//!    the decoder must reject cleanly (with `SliceTruncated` or
//!    `HuffmanDecodeFailure`) rather than panic.
//!
//! 3. **Roundtrip on a synthesised valid codebook.** A subset of fuzz
//!    inputs constructs a *deliberately Kraft-valid* descriptor from a
//!    fuzz-derived shape (uniform-length over an attacker-chosen number
//!    of active symbols, padded with the 255 sentinel), uses the
//!    resulting [`HuffmanTable::code_for`] forward map to bit-pack a
//!    fuzz-derived symbol sequence via [`BitWriter`], and asserts
//!    `decode_slice` on the packed bytes recovers the input bit-exactly.
//!    This pins the `BitWriter`/`BitReader` symmetry below the frame
//!    layer, where a 1-bit-offset overflow would otherwise show up only
//!    when the full encode/decode loop happens to mis-align by chance.
//!
//! The roundtrip is structurally gated on a Kraft-valid descriptor —
//! property 3 only fires when the descriptor we synthesise has its
//! Kraft sum exactly equal to 1.0 (the canonical Huffman invariant per
//! `spec/05` §2.2) and `build` accepts it. The synthesis path
//! guarantees that by construction (uniform length `k` over exactly
//! `2^k` active symbols), so property 3 fires on every iteration that
//! reaches it — not just on the rare random-descriptor-happens-to-be-
//! Kraft-valid case.

use libfuzzer_sys::fuzz_target;
use oxideav_utvideo::huffman::{BitWriter, HuffmanTable};

/// Maximum pixel count we ever request from `decode_slice`. Caps the
/// inner loop budget so an iteration that synthesises a 2-symbol
/// codebook can't ask for a billion-pixel decode and waste the
/// fuzzer's time on the loop body rather than on parser defects.
const MAX_PIXELS: usize = 4096;

/// Maximum slice_data byte length we ever hand `decode_slice`. Caps
/// the bit-reader tail so the fast path's word-aligned load loop
/// can't accept an attacker-sized residual buffer.
const MAX_SLICE_BYTES: usize = 8192;

fuzz_target!(|data: &[u8]| {
    if data.len() < 258 {
        return;
    }
    let header = &data[..2];
    let descriptor_bytes = &data[2..258];
    let slice_tail = &data[258..];

    let mut descriptor = [0u8; 256];
    descriptor.copy_from_slice(descriptor_bytes);

    // Property 1: panic-free build on any descriptor.
    let raw_build = HuffmanTable::build(&descriptor);

    // Property 2: panic-free decode_slice on any slice_data + n_pixels.
    // Cap the input to keep one iteration cheap; `decode_slice` must
    // handle the boundary case where the requested pixel count exceeds
    // the bit budget cleanly (it returns `SliceTruncated`).
    if let Ok(table) = &raw_build {
        let n_pixels = ((header[0] as usize) | ((header[1] as usize) << 8)) % (MAX_PIXELS + 1);
        let slice_len = slice_tail.len().min(MAX_SLICE_BYTES);
        // Trim to a 4-byte multiple per `spec/05` §4.1 — the slice
        // word-alignment is the caller's responsibility (the per-frame
        // byte walk enforces it). `decode_slice` itself does NOT
        // re-check; the panic-freedom property holds regardless.
        let slice_len = slice_len & !3;
        let slice_data = &slice_tail[..slice_len];
        let _ = table.decode_slice(slice_data, n_pixels);

        // Single-symbol idempotency: when build flags single-symbol,
        // decode_slice MUST return exactly `n_pixels` copies of the
        // sentinel symbol, regardless of slice_data.
        if let Some(sym) = table.single_symbol {
            let decoded = table
                .decode_slice(slice_data, n_pixels)
                .expect("single-symbol decode is infallible");
            assert_eq!(decoded.len(), n_pixels);
            for &b in &decoded {
                assert_eq!(b, sym, "single-symbol decode emitted off-table byte");
            }
        }
    }

    // Property 3: synthesised-valid-codebook bit-pack / bit-unpack
    // roundtrip. Bias one fuzz byte to choose a uniform-length descriptor
    // shape: pick a length `k` in `1..=8` and mark exactly `2^k` symbols
    // (those with the lowest indices) at length `k`, sentinel-pad the
    // rest. Kraft sum is `2^k * 2^(-k) = 1` exactly, so canonical Huffman
    // build always succeeds. Then bit-pack a fuzz-derived symbol stream
    // through `BitWriter::write_code(code_for(sym))` and assert
    // `decode_slice` recovers the input.
    //
    // The "fuzz-derived shape" lives in a different byte slot from the
    // raw descriptor above so the two properties don't shadow each
    // other on the same input; this target's fuzz budget covers both
    // paths per iteration.
    {
        let k = ((header[0] >> 4) % 8) + 1; // 1..=8 -> alphabet 2..=256
        let active = 1usize << k; // exactly 2^k active symbols
        let mut roundtrip_descriptor = [255u8; 256];
        for sym in &mut roundtrip_descriptor[..active] {
            *sym = k;
        }
        let table = HuffmanTable::build(&roundtrip_descriptor)
            .expect("uniform-length 2^k alphabet must be Kraft-valid by construction");

        // Build a symbol sequence: take fuzz bytes mod `active` so each
        // symbol is guaranteed in-codebook. Length is fuzz-driven, capped
        // by MAX_PIXELS so a single iteration stays cheap.
        let want = slice_tail.len().min(MAX_PIXELS);
        let mut symbols = Vec::with_capacity(want);
        for &b in &slice_tail[..want] {
            symbols.push((b as usize % active) as u8);
        }

        // Encode via BitWriter::write_code(code_for(sym)).
        let mut bw = BitWriter::new();
        for &s in &symbols {
            let (code, len) = table
                .code_for(s)
                .expect("every symbol in 0..2^k must have a code in a uniform-k table");
            bw.write_code(code, len);
        }
        let packed = bw.finish();

        // Decode and assert bit-exact.
        let decoded = table
            .decode_slice(&packed, symbols.len())
            .expect("decoder must accept bytes its own encoder produced");
        assert_eq!(
            decoded,
            symbols,
            "encode_then_decode roundtrip mismatch: k={k} active={active} \
             len={want} packed_bytes={}",
            packed.len()
        );
    }
});
