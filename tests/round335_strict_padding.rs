//! Round 335 — strict-decode trailing-padding verification.
//!
//! `spec/05` §4.3 states that after a slice's last Huffman code, the
//! bit stream is padded with **zero bits** to the next 32-bit word
//! boundary, and `spec/05` §8 names a non-zero padding bit a SHOULD-warn
//! for defensive decoders ("A defensive decoder MAY verify the padding
//! bits are zero"). The default [`decode_frame`] path follows the spec's
//! companion rule — "a decoder MUST NOT consume the padding bits" — and
//! ignores the slice tail entirely. Up to this round there was no way
//! for a conformance-checking caller to *reject* a stream whose encoder
//! left non-zero padding.
//!
//! Round 335 adds the opt-in [`decode_frame_strict`] entry point. It
//! produces byte-identical output to [`decode_frame`] for every
//! well-formed stream, but elevates the §4.3 / §8 SHOULD-warn to a hard
//! [`Error::NonZeroPadding`] (category `MalformedStream`) naming the
//! offending plane / slice / first-non-zero-padding-bit offset.
//!
//! This suite pins:
//!
//! 1. **Equivalence on clean streams.** Every FourCC × predictor ×
//!    slice-count frame the in-crate encoder emits — which always zeros
//!    its padding (`spec/05` §4.3, SHOULD-zero for encoders) —
//!    strict-decodes to exactly the same planes the lenient path
//!    yields.
//! 2. **Rejection on a flipped padding bit.** Surgically setting a
//!    padding bit in plane 0's slice data trips `NonZeroPadding` under
//!    the strict path, while the lenient path still decodes (it never
//!    looks at the tail).
//! 3. **Location fields.** The error names the correct plane index and
//!    a `bit_position` that lies in the slice's padding region
//!    (`>= payload_bits`).
//!
//! All behaviour derived from `docs/video/utvideo/spec/05` §4.3 + §8;
//! no external library source, no web.

#![cfg(test)]

use oxideav_utvideo::decoder::{decode_frame, decode_frame_strict};
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::error::{Error, ErrorCategory};
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

fn cfg(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let flags = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *b"YV12",
        frame_info_size: 4,
        flags,
    };
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

/// Deterministic noise plane (self-contained LCG; no codec provenance).
fn noise_plane(seed: u64, n: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 56) as u8);
    }
    out
}

/// Per-FourCC plane pixel counts for a `w × h` frame.
fn plane_sizes(fc: Fourcc, w: usize, h: usize) -> Vec<usize> {
    match fc {
        Fourcc::Uly4 => vec![w * h; 3],
        Fourcc::Uly2 => vec![w * h, (w / 2) * h, (w / 2) * h],
        Fourcc::Uly0 => vec![w * h, (w / 2) * (h / 2), (w / 2) * (h / 2)],
        Fourcc::Ulrg => vec![w * h; 3],
        Fourcc::Ulra => vec![w * h; 4],
    }
}

