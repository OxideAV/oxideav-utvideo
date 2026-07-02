//! Round 382 — reference-stream golden decode conformance.
//!
//! Every prior test in this crate is a *self*-round-trip: encode with
//! the in-crate encoder, decode with the in-crate decoder, assert
//! plane equality. That proves the encoder and decoder are mutually
//! inverse, but it does **not** prove the decoder reproduces the
//! byte-exact pixels a *reference* Ut Video stream carries — a bug that
//! is symmetric across our own encode/decode (e.g. a wrong-but-
//! consistent predictor seed or plane order) would pass every
//! self-round-trip yet mis-decode real streams.
//!
//! This round closes that gap. The `tests/fixtures/reference/` corpus
//! holds pre-extracted raw Ut Video **frame bodies** (the `00dc`
//! chunk payloads, `spec/02` §1) together with their 16-byte wire
//! extradata (`spec/01` §2) and the ground-truth per-plane pixel bytes.
//! The frame bodies and pixel references were produced offline by a
//! black-box reference-encoder / reference-decoder validator binary
//! (invoked purely as an opaque I/O oracle — no third-party codec
//! source is read at any stage, per the crate's clean-room charter);
//! the fixtures are committed as static bytes so this test is pure Rust
//! and needs no external binary at build or CI time.
//!
//! The reference decoder's raw planar output order matches this
//! crate's on-wire [`DecodedPlane`] order exactly: Y,U,V for the YUV
//! FourCCs and G,B,R(,A) for the RGB FourCCs (`spec/02` §3,
//! `spec/04` §6), so the concatenation of our decoded planes must equal
//! the reference pixel bytes byte-for-byte.
//!
//! Coverage: all five FourCCs (`ULY0`/`ULY2`/`ULY4`/`ULRG`/`ULRA`), the
//! three reference-encoder-supported predictors (none / left / median,
//! `spec/04` §§3, 4, 5), single- and multi-slice frames (1/2/4), and
//! low-entropy (structured gradient, solid), ramp, and high-entropy
//! (noise) content that between them exercise the single-symbol plane
//! fast path (`spec/05` §6.1), the dense Huffman LUT, and the long-code
//! tier-scan fallback (`spec/05` §7).

use oxideav_utvideo::decoder::{decode_frame_parallel, decode_frame_serial};
use oxideav_utvideo::{
    decode_frame, decode_frame_strict, encode_frame, peek_frame, peek_frame_info, EncodedFrame,
    Extradata, Fourcc, PlaneInput, Predictor, StreamConfig,
};

struct Fixture {
    name: &'static str,
    fourcc: &'static [u8; 4],
    width: u32,
    height: u32,
    num_slices: usize,
    pred: Predictor,
    extradata: &'static [u8],
    chunk: &'static [u8],
    pixels: &'static [u8],
}

macro_rules! fixtures {
    ($(($name:literal, $fcc:literal, $w:expr, $h:expr, $slices:expr, $pred:ident)),* $(,)?) => {
        &[ $(
            Fixture {
                name: $name,
                fourcc: $fcc,
                width: $w,
                height: $h,
                num_slices: $slices,
                pred: Predictor::$pred,
                extradata: include_bytes!(concat!("fixtures/reference/", $name, ".extradata")),
                chunk: include_bytes!(concat!("fixtures/reference/", $name, ".chunk")),
                pixels: include_bytes!(concat!("fixtures/reference/", $name, ".pixels")),
            },
        )* ]
    };
}

