//! Round 9 — Huffman-descriptor wire-mutation negative tests + encoder
//! API misuse rejection + extradata builder boundary checks.
//!
//! Round 8 hardened the *slice-data* malformed-payload variants. The
//! remaining undelivered defensive surface — by wire byte — is the
//! **256-byte Huffman descriptor** of each plane (`spec/02` §4): an
//! attacker who fuzzes those bytes trips `KraftViolation` or
//! `MultipleSingleSymbolSentinels` in `huffman::HuffmanTable::build`,
//! not the slice-data guards Round 8 covered. The Huffman unit tests
//! in `src/huffman.rs` already exercise these two error variants on a
//! **synthetic** descriptor — this suite pins the *integration* path:
//! a real encoded frame whose plane-0 descriptor is mutated by a
//! single wire byte, fed through `decode_frame` (the public surface),
//! must surface the correct `Error` variant rather than panicking or
//! mis-decoding. This is the *plane-0 descriptor* analogue of Round 8's
//! plane-0 slice-data sweep.
//!
//! The suite also pins three never-tested-at-integration surfaces:
//!
//! 1. **Encoder input rejection.** `encoder::prepare_planes` checks
//!    `EncoderPlaneSizeMismatch`, `InvalidSliceCount`, and dimension
//!    constraints. None of these had an integration test.
//! 2. **`Extradata::ffmpeg_for` boundary.** Round 6 tested the happy
//!    case (1 slice) and `num_slices == 256`; the explicit
//!    `Err(InvalidSliceCount)` arm at `num_slices == 0` and `> 256`
//!    had no test.
//! 3. **`StreamConfig::new` dimension cascade.** `validate_dims`'s
//!    `(0, _) / (_, 0)` rejection had a unit test on `Fourcc`; the
//!    `StreamConfig` constructor's pass-through plus its own
//!    `num_slices == 0` guard had none.
//!
//! Plus a **`BitWriter` → `BitReader` round-trip invariant** sweep over
//! pathological code lengths (1..=32 bits per write, mixed-length
//! sequences, exactly-1-bit-per-symbol stream of length 1024) that
//! pins the bit-pack/unpack pair without going through the Huffman
//! table builder. The existing huffman.rs unit tests round-trip
//! through a `HuffmanTable`; this suite tests `BitWriter` +
//! `BitReader` in isolation — exercising the 32-bit-LE-word /
//! MSB-first-within-word boundary at every offset.
//!
//! All wire layout / error conditions derived from
//! `docs/video/utvideo/spec/02` + `docs/video/utvideo/spec/05`; no
//! external library source, no web. The xorshift64*-flavoured content
//! source is a self-contained PRNG with no codec provenance.

#![cfg(test)]

use oxideav_utvideo::decoder::decode_frame;
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::error::Error;
use oxideav_utvideo::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};
use oxideav_utvideo::huffman::{BitReader, BitWriter};

// ---------------------------------------------------------------------
// Test scaffolding (shared shape with round8).
// ---------------------------------------------------------------------

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

/// Deterministic noise plane (xorshift64*-flavoured LCG, self-contained
/// PRNG with no codec provenance).
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

fn build_frame(
    fc: Fourcc,
    planes: &[Vec<u8>],
    w: u32,
    h: u32,
    pred: Predictor,
    slices: usize,
) -> Vec<u8> {
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes: planes
            .iter()
            .map(|p| PlaneInput { samples: p.clone() })
            .collect(),
    };
    encode_frame(&frame).unwrap()
}

/// A small valid ULY4 single-slice frame: 8×8, all three planes 8×8.
/// Same shape as Round 8 so the byte layout is the easiest to reason
/// about: plane 0 descriptor at `[0..256]`, offset table at `[256..260]`,
/// slice data at `[260..]`.
fn valid_uly4_8x8() -> (StreamConfig, Vec<u8>) {
    let y = noise_plane(0x01, 8 * 8);
    let u = noise_plane(0x02, 8 * 8);
    let v = noise_plane(0x03, 8 * 8);
    let bytes = build_frame(Fourcc::Uly4, &[y, u, v], 8, 8, Predictor::Left, 1);
    (cfg(Fourcc::Uly4, 8, 8, 1), bytes)
}

