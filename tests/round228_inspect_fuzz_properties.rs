//! Round 228 — stable-CI mirror of the `inspect_utvideo` cargo-fuzz
//! target.
//!
//! `cargo-fuzz` requires a nightly toolchain (libFuzzer's sanitizer-
//! coverage flags are `-Z`-gated), so the regular CI matrix never
//! builds the `fuzz/` binary crate. This stable-Rust test gives the
//! same logical coverage on a deterministic seed corpus: every input
//! drives the same three properties the libFuzzer target asserts on
//! attacker bytes, so a regressed inspector / decoder skew / off-by-
//! one in the byte walk surfaces in the regular cargo-test lane
//! instead of waiting for the daily fuzz run.
//!
//! The three properties under test are documented at the top of
//! `fuzz/fuzz_targets/inspect_utvideo.rs`:
//!
//! 1. **Panic-free inspector** — `peek_frame_info` and `peek_frame`
//!    always return a `Result` on any bytes; this test asserts the
//!    same by simply calling them without `catch_unwind` (any panic
//!    fails the test).
//!
//! 2. **Containment** — when `peek_frame` succeeds, every reported
//!    byte offset (`descriptor_start`, `end_offsets_start`,
//!    `slice_data_start`, every per-slice `start`/`end`) lies inside
//!    `[0, payload.len())` and respects the documented ordering
//!    invariants.
//!
//! 3. **Inspector/decoder agreement** — when `decode_frame` succeeds
//!    on the same `(cfg, payload)`, `peek_frame` also succeeds AND
//!    `(peek.predictor, peek.frame_info) == (decoded.predictor,
//!    decoded.frame_info)`.
//!
//! Plus a fourth deterministic-only property the libFuzzer target
//! can't easily assert (it can't enumerate descriptors):
//!
//! 4. **Roundtrip seed corpus** — every `(FOURCC, predictor,
//!    num_slices, dims)` cell in a small enumeration is round-tripped
//!    `encode_frame -> peek_frame -> decode_frame` and the inspector
//!    output is checked field-by-field against the decoder output.
//!    This catches the "inspector and decoder disagree on a
//!    well-formed frame" regression directly.

use oxideav_utvideo::{
    decode_frame, encode_frame, peek_frame, peek_frame_info, EncodedFrame, Error, Extradata,
    Fourcc, PlaneInput, Predictor, StreamConfig,
};

/// Cheap xorshift32 — deterministic, no `rand` dep.
fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

fn build_plane(width: u32, height: u32, plane: usize, seed: u32) -> Vec<u8> {
    let n = (width as usize) * (height as usize);
    let mut state = seed ^ (plane as u32).wrapping_mul(0x9e37_79b9);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(xorshift_byte(&mut state));
    }
    out
}

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

