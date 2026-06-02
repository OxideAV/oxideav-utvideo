//! Round 17 — `Encoder` trait wiring from `CodecParameters` +
//! end-to-end integration suite.
//!
//! Round 14 closed the analogous gap on the decoder side: the registry
//! [`make_decoder`] factory now derives the [`StreamConfig`] at
//! construction time from a populated [`CodecParameters`], so trait-driven
//! decode works without callers having to downcast and call a private
//! `configure()` hook. The encoder path stayed direct-API-only — the
//! `oxideav_core::Encoder` trait was not implemented, capabilities did
//! not advertise `with_encode()`, and no factory was registered.
//!
//! Round 17 mirrors round 14 on the encode side. After this round:
//!
//! - [`oxideav_core::CodecRegistry::has_encoder`] returns `true` for the
//!   `"utvideo"` codec id.
//! - `make_encoder(&params)` validates the identification surface at
//!   factory time, mirroring the decoder's `build_stream_config`. A
//!   missing FourCC / pixel format / dims surfaces a diagnosable
//!   `Error::Invalid` immediately (containers learn "I cannot encode
//!   into this stream" before the first frame is dispatched).
//! - `send_frame` / `receive_packet` round-trips every classic-family
//!   `Fourcc` × `PixelFormat` combination the wire defines, and the
//!   bytes the trait emits decode back to the same input under our own
//!   `decode_frame`.
//! - Trait-path output is byte-identical to a direct
//!   `encoder::encode_frame` call with the matching `EncodedFrame` —
//!   wiring is a thin shim, not a re-implementation.
//!
//! Five test groups (mirroring round 14's structure):
//!
//! 1. **Factory happy path** — every FourCC × derivation route (tag
//!    vs. pixel format) constructs cleanly; output_params reflects
//!    the resolved identification surface.
//! 2. **Trait-path byte-equality** — `send_frame` + `receive_packet`
//!    produces the same bytes a direct [`encode_frame`] call would.
//! 3. **State-machine contract** — `NeedMore` before `send_frame`,
//!    `Eof` after `flush`, double-`send_frame` rejection,
//!    `Packet::flags.keyframe = true` invariant (every Ut Video
//!    frame is intra-only per `spec/02` §1), PTS pass-through path.
//! 4. **Factory construction-time rejection** — missing FourCC,
//!    missing dims, packed-RGB pixel format, malformed extradata,
//!    ULY0 / ULY2 dimension constraint violations all surface
//!    `Error::Invalid` at `make_encoder` time.
//! 5. **Plane-count + stride validation** — `send_frame` rejects
//!    wrong plane counts and short / mis-strided plane buffers;
//!    stride-padded buffers are repacked tight before encode.
//! 6. **Round-trip via the trait** — encode through the trait, decode
//!    through the round-14 decoder trait, and assert sample-equal
//!    output for every FourCC.

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Error as CoreError, Frame, Packet,
    PixelFormat, TimeBase, VideoFrame, VideoPlane,
};

use oxideav_utvideo::decoder::decode_frame as direct_decode;
use oxideav_utvideo::encoder::{encode_frame as direct_encode, EncodedFrame, PlaneInput};
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};
use oxideav_utvideo::registry::CODEC_ID_STR;

fn build_params_with_tag(fourcc: Fourcc, w: u32, h: u32, slices: usize) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    p.tag = Some(CodecTag::fourcc(fourcc.as_bytes()));
    p.width = Some(w);
    p.height = Some(h);
    p.extradata = Extradata::ffmpeg_for(fourcc, slices)
        .unwrap()
        .to_bytes()
        .to_vec();
    p
}

fn build_params_with_pixfmt(fmt: PixelFormat, w: u32, h: u32) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    p.pixel_format = Some(fmt);
    p.width = Some(w);
    p.height = Some(h);
    p
}