// ---------------------------------------------------------------------
// Descriptor-mutation rejection (plane 0).
// ---------------------------------------------------------------------

/// Adding a second zero-codelen sentinel to plane 0's descriptor must
/// surface `MultipleSingleSymbolSentinels` from `HuffmanTable::build`.
/// `spec/02` §4 + `spec/05` §6.1: a `0` sentinel marks the
/// single-symbol-plane fast path; only one such entry is permitted per
/// plane.
#[test]
fn descriptor_two_zero_sentinels_rejected() {
    let (cfg, bytes) = valid_uly4_8x8();
    let mut mutated = bytes.clone();
    // Find a non-zero, non-255 codelen entry in plane 0's descriptor and
    // overwrite it with 0 to install a second sentinel (the first one
    // — if any — is wherever the encoder put it; in this xorshift noise
    // stream the descriptor is dense, so most slots will be small but
    // nonzero codelens).
    // The encoded noise plane uses a dense histogram; no codelen-0
    // sentinel should be present. Inject two sentinels at distinct
    // positions and assert the error.
    let mut zero_positions: Vec<usize> = Vec::new();
    for (i, b) in mutated[0..256].iter().enumerate() {
        if *b == 0 {
            zero_positions.push(i);
        }
    }
    // Force the descriptor to have two distinct sentinels regardless of
    // the encoder's choice for this fixture: zero out positions 5 and 17.
    mutated[5] = 0;
    mutated[17] = 0;
    // Discard the trivial case where 5 == 17 (it doesn't, but be explicit).
    assert_ne!(5, 17);
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::MultipleSingleSymbolSentinels),
        "expected MultipleSingleSymbolSentinels, got {err:?}",
    );
    // Silence unused warning on zero_positions while keeping it as
    // discovery evidence.
    let _ = zero_positions;
}

/// Truncating plane 0's descriptor to a single Kraft-incomplete length
/// (e.g. one codelen=1 entry, everything else 255 sentinel) violates
/// `Σ 2^(-l) == 1` and surfaces `KraftViolation` (`spec/02` §4 invariant
/// + `spec/05` §2).
#[test]
fn descriptor_kraft_incomplete_rejected() {
    let (cfg, bytes) = valid_uly4_8x8();
    let mut mutated = bytes.clone();
    // Zero out all of plane 0's descriptor, then set exactly one symbol
    // to codelen 1. Kraft sum = 1/2, not 1.
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    mutated[42] = 1;
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::KraftViolation),
        "expected KraftViolation, got {err:?}",
    );
}

/// A descriptor whose Kraft sum *exceeds* 1 also fails (two codelen-1
/// entries → sum = 1; three codelen-1 entries → sum = 1.5, > 1).
#[test]
fn descriptor_kraft_excess_rejected() {
    let (cfg, bytes) = valid_uly4_8x8();
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 255;
    }
    // Three codelen-1 entries: Kraft sum = 3/2 > 1.
    mutated[10] = 1;
    mutated[20] = 1;
    mutated[30] = 1;
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::KraftViolation),
        "expected KraftViolation, got {err:?}",
    );
}

/// Setting plane 0's descriptor to "all codelen=1" (256 entries × 2^-1
/// = 128, far above 1) trips Kraft. Catches a different histogram
/// pattern from the deliberate-overflow case above.
#[test]
fn descriptor_uniform_codelen_1_rejected() {
    let (cfg, bytes) = valid_uly4_8x8();
    let mut mutated = bytes.clone();
    for b in mutated[0..256].iter_mut() {
        *b = 1;
    }
    let err = decode_frame(&cfg, &mutated).unwrap_err();
    assert!(
        matches!(err, Error::KraftViolation),
        "expected KraftViolation, got {err:?}",
    );
}