fn encode_frame_for(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
    let plane_count = fc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for i in 0..plane_count {
        let (pw, ph) = fc.plane_dim(i, w, h);
        planes.push(PlaneInput {
            samples: build_plane(pw, ph, i, 0xc0de_d00d),
        });
    }
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

/// The same fuzz-target header decode used in
/// `fuzz/fuzz_targets/inspect_utvideo.rs`. Returns `None` if the input
/// is too short or the synthesised geometry is invalid.
fn synth_cfg_from_header(data: &[u8]) -> Option<(StreamConfig, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    let (header, payload) = data.split_at(4);
    let fourcc = match header[0] % 5 {
        0 => Fourcc::Uly0,
        1 => Fourcc::Uly2,
        2 => Fourcc::Uly4,
        3 => Fourcc::Ulrg,
        _ => Fourcc::Ulra,
    };
    let width = ((header[1] as u32 % 64) + 2) & !1;
    let height = ((header[2] as u32 % 64) + 2) & !1;
    let num_slices = (header[3] as u32 % 16) + 1;
    let flags = 0x0000_0001 | ((num_slices - 1) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *fourcc.as_bytes(),
        frame_info_size: 4,
        flags,
    };
    let cfg = StreamConfig::new(fourcc, width, height, extradata).ok()?;
    Some((cfg, payload))
}

/// The three universal properties for one fuzz-shaped input.
fn assert_fuzz_invariants(cfg: &StreamConfig, payload: &[u8]) {
    let peek_info = peek_frame_info(payload);
    let peek = peek_frame(cfg, payload);

    // Property 2: containment on every reported offset.
    if let Ok(layout) = &peek {
        let n = payload.len();
        for plane in &layout.planes {
            assert!(
                plane.descriptor_start <= n,
                "descriptor_start={} > payload.len()={}",
                plane.descriptor_start,
                n
            );
            assert!(plane.end_offsets_start <= n);
            assert!(plane.slice_data_start <= n);
            assert!(plane.descriptor_start <= plane.end_offsets_start);
            assert!(plane.end_offsets_start <= plane.slice_data_start);
            for slice in &plane.slices {
                assert!(slice.start <= n, "slice.start={} > {}", slice.start, n);
                assert!(slice.end <= n, "slice.end={} > {}", slice.end, n);
                assert!(slice.start <= slice.end);
                assert!(slice.start >= plane.slice_data_start);
            }
        }
        let (peek_info_dword, peek_info_pred) = peek_info.expect(
            "peek_frame_info must succeed whenever peek_frame succeeds (both gate on payload.len() >= 4)",
        );
        assert_eq!(peek_info_dword, layout.frame_info);
        assert_eq!(peek_info_pred, layout.predictor);
    }

    // Property 3: inspector/decoder agreement on success.
    if let Ok(decoded) = decode_frame(cfg, payload) {
        let layout = peek
            .as_ref()
            .expect("decode_frame succeeded; peek_frame must succeed on the same bytes");
        assert_eq!(layout.frame_info, decoded.frame_info);
        assert_eq!(layout.predictor, decoded.predictor);
    }
}

// ---------------------------------------------------------------------
// Property 1 + 2 + 3: synthetic fuzz-shaped seed corpus.
// ---------------------------------------------------------------------

#[test]
fn empty_input_returns_early_without_panic() {
    // Mirrors the `if data.len() < 4 { return; }` short-circuit in
    // the fuzz target. We additionally assert peek_frame_info reports
    // the documented `MissingFrameInfo` error.
    for short in &[&[][..], &[1u8][..], &[1, 2, 3][..]] {
        let r = peek_frame_info(short);
        assert!(matches!(r, Err(Error::MissingFrameInfo)));
    }
}

#[test]
fn all_zero_chunk_payload_fuzz_shape() {
    // Header (0, 0, 0, 0) → Uly0, w=2, h=2, num_slices=1. Payload
    // is all zeros — descriptor is all-zero, slice-end table is
    // all-zero. The decoder rejects (every code length 0 → Kraft
    // violation); the inspector must surface that or a containment-
    // valid layout without panicking.
    let mut data = vec![0u8; 4];
    data.resize(4 + 4096, 0u8); // large enough to fit a 2×2 Uly0 chunk
    let (cfg, payload) = synth_cfg_from_header(&data).unwrap();
    assert_fuzz_invariants(&cfg, payload);
}

#[test]
fn truncated_to_four_bytes_only_frame_info() {
    // Synthetic header → cfg; payload is exactly 4 bytes (the
    // trailing frame_info dword). peek_frame should report
    // ChunkTooShort on the descriptor; peek_frame_info should
    // succeed and read those 4 bytes as the frame_info.
    let header = [1u8, 1, 1, 0];
    let payload_bytes = [0x42u8, 0x00, 0x00, 0x00];
    let mut data = Vec::new();
    data.extend_from_slice(&header);
    data.extend_from_slice(&payload_bytes);
    let (cfg, payload) = synth_cfg_from_header(&data).unwrap();
    assert_fuzz_invariants(&cfg, payload);
    let (fi, _pred) = peek_frame_info(payload).unwrap();
    assert_eq!(fi, 0x42);
    let r = peek_frame(&cfg, payload);
    assert!(matches!(r, Err(Error::ChunkTooShort { .. })));
}

#[test]
fn fuzz_corpus_xorshift_panic_freedom() {
    // A swept corpus of 64 deterministic inputs covering every
    // FourCC × slice-count band × payload-length regime. Each input
    // is a `(header, xorshift-payload)` pair sized so the inspector
    // sees a mix of ChunkTooShort / NonMonotonic / well-formed
    // layouts (depending on whether xorshift happens to land
    // monotonic offsets, which it almost never does — the value of
    // this corpus is panic-freedom under bad-shape inputs, not
    // success).
    let mut state: u32 = 0xfee1_dead;
    for i in 0..64u32 {
        let header = [
            (i & 0xff) as u8,                      // FourCC selector
            (i.wrapping_mul(7) & 0xff) as u8,      // width seed
            (i.wrapping_mul(11) & 0xff) as u8,     // height seed
            (i.wrapping_mul(13) & 0xff) as u8 & 7, // slice-count seed in 0..8
        ];
        let payload_len = ((i * 257 + 13) % 4096) as usize;
        let mut payload = Vec::with_capacity(payload_len);
        for _ in 0..payload_len {
            payload.push(xorshift_byte(&mut state));
        }
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&header);
        data.extend_from_slice(&payload);
        if let Some((cfg, p)) = synth_cfg_from_header(&data) {
            assert_fuzz_invariants(&cfg, p);
        }
    }
}

