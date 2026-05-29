//! Round 14 — `oxideav_core::Decoder` trait integration through the
//! registry factory.
//!
//! Rounds 1–13 exercised the crate-local [`decode_frame`] surface
//! directly, with no callers driving the
//! [`Decoder`](oxideav_core::Decoder) trait end-to-end. The registry
//! module's only smoke tests were "the codec id is installed" and "the
//! five FourCCs resolve" — the factory → `send_packet` → `receive_frame`
//! path that every container driver actually uses had **zero** coverage.
//!
//! The round-14 wiring change ([`make_decoder`] in `src/registry.rs`)
//! now builds a [`StreamConfig`] from `CodecParameters.tag`,
//! `CodecParameters.extradata`, and `CodecParameters.width` /
//! `.height`, mirroring the `oxideav-huffyuv` pattern so trait-driven
//! decode works without a downcast to call the hidden `configure()`
//! hook. This suite locks the new contract and the per-FourCC + per-API
//! invariants:
//!
//! 1. **Factory happy path on every FourCC.** Resolve the FourCC →
//!    codec id via the registry, then `make_decoder` with extradata
//!    `Extradata::ffmpeg_for(fc, slices).to_bytes()` + dims yields a
//!    `Box<dyn Decoder>` that, fed a packet our `encode_frame` produced,
//!    returns a [`Frame::Video`] with the expected plane count + per-
//!    plane stride + per-plane payload-size pinned to FOURCC-derived
//!    dimensions.
//! 2. **Packet bytes survive the trait path exactly.** The
//!    `Frame::Video` returned from the trait carries plane samples
//!    byte-identical to a direct
//!    [`oxideav_utvideo::decode_frame`] call on the same chunk-payload,
//!    so the trait wrapper introduces no transform.
//! 3. **PTS pass-through.** `Packet.pts` flows into `VideoFrame.pts`
//!    verbatim.
//! 4. **`NeedMore` before any packet.** Calling `receive_frame` on a
//!    fresh decoder, before any `send_packet`, returns
//!    [`oxideav_core::Error::NeedMore`].
//! 5. **`Eof` after `flush` with no pending packet.** Calling `flush`
//!    then `receive_frame` returns [`oxideav_core::Error::Eof`].
//! 6. **`send_packet` twice without intervening `receive_frame`
//!    rejects.** The second call returns an error (decoder's
//!    one-packet-in-flight rule, see `src/registry.rs`).
//! 7. **Factory rejects malformed extradata at construction time.**
//!    `make_decoder` with truncated / Huffman-bit-clear / interlaced
//!    extradata returns [`oxideav_core::Error::InvalidData`] without
//!    ever building a decoder.
//! 8. **Factory accepts missing extradata** (the container has not yet
//!    populated it). The decoder is constructed but `receive_frame`
//!    returns `InvalidData` on the first packet so the caller is told
//!    why decode cannot proceed.
//! 9. **Factory rejects malformed dims** (ULY0 / ULY2 chroma-subsampling
//!    constraint violations from `spec/02` §3.2). These trip
//!    [`oxideav_core::Error::InvalidData`] at construction, not at
//!    decode.

#![cfg(test)]

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, CodecTag, Decoder, Error as CoreError, Frame, Packet,
    PixelFormat, ProbeContext, TimeBase,
};
use oxideav_utvideo::decoder::{decode_frame, DecodedPlane, PlaneLabel};
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};
use oxideav_utvideo::registry::{register_codecs, CODEC_ID_STR};

// ──────────────────────── helpers ────────────────────────

/// Build a `CodecRegistry` with utvideo wired in.
fn registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    reg
}

/// Build a `CodecParameters` for a Ut Video stream that the registry
/// `make_decoder` factory consumes. Mirrors what `oxideav-avi` would
/// hand us off an `strh + strf + LIST hdrl` chain.
fn make_params(fc: Fourcc, w: u32, h: u32, slices: usize) -> CodecParameters {
    let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(PixelFormat::Yuv420P);
    params.tag = Some(CodecTag::fourcc(fc.as_bytes()));
    params.extradata = Extradata::ffmpeg_for(fc, slices)
        .expect("valid slice count")
        .to_bytes()
        .to_vec();
    params
}

/// Make an `EncodedFrame` with deterministic gradient samples per plane.
fn encoded_frame(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> EncodedFrame {
    let planes: Vec<PlaneInput> = (0..fc.plane_count())
        .map(|idx| {
            let (pw, ph) = fc.plane_dim(idx, w, h);
            let n = (pw as usize) * (ph as usize);
            let mut samples = Vec::with_capacity(n);
            for i in 0..n {
                // Stable, per-FOURCC + per-plane content so a wrong plane
                // pairing (alpha treated as luma etc.) trips a diff.
                samples.push(((i + (idx + 1) * 7) & 0xff) as u8);
            }
            PlaneInput { samples }
        })
        .collect();
    EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes,
    }
}