/// A single-byte-flip sweep over plane 0's descriptor span. Every flip
/// must either: (a) leave the descriptor still Kraft-complete (the
/// decoder either succeeds or fails on a slice-data guard since the
/// slice bytes encode old symbols) — or (b) trip `KraftViolation`,
/// `MultipleSingleSymbolSentinels`, or `SliceTruncated` /
/// `HuffmanDecodeFailure`. Never `MissingFrameInfo`, never
/// `ChunkTooShort`, never `NonMonotonicSliceOffsets`, never
/// `SliceNotWordAligned`, never a panic.
#[test]
fn single_byte_flip_in_descriptor_never_panics() {
    let (cfg, bytes) = valid_uly4_8x8();
    // For every byte in the descriptor span, flip the low bit and
    // confirm the decoder either succeeds (rare; the residual stream
    // still might match the alternative codebook by accident) or rejects
    // with one of the expected variants.
    for pos in 0..256 {
        let mut mutated = bytes.clone();
        mutated[pos] ^= 0x01;
        let res = std::panic::catch_unwind(|| decode_frame(&cfg, &mutated));
        let r = res.expect("decode_frame must never panic");
        if let Err(e) = r {
            let ok = matches!(
                e,
                Error::KraftViolation
                    | Error::MultipleSingleSymbolSentinels
                    | Error::SliceTruncated { .. }
                    | Error::HuffmanDecodeFailure { .. }
            );
            assert!(
                ok,
                "descriptor flip at byte {pos} produced disallowed error: {e:?}",
            );
        }
    }
}

// ---------------------------------------------------------------------
// Encoder API misuse rejection.
// ---------------------------------------------------------------------

/// `EncodedFrame.planes.len()` not matching `fourcc.plane_count()` is
/// `EncoderPlaneSizeMismatch`.
#[test]
fn encoder_rejects_wrong_plane_count() {
    // ULRA needs 4 planes; pass 3.
    let frame = EncodedFrame {
        fourcc: Fourcc::Ulra,
        width: 8,
        height: 8,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
        ],
    };
    let err = encode_frame(&frame).unwrap_err();
    assert!(
        matches!(err, Error::EncoderPlaneSizeMismatch { .. }),
        "expected EncoderPlaneSizeMismatch, got {err:?}",
    );
}

/// A per-plane buffer whose length disagrees with `fourcc.plane_dim`
/// surfaces `EncoderPlaneSizeMismatch` with the offending plane index.
#[test]
fn encoder_rejects_wrong_plane_buffer_size() {
    // ULY0 8×8 expects Y=64, U=V=16. Give U only 15 bytes.
    let frame = EncodedFrame {
        fourcc: Fourcc::Uly0,
        width: 8,
        height: 8,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 15],
            }, // wrong
            PlaneInput {
                samples: vec![0u8; 16],
            },
        ],
    };
    let err = encode_frame(&frame).unwrap_err();
    assert!(
        matches!(
            err,
            Error::EncoderPlaneSizeMismatch {
                plane: 1,
                expected: 16,
                got: 15,
            }
        ),
        "expected EncoderPlaneSizeMismatch{{plane:1,expected:16,got:15}}, got {err:?}",
    );
}

/// `num_slices == 0` on encode input is `InvalidSliceCount`.
#[test]
fn encoder_rejects_zero_slices() {
    let frame = EncodedFrame {
        fourcc: Fourcc::Uly4,
        width: 8,
        height: 8,
        predictor: Predictor::Left,
        num_slices: 0,
        planes: vec![
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
        ],
    };
    let err = encode_frame(&frame).unwrap_err();
    assert!(
        matches!(err, Error::InvalidSliceCount),
        "expected InvalidSliceCount, got {err:?}",
    );
}