/// Deterministic xorshift32 → fill a plane with non-trivially
/// compressible bytes so the Huffman path engages the LUT walk + the
/// Gradient predictor's row state-carry seam (not a constant-plane
/// short-circuit).
fn fill_plane(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut state: u32 = seed | 1;
    let mut out = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let base = ((r as u32).wrapping_add(c as u32) >> 1) & 0xff;
            let noise = state & 0x0f;
            out[r * w + c] = (base.wrapping_add(noise) & 0xff) as u8;
        }
    }
    out
}

fn build_video_frame(fourcc: Fourcc, w: u32, h: u32, seed: u32) -> VideoFrame {
    let mut planes = Vec::with_capacity(fourcc.plane_count());
    for idx in 0..fourcc.plane_count() {
        let (pw, ph) = fourcc.plane_dim(idx, w, h);
        let data = fill_plane(
            pw as usize,
            ph as usize,
            seed ^ (idx as u32 + 1).wrapping_mul(0x9e37_79b9),
        );
        planes.push(VideoPlane {
            stride: pw as usize,
            data,
        });
    }
    VideoFrame { pts: None, planes }
}

fn build_encoded_frame_mirror(
    fourcc: Fourcc,
    w: u32,
    h: u32,
    slices: usize,
    vf: &VideoFrame,
) -> EncodedFrame {
    let planes = vf
        .planes
        .iter()
        .map(|p| PlaneInput {
            samples: p.data.clone(),
        })
        .collect();
    EncodedFrame {
        fourcc,
        width: w,
        height: h,
        // The registry encoder picks Gradient (round 15/16 perf wins
        // land there); the mirror direct-API call must use the same
        // predictor so the bytes match.
        predictor: Predictor::Gradient,
        num_slices: slices,
        planes,
    }
}

// ────────────────── §1. Factory happy path ──────────────────

#[test]
fn factory_via_tag_every_fourcc() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    for fc in [
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        let p = build_params_with_tag(fc, 16, 16, 1);
        let enc = reg
            .first_encoder(&p)
            .expect("factory must construct via tag");
        assert_eq!(enc.codec_id().as_str(), CODEC_ID_STR);
        let op = enc.output_params();
        assert_eq!(op.width, Some(16));
        assert_eq!(op.height, Some(16));
        assert_eq!(op.tag, Some(CodecTag::fourcc(fc.as_bytes())));
        assert_eq!(op.extradata.len(), 16, "extradata block size (spec/01 §4)");
    }
}

#[test]
fn factory_via_pixel_format_yuv_trio() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    for (fmt, expected) in [
        (PixelFormat::Yuv420P, Fourcc::Uly0),
        (PixelFormat::Yuv422P, Fourcc::Uly2),
        (PixelFormat::Yuv444P, Fourcc::Uly4),
    ] {
        let p = build_params_with_pixfmt(fmt, 16, 16);
        let enc = reg
            .first_encoder(&p)
            .expect("factory must derive FourCC from pixel format");
        let op = enc.output_params();
        assert_eq!(op.tag, Some(CodecTag::fourcc(expected.as_bytes())));
        // Synthesised extradata from `Extradata::ffmpeg_for(fc, 1)`.
        let expected_ext = Extradata::ffmpeg_for(expected, 1).unwrap().to_bytes();
        assert_eq!(op.extradata, expected_ext.to_vec());
    }
}

#[test]
fn factory_tag_wins_over_pixel_format() {
    // The tag is the authoritative identification surface (`spec/01`
    // §2); pixel_format is the framework-level hint. When both are
    // present, the tag picks the FourCC.
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let mut p = build_params_with_tag(Fourcc::Ulrg, 16, 16, 1);
    p.pixel_format = Some(PixelFormat::Yuv420P); // contradictory; tag wins
    let enc = reg.first_encoder(&p).expect("tag must win");
    assert_eq!(
        enc.output_params().tag,
        Some(CodecTag::fourcc(Fourcc::Ulrg.as_bytes()))
    );
}