fn cfg(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, slices).expect("valid slice count");
    StreamConfig::new(fc, w, h, extradata).expect("dims OK")
}

/// Construct a fresh `Decoder` from the registry-routed FourCC.
fn build_decoder(reg: &CodecRegistry, params: &CodecParameters) -> Box<dyn Decoder> {
    reg.first_decoder(params)
        .expect("factory should accept valid params")
}

fn pkt(bytes: Vec<u8>, pts: Option<i64>) -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 1000), bytes);
    p.pts = pts;
    p
}

// ──────────────────────── tests ────────────────────────

#[test]
fn factory_happy_path_each_fourcc_returns_video_frame_with_correct_plane_shape() {
    let reg = registry();
    let cases = [
        (Fourcc::Uly0, 32u32, 32u32),
        (Fourcc::Uly2, 32, 16),
        (Fourcc::Uly4, 16, 16),
        (Fourcc::Ulrg, 16, 16),
        (Fourcc::Ulra, 16, 16),
    ];
    for (fc, w, h) in cases {
        let params = make_params(fc, w, h, 1);
        let mut dec = build_decoder(&reg, &params);
        let payload = encode_frame(&encoded_frame(fc, w, h, 1, Predictor::Gradient)).unwrap();
        dec.send_packet(&pkt(payload, Some(42))).unwrap();
        let frame = match dec.receive_frame().expect("decode produces a frame") {
            Frame::Video(v) => v,
            other => panic!("expected Frame::Video, got {other:?} for {fc:?}"),
        };

        assert_eq!(frame.pts, Some(42), "PTS should pass through for {fc:?}");
        assert_eq!(
            frame.planes.len(),
            fc.plane_count(),
            "plane count for {fc:?}"
        );
        for (idx, plane) in frame.planes.iter().enumerate() {
            let (pw, ph) = fc.plane_dim(idx, w, h);
            assert_eq!(
                plane.stride, pw as usize,
                "stride for {fc:?} plane {idx} should equal plane width"
            );
            assert_eq!(
                plane.data.len(),
                (pw as usize) * (ph as usize),
                "data length for {fc:?} plane {idx}",
            );
        }
    }
}

#[test]
fn trait_path_returns_bytes_identical_to_direct_decode_call() {
    // Property 2 — the registry wrapper is a thin trait adapter; no
    // sample is transformed between `decode_frame` and the
    // `Frame::Video` returned through the trait.
    let reg = registry();
    let fc = Fourcc::Uly2;
    let (w, h) = (32u32, 32u32);
    let params = make_params(fc, w, h, 1);
    let payload = encode_frame(&encoded_frame(fc, w, h, 1, Predictor::Median)).unwrap();

    // Direct path — the same bytes through `decode_frame`.
    let direct = decode_frame(&cfg(fc, w, h, 1), &payload).unwrap();
    let direct_samples: Vec<&[u8]> = direct.planes.iter().map(|p| p.samples.as_slice()).collect();

    // Trait path.
    let mut dec = build_decoder(&reg, &params);
    dec.send_packet(&pkt(payload, Some(99))).unwrap();
    let trait_frame = match dec.receive_frame().unwrap() {
        Frame::Video(v) => v,
        other => panic!("expected Frame::Video, got {other:?}"),
    };
    let trait_samples: Vec<&[u8]> = trait_frame
        .planes
        .iter()
        .map(|p| p.data.as_slice())
        .collect();

    assert_eq!(trait_samples.len(), direct_samples.len());
    for (i, (t, d)) in trait_samples.iter().zip(direct_samples.iter()).enumerate() {
        assert_eq!(t, d, "trait vs direct mismatch on plane {i}");
    }
}

#[test]
fn receive_frame_before_send_packet_returns_need_more() {
    // Property 4 — empty decoder reports `NeedMore`.
    let reg = registry();
    let params = make_params(Fourcc::Ulrg, 16, 16, 1);
    let mut dec = build_decoder(&reg, &params);
    let err = dec.receive_frame().expect_err("should fail with NeedMore");
    assert!(
        matches!(err, CoreError::NeedMore),
        "expected NeedMore, got {err:?}"
    );
}