const CORPUS: &[Fixture] = fixtures![
    ("uly0_left_grad_s1", b"ULY0", 32, 32, 1, Left),
    ("uly0_median_grad_s1", b"ULY0", 32, 32, 1, Median),
    ("uly0_none_grad_s1", b"ULY0", 32, 32, 1, None),
    ("uly0_left_noise_s4", b"ULY0", 64, 64, 4, Left),
    ("uly0_left_ramp_s2", b"ULY0", 64, 48, 2, Left),
    ("uly2_left_grad_s1", b"ULY2", 32, 32, 1, Left),
    ("uly2_median_grad_s2", b"ULY2", 32, 32, 2, Median),
    ("uly4_left_grad_s1", b"ULY4", 32, 32, 1, Left),
    ("uly4_none_solid_s1", b"ULY4", 32, 32, 1, None),
    ("uly4_median_noise_s4", b"ULY4", 48, 48, 4, Median),
    ("ulrg_left_grad_s1", b"ULRG", 32, 32, 1, Left),
    ("ulrg_median_grad_s1", b"ULRG", 32, 32, 1, Median),
    ("ulra_left_grad_s1", b"ULRA", 32, 32, 1, Left),
    ("ulra_none_grad_s1", b"ULRA", 32, 32, 1, None),
    // Crosses the decoder's 64 Ki-pixel auto-parallel threshold with 8
    // slices, so the default `decode_frame` entry dispatches the
    // multi-threaded path on a *real* stream (median-at-scale).
    ("uly0_median_grad_s8_256", b"ULY0", 256, 256, 8, Median),
    // Odd frame height on a 4:2:2 FourCC: `spec/02` §3.1 subsamples the
    // U/V width but NOT the height, so plane heights are 33/33/33 while
    // widths are 32/16/16 (`spec/02` §3.2 lets ULY2 accept odd height).
    ("uly2_left_oddh_s3", b"ULY2", 32, 33, 3, Left),
];

/// Build a [`StreamConfig`] from a fixture's committed wire extradata.
fn config_for(fx: &Fixture) -> StreamConfig {
    let fourcc = Fourcc::from_bytes(*fx.fourcc)
        .unwrap_or_else(|e| panic!("{}: unknown fourcc: {e}", fx.name));
    let extradata = Extradata::parse(fx.extradata)
        .unwrap_or_else(|e| panic!("{}: extradata parse: {e}", fx.name));
    // The wire extradata must independently agree with the fixture's
    // declared slice count (`spec/01` §4.4.3).
    assert_eq!(
        extradata.num_slices(),
        fx.num_slices,
        "{}: extradata slice count",
        fx.name
    );
    StreamConfig::new(fourcc, fx.width, fx.height, extradata)
        .unwrap_or_else(|e| panic!("{}: stream config: {e}", fx.name))
}

/// Concatenate a decoded frame's planes in on-wire order.
fn concat_planes(planes: &[oxideav_utvideo::DecodedPlane]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in planes {
        out.extend_from_slice(&p.samples);
    }
    out
}

/// Split a flat reference pixel buffer into per-plane [`PlaneInput`]s in
/// on-wire order using the FourCC's plane geometry (`spec/02` §3.1).
fn planes_from_pixels(fx: &Fixture) -> Vec<PlaneInput> {
    let fourcc = Fourcc::from_bytes(*fx.fourcc).unwrap();
    let mut out = Vec::with_capacity(fourcc.plane_count());
    let mut off = 0usize;
    for i in 0..fourcc.plane_count() {
        let (pw, ph) = fourcc.plane_dim(i, fx.width, fx.height);
        let n = pw as usize * ph as usize;
        out.push(PlaneInput {
            samples: fx.pixels[off..off + n].to_vec(),
        });
        off += n;
    }
    assert_eq!(off, fx.pixels.len(), "{}: plane split residue", fx.name);
    out
}

#[test]
fn every_reference_frame_decodes_byte_exact() {
    assert_eq!(CORPUS.len(), 16, "fixture corpus size drifted");
    for fx in CORPUS {
        let cfg = config_for(fx);
        let decoded = decode_frame(&cfg, fx.chunk)
            .unwrap_or_else(|e| panic!("{}: decode failed: {e}", fx.name));

        // Predictor recovered from the trailing frame-info dword
        // (`spec/02` §6.1) must match what the reference encoder was
        // asked to use.
        assert_eq!(decoded.predictor, fx.pred, "{}: predictor", fx.name);
        assert_eq!(decoded.width, fx.width, "{}: width", fx.name);
        assert_eq!(decoded.height, fx.height, "{}: height", fx.name);
        assert_eq!(
            decoded.planes.len(),
            cfg.fourcc.plane_count(),
            "{}: plane count",
            fx.name
        );

        let got = concat_planes(&decoded.planes);
        assert_eq!(
            got.len(),
            fx.pixels.len(),
            "{}: decoded pixel-buffer length {} != reference {}",
            fx.name,
            got.len(),
            fx.pixels.len()
        );
        if got != fx.pixels {
            // Locate the first mismatch for a legible failure.
            let at = got
                .iter()
                .zip(fx.pixels.iter())
                .position(|(a, b)| a != b)
                .unwrap();
            panic!(
                "{}: pixel mismatch at byte {} (decoded {} vs reference {})",
                fx.name, at, got[at], fx.pixels[at]
            );
        }
    }
}

