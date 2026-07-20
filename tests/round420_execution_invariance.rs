//! Round 420 — execution-budget output invariance.
//!
//! The threading contract makes the caller-granted worker budget a
//! pure *scheduling* input: it may change wall-clock time, never
//! bytes. This suite proves that on the full 19-fixture reference
//! corpus (`tests/fixtures/reference/`) for every surface that accepts
//! a budget:
//!
//! 1. **Direct decode.** `decode_frame_with_workers` and
//!    `decode_frame_parallel` under budgets 1 / 2 / 8 reproduce the
//!    serial decode — and the reference pixels — byte-exact.
//! 2. **Direct encode.** `encode_frame_with_workers` and
//!    `encode_frame_parallel` under budgets 1 / 2 / 8 emit payloads
//!    byte-identical to the serial encoder, and the payloads decode
//!    back to the input pixels.
//! 3. **Registry trait path.** Decoders / encoders built through
//!    `CodecRegistry::first_decoder` / `first_encoder` produce
//!    identical output with no `set_execution_context` call (the
//!    serial contract default) and with budgets 1 / 2 / 8.
//!
//! Slices are fully independent (self-contained Huffman bit-streams,
//! `spec/02` §5; per-slice `+128` predictor seed, `spec/04` §§3.1, 4,
//! 5, 7), so any budget-dependent byte would be an implementation bug
//! — a lost strip, a misordered slice, or cross-slice state leakage.

#![cfg(test)]

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, ExecutionContext, Frame, Packet, TimeBase,
    VideoFrame, VideoPlane,
};
use oxideav_utvideo::decoder::{
    decode_frame_parallel, decode_frame_serial, decode_frame_with_workers,
};
use oxideav_utvideo::encoder::{
    encode_frame_parallel, encode_frame_serial, encode_frame_with_workers,
};
use oxideav_utvideo::{
    register_codecs, EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig,
};

const BUDGETS: [usize; 3] = [1, 2, 8];

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
    ($(($name:literal, $fcc:literal, $w:expr, $h:expr, $slices:expr, $pred:ident, $pixels:literal)),* $(,)?) => {
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
                pixels: include_bytes!(concat!("fixtures/reference/", $pixels, ".pixels")),
            },
        )* ]
    };
}

/// The complete committed reference corpus — every fixture under
/// `tests/fixtures/reference/`, including the gradient (mode 2) and
/// 128-slice interop streams (`uly0_left_grad_s128` shares its pixel
/// ground truth with `uly0_median_grad_s8_256`; same deterministic
/// source image).
const CORPUS: &[Fixture] = fixtures![
    (
        "uly0_left_grad_s1",
        b"ULY0",
        32,
        32,
        1,
        Left,
        "uly0_left_grad_s1"
    ),
    (
        "uly0_median_grad_s1",
        b"ULY0",
        32,
        32,
        1,
        Median,
        "uly0_median_grad_s1"
    ),
    (
        "uly0_none_grad_s1",
        b"ULY0",
        32,
        32,
        1,
        None,
        "uly0_none_grad_s1"
    ),
    (
        "uly0_left_noise_s4",
        b"ULY0",
        64,
        64,
        4,
        Left,
        "uly0_left_noise_s4"
    ),
    (
        "uly0_left_ramp_s2",
        b"ULY0",
        64,
        48,
        2,
        Left,
        "uly0_left_ramp_s2"
    ),
    (
        "uly2_left_grad_s1",
        b"ULY2",
        32,
        32,
        1,
        Left,
        "uly2_left_grad_s1"
    ),
    (
        "uly2_median_grad_s2",
        b"ULY2",
        32,
        32,
        2,
        Median,
        "uly2_median_grad_s2"
    ),
    (
        "uly4_left_grad_s1",
        b"ULY4",
        32,
        32,
        1,
        Left,
        "uly4_left_grad_s1"
    ),
    (
        "uly4_none_solid_s1",
        b"ULY4",
        32,
        32,
        1,
        None,
        "uly4_none_solid_s1"
    ),
    (
        "uly4_median_noise_s4",
        b"ULY4",
        48,
        48,
        4,
        Median,
        "uly4_median_noise_s4"
    ),
    (
        "ulrg_left_grad_s1",
        b"ULRG",
        32,
        32,
        1,
        Left,
        "ulrg_left_grad_s1"
    ),
    (
        "ulrg_median_grad_s1",
        b"ULRG",
        32,
        32,
        1,
        Median,
        "ulrg_median_grad_s1"
    ),
    (
        "ulra_left_grad_s1",
        b"ULRA",
        32,
        32,
        1,
        Left,
        "ulra_left_grad_s1"
    ),
    (
        "ulra_none_grad_s1",
        b"ULRA",
        32,
        32,
        1,
        None,
        "ulra_none_grad_s1"
    ),
    (
        "uly0_median_grad_s8_256",
        b"ULY0",
        256,
        256,
        8,
        Median,
        "uly0_median_grad_s8_256"
    ),
    (
        "uly2_left_oddh_s3",
        b"ULY2",
        32,
        33,
        3,
        Left,
        "uly2_left_oddh_s3"
    ),
    (
        "gradient_uly4_mode2_s1",
        b"ULY4",
        32,
        32,
        1,
        Gradient,
        "gradient_uly4_mode2_s1"
    ),
    (
        "gradient_uly4_mode2_s4",
        b"ULY4",
        32,
        32,
        4,
        Gradient,
        "gradient_uly4_mode2_s4"
    ),
    (
        "uly0_left_grad_s128",
        b"ULY0",
        256,
        256,
        128,
        Left,
        "uly0_median_grad_s8_256"
    ),
];