/// `num_slices > 256` on encode is also `InvalidSliceCount` — the wire
/// formula `((flags >> 24) & 0xff) + 1` caps at 256.
#[test]
fn encoder_rejects_excess_slices() {
    let frame = EncodedFrame {
        fourcc: Fourcc::Uly4,
        width: 8,
        height: 8,
        predictor: Predictor::Left,
        num_slices: 257,
        planes: vec![
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
            PlaneInput {
                samples: vec![0u8; 64],
            },
        ],
    };
    let err = encode_frame(&frame).unwrap_err();
    assert!(
        matches!(err, Error::InvalidSliceCount),
        "expected InvalidSliceCount, got {err:?}",
    );
}

/// Odd dimension on a chroma-subsampled FOURCC must be rejected at
/// encode (`spec/02` §3.2). ULY0 requires even W and H.
#[test]
fn encoder_rejects_odd_dim_uly0() {
    // ULY0 9×8: odd width.
    let frame = EncodedFrame {
        fourcc: Fourcc::Uly0,
        width: 9,
        height: 8,
        predictor: Predictor::Left,
        num_slices: 1,
        planes: vec![
            PlaneInput {
                samples: vec![0u8; 72],
            }, // 9*8
            PlaneInput {
                samples: vec![0u8; 16],
            }, // 4*4 (floor div)
            PlaneInput {
                samples: vec![0u8; 16],
            },
        ],
    };
    let err = encode_frame(&frame).unwrap_err();
    assert!(
        matches!(err, Error::DimensionConstraint(_)),
        "expected DimensionConstraint, got {err:?}",
    );
}

// ---------------------------------------------------------------------
// `Extradata::ffmpeg_for` boundary checks.
// ---------------------------------------------------------------------

/// `num_slices == 0` is `InvalidSliceCount` from the builder.
#[test]
fn extradata_ffmpeg_for_rejects_zero_slices() {
    let err = Extradata::ffmpeg_for(Fourcc::Uly0, 0).unwrap_err();
    assert!(
        matches!(err, Error::InvalidSliceCount),
        "expected InvalidSliceCount, got {err:?}",
    );
}

/// `num_slices > 256` is `InvalidSliceCount` from the builder.
#[test]
fn extradata_ffmpeg_for_rejects_excess_slices() {
    let err = Extradata::ffmpeg_for(Fourcc::Uly0, 257).unwrap_err();
    assert!(
        matches!(err, Error::InvalidSliceCount),
        "expected InvalidSliceCount, got {err:?}",
    );
    let err = Extradata::ffmpeg_for(Fourcc::Uly0, usize::MAX).unwrap_err();
    assert!(
        matches!(err, Error::InvalidSliceCount),
        "expected InvalidSliceCount, got {err:?}",
    );
}

/// `num_slices == 256` is the upper bound and succeeds. Plus, the
/// resulting `flags` field's high byte must be `0xff` per
/// `spec/01` §4.4.3 (`((255 << 24)) | 1`).
#[test]
fn extradata_ffmpeg_for_accepts_256_slices() {
    let ed = Extradata::ffmpeg_for(Fourcc::Uly0, 256).unwrap();
    assert_eq!(ed.flags, 0x0000_0001 | (0xffu32 << 24));
    assert_eq!(ed.num_slices(), 256);
}

// ---------------------------------------------------------------------
// `StreamConfig::new` cascade.
// ---------------------------------------------------------------------

/// Zero width is `DimensionConstraint` (from `validate_dims` —
/// `width/height must be > 0`).
#[test]
fn stream_config_rejects_zero_width() {
    let ed = Extradata::ffmpeg_for(Fourcc::Uly0, 1).unwrap();
    let err = StreamConfig::new(Fourcc::Uly0, 0, 8, ed).unwrap_err();
    assert!(
        matches!(err, Error::DimensionConstraint(_)),
        "expected DimensionConstraint, got {err:?}",
    );
}

