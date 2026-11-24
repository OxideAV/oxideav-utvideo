//! Registry surface added in r494: the decoder states its output shape,
//! 8-bit planar GBR / GBRA select `ULRG` / `ULRA`, and the encoder takes
//! `slices` / `pred` through `CodecParameters::options`.
#![cfg(feature = "registry")]

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Frame, PixelFormat, VideoFrame, VideoPlane,
};
use oxideav_utvideo::{CODEC_ID_STR, Extradata, Fourcc, Predictor, UtVideoEncoderOptions};

fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::register_codecs(&mut reg);
    reg
}

fn params(fmt: PixelFormat, w: u32, h: u32) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    p.pixel_format = Some(fmt);
    p.width = Some(w);
    p.height = Some(h);
    p
}

const FORMATS: [(PixelFormat, Fourcc); 5] = [
    (PixelFormat::Yuv420P, Fourcc::Uly0),
    (PixelFormat::Yuv422P, Fourcc::Uly2),
    (PixelFormat::Yuv444P, Fourcc::Uly4),
    (PixelFormat::Gbrp8, Fourcc::Ulrg),
    (PixelFormat::Gbrap8, Fourcc::Ulra),
];

fn picture(fc: Fourcc, w: u32, h: u32) -> VideoFrame {
    let planes = (0..fc.plane_count())
        .map(|i| {
            let (pw, ph) = fc.plane_dim(i, w, h);
            let data = (0..pw * ph)
                .map(|n| (n * 7 + (n / pw) * 13 + i as u32 * 50) as u8)
                .collect();
            VideoPlane {
                stride: pw as usize,
                data,
            }
        })
        .collect();
    VideoFrame { pts: None, planes }
}

#[test]
fn every_layout_round_trips_through_the_registry_by_pixel_format() {
    let reg = registry();
    for (fmt, fc) in FORMATS {
        let mut enc = reg.first_encoder(&params(fmt, 32, 20)).unwrap();
        let out = enc.output_params().clone();
        assert_eq!(out.tag, Some(CodecTag::fourcc(fc.as_bytes())));
        assert_eq!(out.pixel_format, Some(fmt));

        let pic = picture(fc, 32, 20);
        enc.send_frame(&Frame::Video(pic.clone())).unwrap();
        let pkt = enc.receive_packet().unwrap();

        // The decoder is built from the encoder's stream parameters and
        // states the frame shape before the first packet.
        let mut dec = reg.first_decoder(&out).unwrap();
        let shape = dec.output_params().expect("shape known at construction");
        assert_eq!(shape.pixel_format, Some(fmt));
        assert_eq!((shape.width, shape.height), (Some(32), Some(20)));
        assert_eq!(dec.output_pixel_format(), Some(fmt));
        assert_eq!(dec.output_params_generation(), 0);

        dec.send_packet(&pkt).unwrap();
        let Frame::Video(got) = dec.receive_frame().unwrap() else {
            panic!("video frame expected");
        };
        assert_eq!(got.planes.len(), pic.planes.len());
        for (a, b) in got.planes.iter().zip(&pic.planes) {
            assert_eq!(a.data, b.data, "{fc:?}");
        }
    }
}

#[test]
fn decoder_without_stream_parameters_has_no_shape_yet() {
    let reg = registry();
    let dec = reg
        .first_decoder(&CodecParameters::video(CodecId::new(CODEC_ID_STR)))
        .unwrap();
    assert!(dec.output_params().is_none());
    assert_eq!(dec.output_pixel_format(), None);
}

fn frame_info(packet: &[u8]) -> u32 {
    u32::from_le_bytes(packet[packet.len() - 4..].try_into().unwrap())
}

#[test]
fn slices_and_pred_options_reach_the_bitstream() {
    let reg = registry();
    for (name, pred) in [
        ("none", Predictor::None),
        ("left", Predictor::Left),
        ("gradient", Predictor::Gradient),
        ("median", Predictor::Median),
    ] {
        for slices in [1u32, 3, 10] {
            let mut p = params(PixelFormat::Yuv420P, 32, 20);
            p.options = p
                .options
                .set("slices", slices.to_string())
                .set("pred", name);
            let mut enc = reg.first_encoder(&p).unwrap();
            let ed = Extradata::parse(&enc.output_params().extradata).unwrap();
            assert_eq!(ed.num_slices(), slices as usize);

            let pic = picture(Fourcc::Uly0, 32, 20);
            enc.send_frame(&Frame::Video(pic.clone())).unwrap();
            let pkt = enc.receive_packet().unwrap();
            assert_eq!(frame_info(&pkt.data), pred.as_frame_info_bits());

            let mut dec = reg.first_decoder(enc.output_params()).unwrap();
            dec.send_packet(&pkt).unwrap();
            let Frame::Video(got) = dec.receive_frame().unwrap() else {
                panic!("video frame expected");
            };
            for (a, b) in got.planes.iter().zip(&pic.planes) {
                assert_eq!(a.data, b.data);
            }
        }
    }
}

#[test]
fn slices_option_overrides_supplied_extradata_and_zero_keeps_it() {
    let reg = registry();
    let mut p = params(PixelFormat::Yuv444P, 16, 16);
    p.extradata = Extradata::canonical_extradata_for(Fourcc::Uly4, 4)
        .unwrap()
        .to_bytes()
        .to_vec();
    let kept = reg.first_encoder(&p).unwrap();
    assert_eq!(
        Extradata::parse(&kept.output_params().extradata)
            .unwrap()
            .num_slices(),
        4
    );
    p.options = p.options.set("slices", "0");
    let zero = reg.first_encoder(&p).unwrap();
    assert_eq!(
        Extradata::parse(&zero.output_params().extradata)
            .unwrap()
            .num_slices(),
        4
    );
    let mut q = p.clone();
    q.options = CodecParameters::video(CodecId::new(CODEC_ID_STR))
        .options
        .set("slices", "7");
    let seven = reg.first_encoder(&q).unwrap();
    assert_eq!(
        Extradata::parse(&seven.output_params().extradata)
            .unwrap()
            .num_slices(),
        7
    );
}

#[test]
fn invalid_options_are_refused_at_construction() {
    let reg = registry();
    for (key, value) in [
        ("slices", "257"),
        ("slices", "11"), // 4:2:0 at 32x20 has 10 chroma rows
        ("slices", "-1"),
        ("pred", "paeth"),
        ("quality", "9"),
    ] {
        let mut p = params(PixelFormat::Yuv420P, 32, 20);
        p.options = p.options.set(key, value);
        assert!(
            reg.first_encoder(&p).is_err(),
            "{key}={value} must be refused"
        );
    }
}

#[test]
fn typed_options_builder() {
    let o = UtVideoEncoderOptions::new()
        .with_slices(8)
        .with_predictor(Some(Predictor::Median));
    assert_eq!(o.slices, 8);
    assert_eq!(o.predictor, Some(Predictor::Median));
    assert_eq!(UtVideoEncoderOptions::default().predictor, None);
}