/// Reference frame bodies are produced by a spec-conformant encoder, so
/// every slice's bit stream is zero-padded to its 32-bit word boundary
/// (`spec/05` §4.3). The opt-in strict decoder ([`decode_frame_strict`])
/// must therefore accept every fixture *and* return output byte-identical
/// to the lenient path — a real stream must never trip
/// [`oxideav_utvideo::Error::NonZeroPadding`].
#[test]
fn strict_decode_accepts_every_reference_frame() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let lenient = decode_frame(&cfg, fx.chunk).unwrap();
        let strict = decode_frame_strict(&cfg, fx.chunk).unwrap_or_else(|e| {
            panic!("{}: strict decode rejected a reference frame: {e}", fx.name)
        });
        assert_eq!(
            strict, lenient,
            "{}: strict decode diverged from lenient decode",
            fx.name
        );
        assert_eq!(
            concat_planes(&strict.planes),
            fx.pixels,
            "{}: strict decode pixels",
            fx.name
        );
    }
}

/// The serial and parallel decode paths are documented as bit-exact
/// equivalents (`decoder` module docs; `spec/02` §7 "Implementation
/// notes" on per-slice independence). Both must reproduce the reference
/// pixels exactly on every fixture, including the genuinely multi-slice
/// frames (`*_s2`, `*_s4`) where the parallel fan-out actually splits
/// work across threads.
#[test]
fn serial_and_parallel_paths_reproduce_reference() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let serial = decode_frame_serial(&cfg, fx.chunk).unwrap();
        let parallel = decode_frame_parallel(&cfg, fx.chunk).unwrap();
        assert_eq!(
            serial, parallel,
            "{}: serial vs parallel divergence",
            fx.name
        );
        assert_eq!(
            concat_planes(&serial.planes),
            fx.pixels,
            "{}: serial path pixels",
            fx.name
        );
    }
}

/// Cross-validate the decode-free [`peek_frame`] inspector against a
/// real decode of the same reference stream: the byte-walk must recover
/// the same plane geometry, the same per-slice row partitioning
/// (`spec/02` §5.2), and Huffman-descriptor primitives that stay within
/// the spec's wire bounds — all without building a Huffman table or
/// decoding a single residual.
#[test]
fn inspector_layout_agrees_with_reference_decode() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let decoded = decode_frame(&cfg, fx.chunk).unwrap();
        let layout = peek_frame(&cfg, fx.chunk)
            .unwrap_or_else(|e| panic!("{}: peek_frame failed: {e}", fx.name));

        // Frame-info / predictor recovered decode-free must match the
        // full decode (`spec/02` §6.1).
        let (fi, pred) = peek_frame_info(fx.chunk).unwrap();
        assert_eq!(fi, decoded.frame_info, "{}: peek frame_info", fx.name);
        assert_eq!(pred, decoded.predictor, "{}: peek predictor", fx.name);

        assert_eq!(
            layout.num_slices, fx.num_slices,
            "{}: layout slices",
            fx.name
        );
        assert_eq!(
            layout.planes.len(),
            decoded.planes.len(),
            "{}: layout plane count",
            fx.name
        );

        for (pl, dp) in layout.planes.iter().zip(decoded.planes.iter()) {
            assert_eq!(pl.width, dp.width, "{}: plane width", fx.name);
            assert_eq!(pl.height, dp.height, "{}: plane height", fx.name);
            assert_eq!(
                pl.slices.len(),
                fx.num_slices,
                "{}: plane slice count",
                fx.name
            );

            // Per-slice row partitioning must tile the plane exactly and
            // the per-slice pixel counts must sum to the plane area
            // (`spec/02` §5.2).
            let mut expect_row = 0u32;
            let mut total_px = 0u32;
            for (s_idx, s) in pl.slices.iter().enumerate() {
                assert_eq!(
                    s.row_start, expect_row,
                    "{}: plane {} slice {} row_start",
                    fx.name, pl.plane_idx, s_idx
                );
                let want_end = (pl.height as usize * (s_idx + 1) / fx.num_slices) as u32;
                assert_eq!(
                    s.row_end, want_end,
                    "{}: plane {} slice {} row_end",
                    fx.name, pl.plane_idx, s_idx
                );
                assert_eq!(
                    s.pixel_count,
                    s.row_count() * pl.width,
                    "{}: plane {} slice {} pixel_count",
                    fx.name,
                    pl.plane_idx,
                    s_idx
                );
                expect_row = s.row_end;
                total_px += s.pixel_count;
            }
            assert_eq!(expect_row, pl.height, "{}: slice rows tile height", fx.name);
            assert_eq!(
                total_px,
                pl.width * pl.height,
                "{}: slice pixel_count sum",
                fx.name
            );

            // Descriptor primitives stay inside the spec wire bounds
            // (`spec/05` §2.1, §7.1); a single-symbol plane carries no
            // active code lengths (`spec/05` §6.1).
            assert!(
                pl.max_code_length <= 254,
                "{}: max_code_length wire bound",
                fx.name
            );
            if pl.is_single_symbol {
                assert_eq!(
                    pl.active_symbol_count, 0,
                    "{}: single-symbol active count",
                    fx.name
                );
                assert_eq!(pl.max_code_length, 0, "{}: single-symbol max len", fx.name);
            } else {
                assert!(
                    pl.min_code_length <= pl.max_code_length,
                    "{}: min<=max code length",
                    fx.name
                );
            }
        }
    }
}