#[test]
fn receive_frame_after_flush_with_no_pending_returns_eof() {
    // Property 5 — flush + receive_frame (with no in-flight packet) is Eof.
    let reg = registry();
    let params = make_params(Fourcc::Ulrg, 16, 16, 1);
    let mut dec = build_decoder(&reg, &params);
    dec.flush().unwrap();
    let err = dec.receive_frame().expect_err("post-flush should be Eof");
    assert!(matches!(err, CoreError::Eof), "expected Eof, got {err:?}");
}

#[test]
fn send_packet_twice_without_receive_frame_rejects() {
    // Property 6 — one-packet-in-flight rule.
    let reg = registry();
    let fc = Fourcc::Ulrg;
    let (w, h) = (16u32, 16u32);
    let params = make_params(fc, w, h, 1);
    let mut dec = build_decoder(&reg, &params);
    let payload = encode_frame(&encoded_frame(fc, w, h, 1, Predictor::Left)).unwrap();
    dec.send_packet(&pkt(payload.clone(), Some(1))).unwrap();
    let err = dec
        .send_packet(&pkt(payload, Some(2)))
        .expect_err("second send_packet should reject");
    // We don't pin the exact CoreError variant — the registry chose
    // `Error::other(...)` — but we DO pin that the second push errors,
    // so a future relaxation to "queue" semantics is a deliberate API
    // change that has to update this test.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("receive_frame") || msg.contains("Other") || msg.contains("oxideav-utvideo"),
        "unexpected error from double send_packet: {msg}"
    );
}