// ---------------------------------------------------------------------
// Property 4: deterministic roundtrip — every well-formed
// `(FOURCC, predictor, num_slices)` cell agrees between inspector
// and decoder.
// ---------------------------------------------------------------------

#[test]
fn roundtrip_inspector_decoder_agreement_every_cell() {
    let fourccs = [
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ];
    let predictors = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    let slice_counts = [1usize, 2, 4, 8];

    for &fc in &fourccs {
        // Uly0 needs even W+H; Uly2 needs even W. Use 16×16 for all
        // — satisfies every FOURCC's parity and gives every
        // slice count a non-zero-row plane (`16 / 8 == 2` rows
        // each, the minimum for monotonic offsets).
        let (w, h) = (16, 16);
        for &pred in &predictors {
            for &num_slices in &slice_counts {
                let cfg = cfg_for(fc, w, h, num_slices);
                let bytes = encode_frame_for(fc, w, h, num_slices, pred);

                // Containment + agreement properties on a well-formed
                // input — peek_frame MUST succeed.
                assert_fuzz_invariants(&cfg, &bytes);

                let layout = peek_frame(&cfg, &bytes).expect("well-formed peek");
                let decoded = decode_frame(&cfg, &bytes).expect("well-formed decode");
                assert_eq!(layout.predictor, pred);
                assert_eq!(layout.predictor, decoded.predictor);
                assert_eq!(layout.frame_info, decoded.frame_info);
                assert_eq!(layout.num_slices, num_slices);
                assert_eq!(layout.planes.len(), fc.plane_count());
                for plane in &layout.planes {
                    assert_eq!(plane.slices.len(), num_slices);
                }
            }
        }
    }
}

#[test]
fn roundtrip_inspector_decoder_agreement_under_garbage_appended_bytes() {
    // Pin: appending garbage after the frame_info dword breaks the
    // total-length identity; both inspector and decoder must reject
    // identically. (The inspector path is the round-228 add; the
    // decoder behaviour is the pre-existing baseline.)
    let cfg = cfg_for(Fourcc::Uly2, 16, 16, 2);
    let mut bytes = encode_frame_for(Fourcc::Uly2, 16, 16, 2, Predictor::Gradient);
    bytes.push(0xff);
    bytes.push(0xff);
    let r_peek = peek_frame(&cfg, &bytes);
    let r_dec = decode_frame(&cfg, &bytes);
    // Both must reject — they share the same `offset != frame_info_off`
    // tail check. The exact error variant is implementation-defined
    // but both calls must return `Err`.
    assert!(r_peek.is_err());
    assert!(r_dec.is_err());
}

#[test]
fn roundtrip_inspector_decoder_agreement_under_truncated_tail() {
    // Pin: lopping bytes off the end either truncates the
    // frame_info dword (→ ChunkTooShort) or short-changes the last
    // plane's slice data (→ ChunkTooShort). Inspector and decoder
    // must both reject; neither may panic.
    let cfg = cfg_for(Fourcc::Ulrg, 16, 16, 4);
    let bytes = encode_frame_for(Fourcc::Ulrg, 16, 16, 4, Predictor::Median);
    // Lop off the trailing 1, 4, and 17 bytes to hit three different
    // truncation regimes.
    for &lop in &[1usize, 4, 17] {
        let truncated = &bytes[..bytes.len() - lop];
        let r_peek = peek_frame(&cfg, truncated);
        let r_dec = decode_frame(&cfg, truncated);
        assert!(r_peek.is_err(), "lop={lop} peek should err");
        assert!(r_dec.is_err(), "lop={lop} decode should err");
    }
}

#[test]
fn peek_frame_info_panic_free_on_every_length_under_16() {
    // Tiny inputs are the path most likely to trip a bounds bug in
    // a trailing-dword read. Cover every length 0..=15 against a
    // simple all-zeros pattern.
    for n in 0usize..=15 {
        let buf = vec![0u8; n];
        let r = peek_frame_info(&buf);
        if n < 4 {
            assert!(matches!(r, Err(Error::MissingFrameInfo)));
        } else {
            let (fi, _) = r.unwrap();
            assert_eq!(fi, 0);
        }
    }
}

#[test]
fn peek_frame_panic_free_on_every_payload_under_300() {
    // Mid-size payloads sweep the descriptor / offset-table boundary
    // (descriptor alone is 256 bytes per plane). Drive every length
    // 0..=299 against a Uly2 cfg with num_slices=1.
    let cfg = cfg_for(Fourcc::Uly2, 4, 4, 1);
    for n in 0usize..=299 {
        let buf = vec![0u8; n];
        // Must always return a `Result` — no panic. Variant is
        // implementation-defined; we only care that the call
        // returns.
        let _ = peek_frame(&cfg, &buf);
    }
}