/// Cross-check the crate's [`Extradata::ffmpeg_for`] builder against the
/// *actual wire extradata* of every reference stream: for each fixture,
/// the 16 bytes our builder produces for `(fourcc, num_slices)` must be
/// byte-identical to the extradata the reference encoder wrote
/// (`spec/01` §2 layout; §2.1 encoder-version constant; §2.2
/// source-format tags including the non-FOURCC ULRG/ULRA encodings
/// `00 00 01 18` / `00 00 02 18`; §2.3 frame-info size 4; §2.4 flags =
/// Huffman bit + slice top byte). Until now the builder was pinned only
/// against spec prose; this pins it against captured wire bytes across
/// all five FourCCs and slice counts 1/2/3/4/8.
#[test]
fn extradata_builder_matches_reference_wire_bytes() {
    for fx in CORPUS {
        let fourcc = Fourcc::from_bytes(*fx.fourcc).unwrap();
        let built = Extradata::ffmpeg_for(fourcc, fx.num_slices).unwrap();
        assert_eq!(
            &built.to_bytes()[..],
            fx.extradata,
            "{}: built extradata differs from reference wire bytes",
            fx.name
        );
    }
}

/// The `uly4_none_solid_s1` fixture is a constant-per-plane input under
/// the identity predictor, so every plane collapses to the single-symbol
/// fast path (`spec/05` §6.1): a lone `code_length = 0` sentinel and
/// zero slice-data bytes. This pins that the inspector recognises the
/// mode on a real reference stream (not just synthesised descriptors).
#[test]
fn solid_reference_frame_is_all_single_symbol() {
    let fx = CORPUS
        .iter()
        .find(|f| f.name == "uly4_none_solid_s1")
        .unwrap();
    let cfg = config_for(fx);
    let layout = peek_frame(&cfg, fx.chunk).unwrap();
    for pl in &layout.planes {
        assert!(
            pl.is_single_symbol,
            "solid frame plane {} should be single-symbol",
            pl.plane_idx
        );
        assert_eq!(
            pl.slices[0].len(),
            0,
            "single-symbol plane carries no slice data"
        );
    }
}