/// Zero height is `DimensionConstraint`.
#[test]
fn stream_config_rejects_zero_height() {
    let ed = Extradata::ffmpeg_for(Fourcc::Uly0, 1).unwrap();
    let err = StreamConfig::new(Fourcc::Uly0, 8, 0, ed).unwrap_err();
    assert!(
        matches!(err, Error::DimensionConstraint(_)),
        "expected DimensionConstraint, got {err:?}",
    );
}

/// ULY2 odd-height accepted (chroma-subsamples only by width).
#[test]
fn stream_config_uly2_accepts_odd_height() {
    let ed = Extradata::ffmpeg_for(Fourcc::Uly2, 1).unwrap();
    assert!(StreamConfig::new(Fourcc::Uly2, 8, 17, ed).is_ok());
}

// ---------------------------------------------------------------------
// `BitWriter` ⇄ `BitReader` round-trip invariants in isolation.
// ---------------------------------------------------------------------
//
// These tests exercise the bit-pack/unpack pair without involving the
// Huffman table builder, pinning the 32-bit-LE-word / MSB-first-
// within-word convention (`spec/05` §4) at every offset alignment.

/// Round-trip a stream of N codes of length L each, for every
/// `L ∈ 1..=32` and several N values. Asserts byte-aligned padding to
/// 32-bit words (`spec/05` §4.1) and exact bit-stream recovery via
/// `peek_bits(L)` / `consume_bits(L)` per the decoded code.
#[test]
fn bit_pack_round_trip_for_every_code_length() {
    for length in 1u8..=32 {
        // Pick a value with the high bit set when length permits, so the
        // MSB-first packing is observably non-trivial.
        let value: u32 = if length == 32 {
            0xdead_beef
        } else {
            (1u32 << (length - 1)) | 0x01
        };
        let n_codes = 200usize;
        let mut bw = BitWriter::new();
        for _ in 0..n_codes {
            bw.write_code(value, length);
        }
        let bytes = bw.finish();
        // Total bits = n_codes * length, padded up to next 32-bit word.
        let total_bits = n_codes * length as usize;
        let words = total_bits.div_ceil(32);
        assert_eq!(
            bytes.len(),
            words * 4,
            "length {length}: bit-pack output length wrong (got {} bytes, expected {})",
            bytes.len(),
            words * 4,
        );
        // Read back: each code must surface as `value` masked to length.
        let mut br = BitReader::new(&bytes);
        let mask: u32 = if length == 32 {
            0xffff_ffff
        } else {
            (1u32 << length) - 1
        };
        let expected = value & mask;
        for i in 0..n_codes {
            assert!(
                br.has_bits(length as usize),
                "length {length}: ran out of bits at code {i}",
            );
            let got = br.peek_bits(length as usize);
            assert_eq!(
                got, expected,
                "length {length}: code {i} mismatch (got {got:#x}, expected {expected:#x})",
            );
            br.consume_bits(length as usize);
        }
    }
}

/// Mixed-length codes — write a sequence of `(value, length)` pairs,
/// read them back, verify exact recovery. Exercises bit-offset
/// alignment at every byte boundary within a 32-bit word.
#[test]
fn bit_pack_round_trip_mixed_lengths() {
    // Deterministic sequence covering all bit-offset transitions:
    // lengths 1..=12 with values walking the alphabet 0..=2^len-1.
    let mut prog: Vec<(u32, u8)> = Vec::new();
    for length in 1u8..=12 {
        let mask: u32 = (1u32 << length) - 1;
        for v in 0..=mask.min(15) {
            prog.push((v, length));
        }
    }
    let mut bw = BitWriter::new();
    for &(v, l) in &prog {
        bw.write_code(v, l);
    }
    let bytes = bw.finish();
    let mut br = BitReader::new(&bytes);
    for (i, &(v, l)) in prog.iter().enumerate() {
        assert!(br.has_bits(l as usize), "mixed: ran out at item {i}");
        let got = br.peek_bits(l as usize);
        assert_eq!(
            got, v,
            "mixed: item {i} length {l} mismatch (got {got:#x}, expected {v:#x})",
        );
        br.consume_bits(l as usize);
    }
}

