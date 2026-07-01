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

use oxideav_utvideo::{decode_frame, Extradata, Fourcc, Predictor, StreamConfig};

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

#[test]
fn every_reference_frame_decodes_byte_exact() {
    assert_eq!(CORPUS.len(), 14, "fixture corpus size drifted");
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