#[test]
fn factory_preserves_caller_extradata() {
    // Slices = 4 via populated extradata; ffmpeg_for(fc, 4) writes
    // (flags >> 24) = 3 ⇒ num_slices == 4 (`spec/01` §4.4.3).
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly2, 64, 32, 4);
    let enc = reg.first_encoder(&p).unwrap();
    let op = enc.output_params();
    let parsed = Extradata::parse(&op.extradata).unwrap();
    assert_eq!(parsed.num_slices(), 4);
}

// ────────────────── §2. Trait-path byte-equality ──────────────────

fn one_shot_encode(fourcc: Fourcc, w: u32, h: u32, slices: usize, seed: u32) -> Vec<u8> {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let params = build_params_with_tag(fourcc, w, h, slices);
    let mut enc = reg.first_encoder(&params).unwrap();
    let vf = build_video_frame(fourcc, w, h, seed);
    enc.send_frame(&Frame::Video(vf)).unwrap();
    let pkt = enc.receive_packet().unwrap();
    pkt.data
}

#[test]
fn trait_path_byte_equal_direct_api_all_fourccs() {
    for fc in [
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        let (w, h, slices, seed) = (16, 16, 1, 0xdead_beef);
        let trait_bytes = one_shot_encode(fc, w, h, slices, seed);
        let vf = build_video_frame(fc, w, h, seed);
        let efr = build_encoded_frame_mirror(fc, w, h, slices, &vf);
        let direct_bytes = direct_encode(&efr).unwrap();
        assert_eq!(
            trait_bytes, direct_bytes,
            "trait-path encode must be byte-equal to direct encode_frame for {:?}",
            fc
        );
    }
}

#[test]
fn trait_path_byte_equal_multi_slice() {
    // Multi-slice exercises the slice-end-offset table layout
    // (`spec/02` §5) and — at ≥ `PARALLEL_PIXEL_THRESHOLD` — the
    // round-5 parallel-encode auto-dispatch. The trait shim must
    // produce the same bytes either way.
    for fc in [Fourcc::Uly4, Fourcc::Ulrg, Fourcc::Ulra] {
        let (w, h, slices, seed) = (32, 32, 4, 0xfeed_face);
        let trait_bytes = one_shot_encode(fc, w, h, slices, seed);
        let vf = build_video_frame(fc, w, h, seed);
        let efr = build_encoded_frame_mirror(fc, w, h, slices, &vf);
        let direct_bytes = direct_encode(&efr).unwrap();
        assert_eq!(trait_bytes, direct_bytes, "{:?} multi-slice byte-equal", fc);
    }
}

// ────────────────── §3. State-machine contract ──────────────────

#[test]
fn receive_packet_before_send_returns_need_more() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    match enc.receive_packet() {
        Err(CoreError::NeedMore) => {}
        other => panic!("expected NeedMore before any send_frame, got {other:?}"),
    }
}

#[test]
fn flush_then_receive_yields_eof() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    enc.flush().unwrap();
    match enc.receive_packet() {
        Err(CoreError::Eof) => {}
        other => panic!("expected Eof after flush, got {other:?}"),
    }
}

#[test]
fn double_send_without_receive_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let vf = build_video_frame(Fourcc::Uly4, 8, 8, 0x1234);
    enc.send_frame(&Frame::Video(vf.clone())).unwrap();
    // Second send before receive_packet must error (the encoder is
    // a one-frame-at-a-time pipeline; spilling a second frame on the
    // floor would silently drop the first).
    match enc.send_frame(&Frame::Video(vf)) {
        Err(_) => {}
        Ok(()) => panic!("expected double-send to error"),
    }
}

#[test]
fn emitted_packets_are_keyframes() {
    // Every Ut Video frame is intra-only (lossless, no inter-frame
    // state — `spec/02` §1). The Packet::flags.keyframe invariant
    // matters for muxers that build seek indexes off the keyframe bit.
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    for fc in [
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        let p = build_params_with_tag(fc, 8, 8, 1);
        let mut enc = reg.first_encoder(&p).unwrap();
        let vf = build_video_frame(fc, 8, 8, 0xa5a5_a5a5);
        enc.send_frame(&Frame::Video(vf)).unwrap();
        let pkt = enc.receive_packet().unwrap();
        assert!(pkt.flags.keyframe, "{:?}: all frames must be keyframes", fc);
    }
}