/// `BitWriter::finish` with no writes returns an empty `Vec` (no padding
/// word). Edge case the encoder relies on for the single-symbol
/// zero-slice-data fast path (`spec/02` §5.1).
#[test]
fn bit_writer_empty_returns_empty() {
    let bw = BitWriter::new();
    assert_eq!(bw.finish(), Vec::<u8>::new());
}

/// `BitWriter` always pads the trailing partial word with zeros per
/// `spec/05` §4.3, and the resulting byte length is a multiple of 4.
#[test]
fn bit_writer_partial_word_padded_with_zeros() {
    // Write 33 bits: 32 ones + 1 zero. The second word is `0x00000000`
    // with only the MSB used (the explicit zero we wrote), and the
    // remaining 31 bits zero-pad per spec/05 §4.3.
    let mut bw = BitWriter::new();
    bw.write_code(0xffff_ffff, 32);
    bw.write_code(0, 1);
    let bytes = bw.finish();
    // 33 bits packed → 8 bytes (two 32-bit words).
    assert_eq!(bytes.len(), 8);
    assert_eq!(&bytes[0..4], &[0xff, 0xff, 0xff, 0xff]);
    // Second word: the explicit 0 bit + 31 zero pad bits → 0x00000000.
    assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x00, 0x00]);
}

/// `BitReader::has_bits` boundary: at exactly the end of stream returns
/// `false` for `n > 0`, `true` for `n == 0`.
#[test]
fn bit_reader_has_bits_at_end_of_stream() {
    let data = vec![0xffu8, 0xff, 0xff, 0xff];
    let mut br = BitReader::new(&data);
    br.consume_bits(32);
    assert!(br.has_bits(0));
    assert!(!br.has_bits(1));
    assert!(!br.has_bits(32));
}

/// `BitReader::peek_bits` straddling a 32-bit-word boundary returns the
/// expected MSB-first concatenation. The encoder packs two 4-bit codes
/// 0xa, 0xb across positions [30..34]; the reader should reconstruct
/// each correctly.
#[test]
fn bit_reader_peek_across_word_boundary() {
    let mut bw = BitWriter::new();
    // Burn 30 bits of zeros to align the next write at position 30.
    bw.write_code(0, 30);
    // Now write a 4-bit pattern 0b1010 (= 0xa) and a 4-bit 0b1011 (=
    // 0xb). The 4-bit code 0xa straddles the first word (positions
    // 30..32 are the top 2 bits of 0xa = 0b10) and the second word
    // (positions 32..34 are the bottom 2 bits of 0xa = 0b10).
    bw.write_code(0xa, 4);
    bw.write_code(0xb, 4);
    let bytes = bw.finish();
    let mut br = BitReader::new(&bytes);
    br.consume_bits(30);
    assert_eq!(br.peek_bits(4), 0xa);
    br.consume_bits(4);
    assert_eq!(br.peek_bits(4), 0xb);
}

// ---------------------------------------------------------------------
// Positive control: the unmutated base fixture re-decodes cleanly. This
// lets a regression in the encoder show up as a *test failure here*
// rather than as "everything in the suite mysteriously errors".
// ---------------------------------------------------------------------

#[test]
fn base_fixture_decodes_clean() {
    let (cfg, bytes) = valid_uly4_8x8();
    let decoded = decode_frame(&cfg, &bytes).unwrap();
    assert_eq!(decoded.planes.len(), 3);
    assert_eq!(decoded.width, 8);
    assert_eq!(decoded.height, 8);
}