/// Encoder round-trip on **reference-authentic** pixels: feed the
/// reference-decoded pixel planes back through the in-crate encoder
/// (using the fixture's own predictor and slice count) and decode again.
///
/// Unlike the crate's pre-existing self-round-trips over *synthetic*
/// inputs, the pixels here are the ground-truth output of a real
/// reference stream — including the median-with-gradient-wrap content
/// (`uly0_median_grad_s1`, `uly2_median_grad_s2`, `ulrg_median_grad_s1`,
/// `uly4_median_noise_s4`) that exposed the mode-3 clip bug. This guards
/// the fixed modular-median formula on the **encode** side too (a
/// wrong forward-median would produce residuals the fixed inverse-median
/// could not invert back to the reference pixels), across all five
/// FourCCs and the RGB decorrelation transform (`spec/04` §6).
#[test]
fn encoder_round_trips_reference_pixels() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let frame = EncodedFrame {
            fourcc: cfg.fourcc,
            width: fx.width,
            height: fx.height,
            predictor: fx.pred,
            num_slices: fx.num_slices,
            planes: planes_from_pixels(fx),
        };
        let encoded =
            encode_frame(&frame).unwrap_or_else(|e| panic!("{}: re-encode failed: {e}", fx.name));
        let decoded = decode_frame(&cfg, &encoded)
            .unwrap_or_else(|e| panic!("{}: re-decode failed: {e}", fx.name));
        assert_eq!(
            decoded.predictor, fx.pred,
            "{}: re-encoded predictor",
            fx.name
        );
        assert_eq!(
            concat_planes(&decoded.planes),
            fx.pixels,
            "{}: encoder round-trip lost reference pixels",
            fx.name
        );
    }
}

/// The `uly0_median_grad_s8_256` fixture is 256×256 = 65 536 luma pixels
/// with 8 slices, so it meets the decoder's documented 64 Ki-pixel
/// auto-parallel threshold — the default [`decode_frame`] entry point
/// dispatches the multi-threaded per-slice path (`decoder` module docs,
/// `spec/02` §7). This pins that the auto-dispatch parallel path decodes
/// a real reference stream byte-exact (the smaller fixtures all stay on
/// the serial side of the threshold), and that it agrees with the forced
/// serial path on the same bytes.
#[test]
fn large_reference_frame_exercises_auto_parallel_path() {
    let fx = CORPUS
        .iter()
        .find(|f| f.name == "uly0_median_grad_s8_256")
        .unwrap();
    // Guard the premise: the frame must actually cross the threshold, or
    // this test silently stops covering the parallel auto-dispatch.
    assert!(
        (fx.width as usize) * (fx.height as usize) >= 64 * 1024 && fx.num_slices > 1,
        "large fixture no longer crosses the auto-parallel threshold"
    );
    let cfg = config_for(fx);
    let auto = decode_frame(&cfg, fx.chunk).unwrap();
    let serial = decode_frame_serial(&cfg, fx.chunk).unwrap();
    assert_eq!(auto, serial, "auto-parallel path diverged from serial");
    assert_eq!(
        concat_planes(&auto.planes),
        fx.pixels,
        "auto-parallel decode lost reference pixels"
    );
}

// ---------------------------------------------------------------------
// Gradient (predictor mode 2) interop corpus.
//
// The reference *encoder* rejects `-pred gradient` ("Gradient prediction
// is not supported.", `spec/04` §7.3), so mode 2 cannot appear in the
// reference-encoder-produced corpus above — yet the reference *decoder*
// accepts gradient streams a spec-compliant encoder emits (`spec/04`
// §7.3, `spec/05` §10, where the mode-2 wire format was itself pinned by
// feeding hand-crafted gradient bitstreams to the reference decoder).
//
// These two fixtures are produced by *this crate's* encoder in gradient
// mode (`frame_info & 0x300 == 0x200`) and were confirmed offline to be
// accepted by the black-box reference decoder, which reconstructs
// exactly the committed `.pixels` (invoked purely as an opaque I/O
// oracle — no third-party codec source read). So the committed
// invariant is identical to the corpus above — `reference_decoder(chunk)
// == pixels` — and additionally proves this crate's gradient *encoder*
// output is wire-compatible with the reference decoder. The pure-Rust
// test below re-establishes `our_decoder(chunk) == pixels`.
//
// Coverage the reference-encoder corpus cannot reach:
// - the gradient interior `P = (left + top - top_left) mod 256`
//   (`spec/04` §7.1),
// - the gradient column-0 edge `P = top` (NOT the median/left
//   continuous-wrap — `spec/04` §7.1), exercised for `r > r_start`,
// - the per-slice `+128` seed under gradient (the `_s4` fixture's four
//   slices each re-seed at their top row).
const GRADIENT_CORPUS: &[Fixture] = fixtures![
    ("gradient_uly4_mode2_s1", b"ULY4", 32, 32, 1, Gradient),
    ("gradient_uly4_mode2_s4", b"ULY4", 32, 32, 4, Gradient),
];