fn config_for(fx: &Fixture) -> StreamConfig {
    let fourcc = Fourcc::from_bytes(*fx.fourcc).unwrap();
    let extradata = Extradata::parse(fx.extradata).unwrap();
    assert_eq!(extradata.num_slices(), fx.num_slices, "{}", fx.name);
    StreamConfig::new(fourcc, fx.width, fx.height, extradata).unwrap()
}

/// Split the fixture's flat reference pixel buffer into per-plane
/// buffers in on-wire order.
fn plane_bufs(fx: &Fixture) -> Vec<Vec<u8>> {
    let fourcc = Fourcc::from_bytes(*fx.fourcc).unwrap();
    let mut out = Vec::with_capacity(fourcc.plane_count());
    let mut off = 0usize;
    for i in 0..fourcc.plane_count() {
        let (pw, ph) = fourcc.plane_dim(i, fx.width, fx.height);
        let n = pw as usize * ph as usize;
        out.push(fx.pixels[off..off + n].to_vec());
        off += n;
    }
    assert_eq!(off, fx.pixels.len(), "{}: plane split residue", fx.name);
    out
}

fn concat_planes(planes: &[oxideav_utvideo::DecodedPlane]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in planes {
        out.extend_from_slice(&p.samples);
    }
    out
}

#[test]
fn corpus_is_the_full_nineteen_fixture_set() {
    assert_eq!(CORPUS.len(), 19, "reference corpus size drifted");
}

/// Direct decode surfaces: budgets 1 / 2 / 8 (and the over-subscribed
/// forced fan-out) are byte-identical to the serial decode and to the
/// reference pixels on every fixture.
#[test]
fn direct_decode_is_invariant_across_budgets() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let baseline = decode_frame_serial(&cfg, fx.chunk)
            .unwrap_or_else(|e| panic!("{}: serial decode: {e}", fx.name));
        assert_eq!(
            concat_planes(&baseline.planes),
            fx.pixels,
            "{}: serial decode vs reference pixels",
            fx.name
        );
        for workers in BUDGETS {
            let budgeted = decode_frame_with_workers(&cfg, fx.chunk, workers).unwrap();
            assert_eq!(
                budgeted, baseline,
                "{}: decode_frame_with_workers({workers}) diverged",
                fx.name
            );
            let forced = decode_frame_parallel(&cfg, fx.chunk, workers).unwrap();
            assert_eq!(
                forced, baseline,
                "{}: decode_frame_parallel({workers}) diverged",
                fx.name
            );
        }
    }
}

/// Direct encode surfaces: budgets 1 / 2 / 8 emit payloads
/// byte-identical to the serial encoder on every fixture (re-encoding
/// the reference pixels with the fixture's own predictor and slice
/// count), and the payload round-trips back to the input pixels.
#[test]
fn direct_encode_is_invariant_across_budgets() {
    for fx in CORPUS {
        let cfg = config_for(fx);
        let frame = EncodedFrame {
            fourcc: cfg.fourcc,
            width: fx.width,
            height: fx.height,
            predictor: fx.pred,
            num_slices: fx.num_slices,
            planes: plane_bufs(fx)
                .into_iter()
                .map(|samples| PlaneInput { samples })
                .collect(),
        };
        let baseline = encode_frame_serial(&frame)
            .unwrap_or_else(|e| panic!("{}: serial encode: {e}", fx.name));
        for workers in BUDGETS {
            let budgeted = encode_frame_with_workers(&frame, workers).unwrap();
            assert_eq!(
                budgeted, baseline,
                "{}: encode_frame_with_workers({workers}) byte drift",
                fx.name
            );
            let forced = encode_frame_parallel(&frame, workers).unwrap();
            assert_eq!(
                forced, baseline,
                "{}: encode_frame_parallel({workers}) byte drift",
                fx.name
            );
        }
        // The (budget-invariant) payload must reproduce the input.
        let decoded = decode_frame_with_workers(&cfg, &baseline, 8).unwrap();
        assert_eq!(
            concat_planes(&decoded.planes),
            fx.pixels,
            "{}: re-encoded payload lost pixels",
            fx.name
        );
    }
}