#[test]
fn factory_rejects_truncated_extradata() {
    // Property 7 — extradata < 16 bytes is rejected at factory time.
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.extradata.truncate(8);
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("truncated extradata should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_rejects_huffman_bit_clear_extradata() {
    // Property 7 — Huffman bit clear (raw mode) is rejected at factory.
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.extradata[12] = 0x00; // flags &= !0x0000_0001
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("Huffman-clear extradata should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_rejects_interlaced_extradata() {
    // Property 7 — interlaced bit set is rejected at factory.
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.extradata[13] |= 0x08; // flags |= 0x0000_0800
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("interlaced extradata should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_rejects_wrong_frame_info_size_extradata() {
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.extradata[8] = 0x08; // frame_info_size = 8 instead of 4
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("frame_info_size != 4 should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_accepts_empty_extradata_then_decode_fails_diagnostically() {
    // Property 8 — empty extradata is "container hasn't populated yet",
    // accepted at construction; decode of a packet then fails so the
    // caller is told what's missing rather than silently decoding into
    // unconfigured space.
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.extradata.clear();
    let mut dec = reg
        .first_decoder(&params)
        .expect("empty-extradata params should still build a decoder");
    let payload =
        encode_frame(&encoded_frame(Fourcc::Ulrg, 16, 16, 1, Predictor::Gradient)).unwrap();
    dec.send_packet(&pkt(payload, None)).unwrap();
    let err = dec
        .receive_frame()
        .expect_err("unconfigured decode should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stream config") || msg.contains("not configured"),
        "expected 'stream config not configured' diagnostic, got {msg}"
    );
}

#[test]
fn factory_rejects_uly0_odd_width() {
    // Property 9 — chroma constraint enforced at construction.
    let reg = registry();
    let mut params = make_params(Fourcc::Uly0, 32, 32, 1);
    params.width = Some(31);
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("ULY0 odd width should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_rejects_uly2_odd_width() {
    let reg = registry();
    let mut params = make_params(Fourcc::Uly2, 32, 16, 1);
    params.width = Some(31);
    let err = reg
        .first_decoder(&params)
        .err()
        .expect("ULY2 odd width should fail at factory");
    assert!(matches!(err, CoreError::InvalidData(_)), "got {err:?}");
}

#[test]
fn factory_accepts_missing_tag_defers_configuration() {
    // Property 8 (sibling) — no `params.tag` (legacy demuxer hasn't set
    // it) defers config; the call still succeeds at factory time.
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.tag = None;
    let mut dec = reg
        .first_decoder(&params)
        .expect("missing-tag params should still build a decoder");
    let payload = encode_frame(&encoded_frame(Fourcc::Ulrg, 16, 16, 1, Predictor::Left)).unwrap();
    dec.send_packet(&pkt(payload, None)).unwrap();
    let err = dec
        .receive_frame()
        .expect_err("unconfigured decode should fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stream config") || msg.contains("not configured"),
        "expected 'not configured' diagnostic, got {msg}"
    );
}

#[test]
fn factory_accepts_missing_dims_defers_configuration() {
    let reg = registry();
    let mut params = make_params(Fourcc::Ulrg, 16, 16, 1);
    params.width = None;
    let _dec = reg
        .first_decoder(&params)
        .expect("missing-dims params should still build a decoder");
}

#[test]
fn each_plane_label_matches_fourcc_in_direct_decode_for_trait_pairing() {
    // Pin the plane-order contract the registry wrapper relies on:
    // every plane the trait emits sits at the same index as
    // `PlaneLabel::for_fourcc(fc, idx)`. Bug-class guard against a
    // future re-ordering inside `map_to_video_frame`.
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        let (w, h) = (16u32, 16u32);
        let payload = encode_frame(&encoded_frame(fc, w, h, 1, Predictor::Left)).unwrap();
        let direct = decode_frame(&cfg(fc, w, h, 1), &payload).unwrap();
        for (idx, DecodedPlane { label, .. }) in direct.planes.iter().enumerate() {
            assert_eq!(
                *label,
                PlaneLabel::for_fourcc(fc, idx),
                "plane label drift on {fc:?} idx {idx}"
            );
        }
    }
}

#[test]
fn fourcc_resolution_through_probecontext_routes_to_utvideo() {
    // Cross-check: the codec id the registry resolves to from a
    // container `ProbeContext(FourCC=ULY0)` is the same id the trait
    // path was built against. A drift here means the registry wiring
    // and the trait factory are using different codec ids.
    let reg = registry();
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        let tag = CodecTag::fourcc(fc.as_bytes());
        let resolved = reg
            .resolve_tag_ref(&ProbeContext::new(&tag))
            .map(|c| c.as_str());
        assert_eq!(resolved, Some(CODEC_ID_STR), "FourCC {fc:?}");
    }
}

#[test]
fn factory_passes_through_pts_none() {
    // PTS = None must stay None.
    let reg = registry();
    let fc = Fourcc::Uly4;
    let (w, h) = (16u32, 16u32);
    let params = make_params(fc, w, h, 1);
    let mut dec = build_decoder(&reg, &params);
    let payload = encode_frame(&encoded_frame(fc, w, h, 1, Predictor::None)).unwrap();
    dec.send_packet(&pkt(payload, None)).unwrap();
    let frame = match dec.receive_frame().unwrap() {
        Frame::Video(v) => v,
        other => panic!("expected Frame::Video, got {other:?}"),
    };
    assert_eq!(frame.pts, None);
}

#[test]
fn trait_decode_works_on_multi_slice_payload() {
    // Trait wrapper must propagate to the slice-parallel path the same
    // way the direct call does (the cfg-based dispatch in
    // `decode_frame` chooses).
    let reg = registry();
    let fc = Fourcc::Uly4;
    let (w, h) = (128u32, 128u32);
    let params = make_params(fc, w, h, 4);
    let mut dec = build_decoder(&reg, &params);
    let payload = encode_frame(&encoded_frame(fc, w, h, 4, Predictor::Gradient)).unwrap();
    dec.send_packet(&pkt(payload, Some(7))).unwrap();
    let frame = match dec.receive_frame().unwrap() {
        Frame::Video(v) => v,
        other => panic!("expected Frame::Video, got {other:?}"),
    };
    assert_eq!(frame.pts, Some(7));
    assert_eq!(frame.planes.len(), 3);
    for plane in &frame.planes {
        assert_eq!(plane.stride, w as usize);
        assert_eq!(plane.data.len(), (w as usize) * (h as usize));
    }
}

#[test]
fn codec_id_accessor_matches_registration() {
    // `Decoder::codec_id` must return the registered id.
    let reg = registry();
    let params = make_params(Fourcc::Ulrg, 16, 16, 1);
    let dec = build_decoder(&reg, &params);
    assert_eq!(dec.codec_id().as_str(), CODEC_ID_STR);
}

#[test]
fn flush_is_idempotent_when_called_twice() {
    // Pin flush idempotency: two calls are not an error and both leave
    // the decoder in Eof mode.
    let reg = registry();
    let params = make_params(Fourcc::Ulrg, 16, 16, 1);
    let mut dec = build_decoder(&reg, &params);
    dec.flush().unwrap();
    dec.flush().unwrap();
    let err = dec.receive_frame().expect_err("post-flush should be Eof");
    assert!(matches!(err, CoreError::Eof), "got {err:?}");
}

#[test]
fn capabilities_reflect_intra_only_lossless() {
    // Round-14 wiring change must NOT have lost the codec-level
    // capability flags the round-1 registration sets — lossless + intra
    // + the `utvideo_sw` implementation name.
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let (_id, imp) = reg
        .all_implementations()
        .find(|(id, _)| id.as_str() == CODEC_ID_STR)
        .expect("utvideo registered");
    assert_eq!(imp.caps.implementation, "utvideo_sw");
    assert!(imp.caps.lossless);
    assert!(imp.caps.intra_only);
    assert!(imp.caps.decode);
}