#[test]
fn gradient_mode2_streams_decode_byte_exact() {
    for fx in GRADIENT_CORPUS {
        let cfg = config_for(fx);
        let decoded = decode_frame(&cfg, fx.chunk)
            .unwrap_or_else(|e| panic!("{}: gradient decode failed: {e}", fx.name));
        assert_eq!(
            decoded.predictor,
            Predictor::Gradient,
            "{}: frame_info must select mode 2 (0x200)",
            fx.name
        );
        assert_eq!(
            concat_planes(&decoded.planes),
            fx.pixels,
            "{}: gradient decode diverged from reference pixels",
            fx.name
        );

        // Strict + serial/parallel equivalence hold for gradient too.
        assert_eq!(
            decode_frame_strict(&cfg, fx.chunk).unwrap(),
            decoded,
            "{}: strict gradient decode",
            fx.name
        );
        assert_eq!(
            decode_frame_parallel(&cfg, fx.chunk).unwrap(),
            decode_frame_serial(&cfg, fx.chunk).unwrap(),
            "{}: gradient serial vs parallel",
            fx.name
        );
    }
}

/// 128-slice interop fixture — the finest legal slice partitioning for a
/// 256×256 ULY0 frame: the reference encoder caps its slice count at the
/// subsampling-applied plane height (observed encoder rejection at 256
/// slices: "Slice count 256 is larger than the subsampling-applied
/// height 128"), so 128 slices gives the 128-row chroma planes exactly
/// **one row per slice** — every chroma slice consists of a lone
/// Left-predictor row seeded at +128 with no inter-row wrap at all
/// (`spec/04` §4).
///
/// Like the gradient corpus, this chunk was produced by *this crate's*
/// encoder (the reference encoder's own default never goes past 8 slices
/// on this frame size) and confirmed offline to be accepted by the
/// black-box reference decoder with byte-exact pixel reconstruction, so
/// `reference_decoder(chunk) == pixels` holds for the committed bytes.
/// The pixel ground-truth is shared with `uly0_median_grad_s8_256` (same
/// deterministic gradient input), so only the chunk + extradata are
/// committed for this fixture.
#[test]
fn slice_count_128_interop_stream_decodes_byte_exact() {
    let fx = Fixture {
        name: "uly0_left_grad_s128",
        fourcc: b"ULY0",
        width: 256,
        height: 256,
        num_slices: 128,
        pred: Predictor::Left,
        extradata: include_bytes!("fixtures/reference/uly0_left_grad_s128.extradata"),
        chunk: include_bytes!("fixtures/reference/uly0_left_grad_s128.chunk"),
        pixels: include_bytes!("fixtures/reference/uly0_median_grad_s8_256.pixels"),
    };
    let cfg = config_for(&fx);
    let decoded = decode_frame(&cfg, fx.chunk).unwrap();
    assert_eq!(decoded.predictor, Predictor::Left);
    assert_eq!(
        concat_planes(&decoded.planes),
        fx.pixels,
        "128-slice decode diverged from reference pixels"
    );
    // Chroma planes must partition into exactly one row per slice.
    let layout = peek_frame(&cfg, fx.chunk).unwrap();
    for pl in &layout.planes[1..] {
        assert_eq!(pl.height, 128, "chroma plane height");
        for s in &pl.slices {
            assert_eq!(s.row_count(), 1, "one row per chroma slice");
        }
    }
    // Strict + serial/parallel equivalence.
    assert_eq!(decode_frame_strict(&cfg, fx.chunk).unwrap(), decoded);
    assert_eq!(
        decode_frame_parallel(&cfg, fx.chunk).unwrap(),
        decode_frame_serial(&cfg, fx.chunk).unwrap(),
        "128-slice serial vs parallel"
    );
}