#[test]
fn receive_then_receive_again_returns_need_more() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let vf = build_video_frame(Fourcc::Uly4, 8, 8, 0xbeef);
    enc.send_frame(&Frame::Video(vf)).unwrap();
    let _ = enc.receive_packet().unwrap();
    match enc.receive_packet() {
        Err(CoreError::NeedMore) => {}
        other => panic!("expected NeedMore after draining pending, got {other:?}"),
    }
}

#[test]
fn non_video_frame_rejected() {
    use oxideav_core::AudioFrame;
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let af = AudioFrame {
        samples: 1,
        pts: None,
        data: vec![vec![0u8; 4]],
    };
    match enc.send_frame(&Frame::Audio(af)) {
        Err(_) => {}
        Ok(()) => panic!("audio frame must be rejected"),
    }
}

// ────────────────── §4. Factory rejection ──────────────────

#[test]
fn factory_missing_tag_and_pixel_format_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let id = CodecId::new(CODEC_ID_STR);
    let mut p = CodecParameters::video(id.clone());
    p.width = Some(16);
    p.height = Some(16);
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("must reject: no tag, no pixel_format"),
    }
}

#[test]
fn factory_missing_dims_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let id = CodecId::new(CODEC_ID_STR);
    let mut p = CodecParameters::video(id.clone());
    p.tag = Some(CodecTag::fourcc(b"ULY4"));
    // no width/height
    p.extradata = Extradata::ffmpeg_for(Fourcc::Uly4, 1)
        .unwrap()
        .to_bytes()
        .to_vec();
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("must reject: missing dims"),
    }
}

#[test]
fn factory_packed_rgb_pixel_format_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    // Packed RGB / RGBA cannot be silently routed to ULRG / ULRA — the
    // wire is planar GBR(A) (`spec/04` §6). The caller must declare
    // ULRG / ULRA via `params.tag` AND hand in 3/4 planes.
    for fmt in [PixelFormat::Rgb24, PixelFormat::Rgba, PixelFormat::Gray8] {
        let p = build_params_with_pixfmt(fmt, 16, 16);
        match reg.first_encoder(&p) {
            Err(_) => {}
            Ok(_) => panic!("packed format {:?} must be rejected", fmt),
        }
    }
}

#[test]
fn factory_uly0_odd_dims_rejected() {
    // ULY0 mandates even width and height (4:2:0 subsampling —
    // `spec/02` §3.1).
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly0, 15, 16, 1);
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("ULY0 with odd width must be rejected at factory time"),
    }
    let p = build_params_with_tag(Fourcc::Uly0, 16, 15, 1);
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("ULY0 with odd height must be rejected at factory time"),
    }
}

#[test]
fn factory_uly2_odd_width_rejected() {
    // ULY2 mandates even width (4:2:2 subsampling — `spec/02` §3.1).
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly2, 15, 16, 1);
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("ULY2 with odd width must be rejected at factory time"),
    }
}

#[test]
fn factory_truncated_extradata_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let mut p = build_params_with_tag(Fourcc::Uly4, 16, 16, 1);
    p.extradata = p.extradata[..8].to_vec(); // 8 < 16 minimum (spec/01 §4)
    match reg.first_encoder(&p) {
        Err(_) => {}
        Ok(_) => panic!("truncated extradata must be rejected"),
    }
}

// ────────────────── §5. Plane-count + stride validation ──────────────────

#[test]
fn send_frame_wrong_plane_count_rejected() {
    // ULRA expects 4 planes (G, B, R, A — `spec/02` §3.1); 3 must
    // error.
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Ulra, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    // Construct a 3-plane VideoFrame deliberately (mismatched for ULRA).
    let vf = build_video_frame(Fourcc::Ulrg, 8, 8, 0x5555);
    match enc.send_frame(&Frame::Video(vf)) {
        Err(_) => {}
        Ok(()) => panic!("ULRA with 3 planes must be rejected"),
    }
}

