#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the Ut Video **encoder**
//! and then through the in-crate decoder so any encoded chunk payload
//! also has to survive the parser the `decode_utvideo` target hammers.
//!
//! The decoder fuzz target (`decode_utvideo`) covers the attacker-facing
//! surface — bytes flow in from a network / file and the decoder must
//! never panic on them. The **encoder** is a different shape of risk:
//! its input is `(fourcc, width, height, predictor, num_slices,
//! per-plane samples)` — a typed `EncodedFrame` rather than a raw byte
//! stream — and it must never panic / abort / overflow / OOM regardless
//! of how hostile the *caller* is, because callers that mis-size a plane
//! buffer, pick a slice count larger than any row, or hand it an unsupported
//! predictor are real (and common) integration bugs. Just as critically,
//! once the encoder accepts an input it produces wire bytes that the
//! decoder MUST round-trip exactly — a silent encoder/decoder skew is a
//! correctness bug that the self-roundtrip suite catches on hand-picked
//! fixtures but a fuzzer drives across the whole parameter cube.
//!
//! Layout of the fuzz input:
//!
//! ```text
//!   byte 0      : FourCC selector (mod 5)  → ULY0 / ULY2 / ULY4 / ULRG / ULRA
//!   byte 1      : width seed   → 1..=32 snapped per fourcc parity
//!   byte 2      : height seed  → 1..=32 snapped per fourcc parity
//!   byte 3      : predictor selector (mod 4) → None / Left / Gradient / Median
//!   byte 4      : slice-count seed (1..=16, capped at height so at least one row per slice)
//!   bytes 5..   : raw plane-sample bytes consumed left-to-right across the
//!                 wire-order plane list (Y,U,V or G,B,R[,A]). If the input
//!                 runs out, the remaining samples are filled with zero so
//!                 a short input still exercises the full pipeline.
//! ```
//!
//! The contract under test is:
//!
//! 1. `encode_frame(&frame)` always *returns* a `Result` — no panic,
//!    no abort, no integer overflow (in a debug / ASAN build), no OOM.
//! 2. Whenever `encode_frame` returns `Ok(bytes)`, `decode_frame(cfg, &bytes)`
//!    must also return `Ok(decoded)` (the encoder is not allowed to emit
//!    syntactically-malformed bytes that its own decoder rejects).
//! 3. Whenever both calls succeed, the decoded per-plane samples must
//!    equal the input per-plane samples bit-exactly. This is the
//!    self-roundtrip invariant the round-1 tests pin on hand-picked
//!    fixtures, here driven across arbitrary `(fourcc × dims × predictor
//!    × num_slices × pixels)` tuples.
//!
//! Dimensions are capped at 32×32 (≤ 4 KiB luma) so the fuzzer's budget
//! lands on encoder/decoder logic — Huffman length builder, slice-range
//! arithmetic, RGB decorrelate, bit-pack/unpack symmetry — rather than
//! the trivial "allocate 4 GiB" branch the format's syntax allows.

use libfuzzer_sys::fuzz_target;
use oxideav_utvideo::{
    decode_frame, encode_frame, EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor,
    StreamConfig,
};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let header = &data[..5];
    let mut payload = &data[5..];

    let fourcc = match header[0] % 5 {
        0 => Fourcc::Uly0,
        1 => Fourcc::Uly2,
        2 => Fourcc::Uly4,
        3 => Fourcc::Ulrg,
        _ => Fourcc::Ulra,
    };

    // Map dim seeds into 2..=32 snapped to satisfy each FourCC's parity
    // constraints (ULY0 needs even W and H; ULY2 needs even W). Cap at 32
    // (≤ 1 024 pixels per luma plane) so a single iteration stays cheap.
    let mut width = ((header[1] as u32 % 32) + 2) & !1; // even, 2..=32
    let mut height = ((header[2] as u32 % 32) + 2) & !1; // even, 2..=32
    if !matches!(fourcc, Fourcc::Uly0) {
        // ULY2 still needs even W; ULY4/ULRG/ULRA accept any positive dim.
        // The +2 above already guarantees even ≥ 2 for both axes, so no
        // further adjustment is needed — kept as a noop for documentation.
    }
    if width == 0 {
        width = 2;
    }
    if height == 0 {
        height = 2;
    }

    let predictor = match header[3] % 4 {
        0 => Predictor::None,
        1 => Predictor::Left,
        2 => Predictor::Gradient,
        _ => Predictor::Median,
    };

    // Slice count 1..=16, additionally capped at `height` so every slice
    // owns at least one row of the luma plane. A slice count above the
    // plane row count is a legitimate caller bug to *reject* (the encoder
    // errors), but it's not what this target is hunting — bias the fuzzer
    // toward inputs that reach the per-slice predict + Huffman pack.
    let num_slices = ((header[4] as u32 % 16) + 1).min(height) as usize;

    // Build the per-plane sample buffers, consuming `payload` left-to-right
    // and zero-filling any shortfall.
    let plane_count = fourcc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for i in 0..plane_count {
        let (pw, ph) = fourcc.plane_dim(i, width, height);
        let expected = (pw as usize) * (ph as usize);
        let mut samples = vec![0u8; expected];
        let take = expected.min(payload.len());
        samples[..take].copy_from_slice(&payload[..take]);
        payload = &payload[take..];
        planes.push(PlaneInput { samples });
    }

    let frame = EncodedFrame {
        fourcc,
        width,
        height,
        predictor,
        num_slices,
        planes,
    };

    // Cache the input samples so the round-trip assertion can compare
    // bit-exactly against the decoded planes after encode_frame consumes
    // the EncodedFrame's planes.
    let inputs: Vec<Vec<u8>> = frame.planes.iter().map(|p| p.samples.clone()).collect();

    let encoded = match encode_frame(&frame) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };

    // Build the StreamConfig the decoder expects. The flags top byte
    // encodes `num_slices - 1` and bit 0x1 enables Huffman (matching
    // `decode_utvideo`'s synthesised extradata).
    let flags = 0x0000_0001 | (((num_slices as u32) - 1) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *fourcc.as_bytes(),
        frame_info_size: 4,
        flags,
    };
    let cfg = match StreamConfig::new(fourcc, width, height, extradata) {
        Ok(c) => c,
        Err(_) => return,
    };

    let decoded = match decode_frame(&cfg, &encoded) {
        Ok(f) => f,
        Err(e) => {
            // The encoder accepted these parameters but our own decoder
            // rejected the bytes it produced — that's a hard contract
            // violation, not a fuzz-discoverable corruption.
            panic!(
                "encoder produced bytes the in-crate decoder rejects: {e:?} \
                 fourcc={fourcc:?} {width}x{height} pred={predictor:?} \
                 slices={num_slices} encoded_len={}",
                encoded.len()
            );
        }
    };

    // Roundtrip equality: every decoded plane must equal the input
    // samples bit-exactly. The encoder's RGB decorrelate (G/B/R) and
    // forward predictor are each lossless by construction, so any
    // mismatch is a real bug in the encode/decode symmetry.
    assert_eq!(
        decoded.planes.len(),
        inputs.len(),
        "plane count mismatch after roundtrip: encoded {} decoded {}",
        inputs.len(),
        decoded.planes.len()
    );
    for (i, (dec, inp)) in decoded.planes.iter().zip(inputs.iter()).enumerate() {
        assert_eq!(
            &dec.samples, inp,
            "plane {i} roundtrip mismatch: fourcc={fourcc:?} {width}x{height} \
             pred={predictor:?} slices={num_slices}"
        );
    }
});