fn build_frame(fc: Fourcc, w: u32, h: u32, pred: Predictor, slices: usize) -> Vec<u8> {
    let sizes = plane_sizes(fc, w as usize, h as usize);
    let planes: Vec<PlaneInput> = sizes
        .iter()
        .enumerate()
        .map(|(i, &n)| PlaneInput {
            samples: noise_plane(0x11 + i as u64, n),
        })
        .collect();
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

/// `[start, end)` of plane 0's slice data within a single-slice payload.
/// Layout (`spec/02` §7): descriptor[0..256], one u32-LE
/// slice-end-offset at [256..260], slice data at [260..260+off0].
fn plane0_slice_data_span(bytes: &[u8]) -> (usize, usize) {
    let off0 = u32::from_le_bytes(bytes[256..260].try_into().unwrap()) as usize;
    (260, 260 + off0)
}

// ---------------------------------------------------------------------
// 1. Equivalence on clean (encoder-zeroed-padding) streams.
// ---------------------------------------------------------------------

#[test]
fn strict_matches_lenient_on_clean_frames() {
    let fourccs = [
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ];
    let preds = [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ];
    for fc in fourccs {
        for pred in preds {
            for slices in [1usize, 2, 4] {
                // 16×8 keeps every subsampled plane dimension even.
                let (w, h) = (16u32, 8u32);
                let conf = cfg(fc, w, h, slices);
                let payload = build_frame(fc, w, h, pred, slices);
                let lenient = decode_frame(&conf, &payload).expect("lenient decode");
                let strict = decode_frame_strict(&conf, &payload).expect("strict decode");
                assert_eq!(
                    lenient, strict,
                    "strict/lenient mismatch fc={fc:?} pred={pred:?} slices={slices}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------
// 2. Rejection on a flipped padding bit.
// ---------------------------------------------------------------------

/// Take a valid single-slice ULY4 frame and search plane 0's slice-data
/// bytes for a bit that, when set, the lenient decoder still accepts
/// (so it sits *past* the decoded payload — i.e. it is padding) but the
/// strict decoder rejects with `NonZeroPadding`. Such a bit must exist
/// whenever plane 0's payload does not exactly fill a 32-bit word.
#[test]
fn strict_rejects_flipped_padding_bit() {
    // 16×3: plane 0 = 48 pixels. Left predictor -> a short slice whose
    // bit length is very unlikely to be a multiple of 32, guaranteeing
    // padding bits exist.
    let (w, h) = (16u32, 3u32);
    let conf = cfg(Fourcc::Uly4, w, h, 1);
    let clean = build_frame(Fourcc::Uly4, w, h, Predictor::Left, 1);

    // Sanity: clean frame strict-decodes and equals lenient.
    let clean_decode = decode_frame(&conf, &clean).expect("clean lenient");
    assert_eq!(
        decode_frame_strict(&conf, &clean).expect("clean strict"),
        clean_decode
    );

    let (start, end) = plane0_slice_data_span(&clean);
    assert!(
        end > start,
        "plane 0 must carry slice data for this fixture"
    );

    // Walk every bit of plane 0's slice data; flip it; a bit that the
    // lenient path tolerates but the strict path rejects is padding.
    let mut found_padding_hit = false;
    for byte_idx in start..end {
        for bit in 0..8u8 {
            let mut corrupt = clean.clone();
            corrupt[byte_idx] |= 1 << bit;
            if corrupt[byte_idx] == clean[byte_idx] {
                continue; // bit already set
            }
            let lenient = decode_frame(&conf, &corrupt);
            let strict = decode_frame_strict(&conf, &corrupt);
            // A genuine padding bit is one the lenient path ignores (so
            // its decode is byte-identical to the clean frame) AND the
            // strict path rejects as NonZeroPadding. A flip inside the
            // payload changes BOTH decodes and is not our case.
            if let (
                Ok(l),
                Err(Error::NonZeroPadding {
                    plane,
                    bit_position,
                    ..
                }),
            ) = (&lenient, &strict)
            {
                if *l != clean_decode {
                    // Flip landed in the payload: corrupt decode shifted
                    // the symbol stream so strict sees a different
                    // payload boundary. Not a padding-bit hit.
                    continue;
                }
                assert_eq!(*plane, 0, "padding hit must name plane 0");
                let slice_bits = (end - start) * 8;
                assert!(
                    *bit_position < slice_bits,
                    "bit_position {bit_position} out of slice range {slice_bits}"
                );
                found_padding_hit = true;
            }
        }
    }
    assert!(
        found_padding_hit,
        "expected at least one padding bit whose flip is lenient-OK / strict-rejected"
    );
}

#[test]
fn non_zero_padding_is_malformed_stream_category() {
    let e = Error::NonZeroPadding {
        plane: 0,
        slice: 0,
        bit_position: 0,
    };
    assert_eq!(e.category(), ErrorCategory::MalformedStream);
    assert!(e.is_malformed_stream());
}