fn decoder_params(fx: &Fixture) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new("utvideo"));
    p.tag = Some(CodecTag::fourcc(fx.fourcc));
    p.width = Some(fx.width);
    p.height = Some(fx.height);
    p.extradata = fx.extradata.to_vec();
    p
}

/// Decode one fixture through the registry trait path with an optional
/// execution budget; returns the concatenated plane bytes.
fn trait_decode(reg: &CodecRegistry, fx: &Fixture, budget: Option<usize>) -> Vec<u8> {
    let mut dec = reg
        .first_decoder(&decoder_params(fx))
        .unwrap_or_else(|e| panic!("{}: first_decoder: {e}", fx.name));
    if let Some(threads) = budget {
        dec.set_execution_context(&ExecutionContext::with_threads(threads));
    }
    let pkt = Packet::new(0, TimeBase::new(1, 1), fx.chunk.to_vec());
    dec.send_packet(&pkt).unwrap();
    let frame = dec.receive_frame().unwrap();
    let Frame::Video(vf) = frame else {
        panic!("{}: expected a video frame", fx.name);
    };
    let mut out = Vec::new();
    for plane in &vf.planes {
        out.extend_from_slice(&plane.data);
    }
    out
}

/// Encode one fixture's pixels through the registry trait path with an
/// optional execution budget; returns the packet bytes.
fn trait_encode(reg: &CodecRegistry, fx: &Fixture, budget: Option<usize>) -> Vec<u8> {
    let mut enc = reg
        .first_encoder(&decoder_params(fx))
        .unwrap_or_else(|e| panic!("{}: first_encoder: {e}", fx.name));
    if let Some(threads) = budget {
        enc.set_execution_context(&ExecutionContext::with_threads(threads));
    }
    let fourcc = Fourcc::from_bytes(*fx.fourcc).unwrap();
    let planes: Vec<VideoPlane> = plane_bufs(fx)
        .into_iter()
        .enumerate()
        .map(|(i, data)| VideoPlane {
            stride: fourcc.plane_dim(i, fx.width, fx.height).0 as usize,
            data,
        })
        .collect();
    enc.send_frame(&Frame::Video(VideoFrame { pts: None, planes }))
        .unwrap();
    enc.receive_packet().unwrap().data
}

/// Registry trait path, decode side: no `set_execution_context` call
/// (the serial contract default) and budgets 1 / 2 / 8 all reproduce
/// the reference pixels byte-exact.
#[test]
fn trait_decode_is_invariant_across_budgets() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    for fx in CORPUS {
        let default = trait_decode(&reg, fx, None);
        assert_eq!(
            default, fx.pixels,
            "{}: trait decode (serial default) vs reference pixels",
            fx.name
        );
        for workers in BUDGETS {
            let budgeted = trait_decode(&reg, fx, Some(workers));
            assert_eq!(
                budgeted, default,
                "{}: trait decode with budget {workers} diverged from serial default",
                fx.name
            );
        }
    }
}

/// Registry trait path, encode side: no `set_execution_context` call
/// and budgets 1 / 2 / 8 all emit byte-identical packets, and the
/// packet round-trips back to the input pixels.
#[test]
fn trait_encode_is_invariant_across_budgets() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    for fx in CORPUS {
        let default = trait_encode(&reg, fx, None);
        for workers in BUDGETS {
            let budgeted = trait_encode(&reg, fx, Some(workers));
            assert_eq!(
                budgeted, default,
                "{}: trait encode with budget {workers} byte drift",
                fx.name
            );
        }
        // Round-trip: the trait-encoded packet must decode back to the
        // input pixels through the trait decoder (budget 8 so the
        // multi-slice fixtures exercise the fan-out on the way back).
        let cfg = config_for(fx);
        let decoded = decode_frame_with_workers(&cfg, &default, 8)
            .unwrap_or_else(|e| panic!("{}: trait-encoded payload decode: {e}", fx.name));
        assert_eq!(
            concat_planes(&decoded.planes),
            fx.pixels,
            "{}: trait-encoded payload lost pixels",
            fx.name
        );
    }
}