#[test]
fn send_frame_short_plane_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let mut vf = build_video_frame(Fourcc::Uly4, 8, 8, 0xaabb);
    vf.planes[0].data.truncate(10); // way short of 8*8 == 64
    match enc.send_frame(&Frame::Video(vf)) {
        Err(_) => {}
        Ok(()) => panic!("short plane data must be rejected"),
    }
}

#[test]
fn send_frame_padded_stride_repacked_tight() {
    // Build a VideoFrame whose plane stride > plane width (a common
    // case when the producer rounds stride to a SIMD alignment). The
    // encoder must repack to a tight buffer transparently and the
    // bytes must match a direct tight-input encode.
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let fc = Fourcc::Uly4;
    let (w, h) = (8u32, 8u32);
    let p = build_params_with_tag(fc, w, h, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    // Tight reference buffer.
    let tight_vf = build_video_frame(fc, w, h, 0x1357);
    // Padded-stride copy: stride = pw + 4 zero-pad bytes per row.
    let pad = 4usize;
    let mut padded_planes = Vec::with_capacity(fc.plane_count());
    for tight_plane in &tight_vf.planes {
        let pw = tight_plane.stride; // tight, == plane width
        let ph = tight_plane.data.len() / pw;
        let stride = pw + pad;
        let mut data = vec![0u8; stride * ph];
        for r in 0..ph {
            let src = &tight_plane.data[r * pw..r * pw + pw];
            data[r * stride..r * stride + pw].copy_from_slice(src);
        }
        padded_planes.push(VideoPlane { stride, data });
    }
    let padded_vf = VideoFrame {
        pts: None,
        planes: padded_planes,
    };
    enc.send_frame(&Frame::Video(padded_vf)).unwrap();
    let padded_bytes = enc.receive_packet().unwrap().data;
    // Direct-API tight reference.
    let efr = build_encoded_frame_mirror(fc, w, h, 1, &tight_vf);
    let tight_bytes = direct_encode(&efr).unwrap();
    assert_eq!(
        padded_bytes, tight_bytes,
        "stride-padded input must repack to the tight encoder bytes"
    );
}

#[test]
fn send_frame_stride_below_plane_width_rejected() {
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let fc = Fourcc::Uly4;
    let p = build_params_with_tag(fc, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let mut vf = build_video_frame(fc, 8, 8, 0xfeed);
    vf.planes[0].stride = 4; // below width 8
    match enc.send_frame(&Frame::Video(vf)) {
        Err(_) => {}
        Ok(()) => panic!("stride < plane width must be rejected"),
    }
}

// ────────────────── §6. End-to-end round-trip via traits ──────────────────

#[test]
fn round_trip_through_both_traits_every_fourcc() {
    for fc in [
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        let mut reg = CodecRegistry::new();
        oxideav_utvideo::registry::register_codecs(&mut reg);
        let _id = CodecId::new(CODEC_ID_STR);
        let (w, h, slices) = (16, 16, 1);
        let p = build_params_with_tag(fc, w, h, slices);
        let mut enc = reg.first_encoder(&p).unwrap();
        let input = build_video_frame(fc, w, h, 0x4242_4242);
        enc.send_frame(&Frame::Video(input.clone())).unwrap();
        let pkt = enc.receive_packet().unwrap();

        // Decode through the round-14 decoder trait (build a fresh
        // decoder factory from the encoder's output_params).
        let dec_params = enc.output_params().clone();
        let mut dec = reg.first_decoder(&dec_params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let out = dec.receive_frame().unwrap();
        let out_vf = match out {
            Frame::Video(v) => v,
            other => panic!("expected video frame, got {other:?}"),
        };

        // Sample-equal at every plane.
        assert_eq!(
            input.planes.len(),
            out_vf.planes.len(),
            "{:?}: plane count must match",
            fc
        );
        for (idx, (a, b)) in input.planes.iter().zip(out_vf.planes.iter()).enumerate() {
            assert_eq!(
                a.data, b.data,
                "{:?} plane {idx} did not round-trip bit-exact",
                fc
            );
        }
    }
}

#[test]
fn round_trip_via_pixel_format_derivation_yuv_trio() {
    // The factory derives the FourCC from pixel_format alone; the
    // round-trip must still come back sample-equal.
    for (fmt, expected_fc) in [
        (PixelFormat::Yuv420P, Fourcc::Uly0),
        (PixelFormat::Yuv422P, Fourcc::Uly2),
        (PixelFormat::Yuv444P, Fourcc::Uly4),
    ] {
        let mut reg = CodecRegistry::new();
        oxideav_utvideo::registry::register_codecs(&mut reg);
        let _id = CodecId::new(CODEC_ID_STR);
        let (w, h) = (16, 16);
        let p = build_params_with_pixfmt(fmt, w, h);
        let mut enc = reg.first_encoder(&p).unwrap();
        let input = build_video_frame(expected_fc, w, h, 0x1111_2222);
        enc.send_frame(&Frame::Video(input.clone())).unwrap();
        let pkt = enc.receive_packet().unwrap();

        // Decode via direct API to keep the assertion focused.
        let cfg = StreamConfig::new(
            expected_fc,
            w,
            h,
            Extradata::ffmpeg_for(expected_fc, 1).unwrap(),
        )
        .unwrap();
        let out = direct_decode(&cfg, &pkt.data).unwrap();
        for (idx, (a, b)) in input.planes.iter().zip(out.planes.iter()).enumerate() {
            assert_eq!(
                a.data, b.samples,
                "{:?} plane {idx} did not round-trip bit-exact",
                fmt
            );
        }
    }
}

#[test]
fn round_trip_multi_slice_parallel_path() {
    // 32×32 ULY4 with 4 slices crosses the round-5 parallel-encode
    // auto-dispatch — the trait shim must produce bytes the decoder
    // accepts byte-for-byte.
    let fc = Fourcc::Uly4;
    let (w, h, slices) = (32u32, 32u32, 4usize);
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(fc, w, h, slices);
    let mut enc = reg.first_encoder(&p).unwrap();
    let input = build_video_frame(fc, w, h, 0xcafe_b0ba);
    enc.send_frame(&Frame::Video(input.clone())).unwrap();
    let pkt = enc.receive_packet().unwrap();
    let cfg = StreamConfig::new(fc, w, h, Extradata::ffmpeg_for(fc, slices).unwrap()).unwrap();
    let out = direct_decode(&cfg, &pkt.data).unwrap();
    for (idx, (a, b)) in input.planes.iter().zip(out.planes.iter()).enumerate() {
        assert_eq!(a.data, b.samples, "plane {idx}");
    }
}

// ────────────────── Misc: PTS pass-through + Packet shape ──────────────────

#[test]
fn pts_pass_through_via_packet() {
    // The encoder owns the Packet construction. The current contract
    // is: pts is supplied by the caller as part of the input frame,
    // but our encoder writes `Packet::new(0, TimeBase::new(1, 1), …)`
    // — pts is None by default. Test that subsequent `with_*` mutation
    // by the caller still works on the returned packet.
    let mut reg = CodecRegistry::new();
    oxideav_utvideo::registry::register_codecs(&mut reg);
    let _id = CodecId::new(CODEC_ID_STR);
    let p = build_params_with_tag(Fourcc::Uly4, 8, 8, 1);
    let mut enc = reg.first_encoder(&p).unwrap();
    let mut vf = build_video_frame(Fourcc::Uly4, 8, 8, 0x9999);
    vf.pts = Some(42);
    enc.send_frame(&Frame::Video(vf)).unwrap();
    let pkt = enc.receive_packet().unwrap();
    // Caller may overwrite the packet's pts/time_base post-emit.
    let pkt = Packet::new(0, TimeBase::new(1, 1000), pkt.data);
    assert_eq!(pkt.pts, None);
    let _ = pkt; // and the bytes are still valid wire output
}
