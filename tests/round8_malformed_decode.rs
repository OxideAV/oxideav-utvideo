//! Round 8 — malformed-payload decode robustness.
//!
//! Every prior round (1..7) exercises the *happy* decode path: a frame
//! the in-crate encoder produced, fed straight back through
//! [`decode_frame`]. The decoder's defensive surface — the `Err(...)`
//! arms in `decoder::parse_payload` and `huffman::decode_slice` — is
//! *raised* by `src/`, and the extradata-level rejections
//! (`UnknownFourcc`, `InvalidFrameInfoSize`, `HuffmanBitClear`,
//! `InterlacedNotSupported`, `ExtradataTruncated`, `DimensionConstraint`)
//! plus the Huffman-build rejections (`KraftViolation`,
//! `MultipleSingleSymbolSentinels`) carry unit tests in `fourcc.rs` /
//! `huffman.rs`. But the **per-frame payload** error variants — the
//! ones a real demuxer would hit on a corrupt `00dc` chunk — had only
//! one smoke test (`round4_parallel_decode.rs` truncates 8 bytes and
//! asserts `is_err()`), and none pinned the *specific* `Error` variant.
//!
//! This suite closes that gap. For each malformed-payload condition the
//! spec names, we start from a valid encoder output and surgically
//! mutate the wire bytes to trip exactly one decoder guard, then assert
//! the precise [`Error`] variant. This is the negative half of the
//! decode contract: a wire-format conformance check that a malformed
//! stream is *rejected with a diagnosable error*, never silently
//! mis-decoded and never a panic.
//!
//! Wire layout pinned by `spec/02` §7 (per-plane: 256-byte Huffman
//! code-length descriptor, then `num_slices × 4` slice-end-offset
//! table as u32 LE, then the slice data) and `spec/05` §4.1 (slice
//! byte length is a multiple of 4). A single-slice frame's plane 0
//! therefore begins at byte 0 with the 256-byte descriptor, the
//! 4-byte offset table at byte 256, and slice data at byte 260.
//!
//! All behaviour derived from `docs/video/utvideo/spec/02` +
//! `docs/video/utvideo/spec/05`; no external library source, no web.

#![cfg(test)]

use oxideav_utvideo::decoder::{
    decode_frame, decode_frame_parallel, decode_frame_serial, PARALLEL_PIXEL_THRESHOLD,
};
use oxideav_utvideo::encoder::{encode_frame, EncodedFrame, PlaneInput};
use oxideav_utvideo::error::Error;
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

/// Deterministic noise plane (xorshift-flavoured LCG, identical to the
/// round-3/4 helpers). Self-contained PRNG, no codec provenance.
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

/// Byte span `[start, end)` of plane 0's slice data within a
/// single-slice payload. Layout per `spec/02` §7: descriptor[0..256],
/// one u32-LE slice-end-offset at [256..260], slice data at
/// [260..260+off0]. `off0` is plane 0's cumulative slice-data byte
/// count.
fn plane0_slice_data_span(bytes: &[u8]) -> (usize, usize) {
    let off0 = u32::from_le_bytes(bytes[256..260].try_into().unwrap()) as usize;
    (260, 260 + off0)
}

/// A small valid ULY4 single-slice frame: 8×8, all three planes 8×8.
/// 4:4:4 means every plane is the full frame size, so the byte layout
/// is the simplest to reason about (no chroma-subsampling arithmetic).
fn valid_uly4_8x8() -> (StreamConfig, Vec<u8>) {
    let y = noise_plane(0x01, 8 * 8);
    let u = noise_plane(0x02, 8 * 8);
    let v = noise_plane(0x03, 8 * 8);
    let bytes = build_frame(Fourcc::Uly4, &[y, u, v], 8, 8, Predictor::Left, 1);
    (cfg(Fourcc::Uly4, 8, 8, 1), bytes)
}

// ---------------------------------------------------------------------
// Length / structure errors (decoder::parse_payload)
// ---------------------------------------------------------------------

/// A payload shorter than the trailing 4-byte frame-info dword is
/// `MissingFrameInfo` (`spec/02` §6 — the frame-info dword always
/// terminates the payload).
#[test]
fn missing_frame_info_on_too_short_payload() {
    let cfg = cfg(Fourcc::Uly4, 8, 8, 1);
    for len in 0..4usize {
        let payload = vec![0u8; len];
        let res = decode_frame(&cfg, &payload);
        assert!(
            matches!(res, Err(Error::MissingFrameInfo)),
            "len {len} must be MissingFrameInfo, got {res:?}"
        );
    }
}

/// A payload with the frame-info dword present but no room for the
/// first plane's 256-byte Huffman descriptor is `ChunkTooShort`
/// (`spec/02` §7).
#[test]
fn chunk_too_short_for_descriptor() {
    let cfg = cfg(Fourcc::Uly4, 8, 8, 1);
    // 4 bytes total: exactly the frame-info dword, zero bytes for the
    // descriptor that must precede it.
    let payload = vec![0u8; 4];
    let res = decode_frame(&cfg, &payload);
    assert!(
        matches!(
            res,
            Err(Error::ChunkTooShort {
                offset: 0,
                needed: 256,
                ..
            })
        ),
        "got {res:?}"
    );
    // 100 bytes + 4-byte frame-info: still < 256 for the descriptor.
    let payload = vec![0u8; 104];
    let res = decode_frame(&cfg, &payload);
    assert!(
        matches!(
            res,
            Err(Error::ChunkTooShort {
                offset: 0,
                needed: 256,
                ..
            })
        ),
        "got {res:?}"
    );
}

/// Descriptor fits but the `num_slices × 4` slice-end-offset table
/// runs off the end is `ChunkTooShort` (`spec/02` §7). Use a 4-slice
/// frame so the table is 16 bytes; truncate just after the descriptor.
#[test]
fn chunk_too_short_for_offset_table() {
    // 4-slice frame, but a payload that is descriptor (256) + 4 bytes
    // (frame-info) + 8 bytes (room for only 2 of the 4 offsets).
    let cfg = cfg(Fourcc::Uly4, 8, 8, 4);
    let mut payload = vec![0u8; 256 + 8 + 4];
    // Make the descriptor a valid single-symbol plane so the *only*
    // failure is the offset-table length, not a Kraft violation: a
    // descriptor of all-zero code lengths means "every symbol length 0",
    // but the parser hits the offset-table-length guard before building
    // the table, so the descriptor content is irrelevant here. Leave it
    // zero.
    let _ = &mut payload;
    let res = decode_frame(&cfg, &payload);
    assert!(
        matches!(
            res,
            Err(Error::ChunkTooShort {
                offset: 256,
                needed: 16,
                ..
            })
        ),
        "got {res:?}"
    );
}

/// A slice-end-offset that points past the bytes actually present is
/// `ChunkTooShort` for the slice-data span (`spec/02` §7). Take a valid
/// frame and inflate plane 0's single slice-end-offset by a huge amount.
#[test]
fn chunk_too_short_for_slice_data() {
    let (cfg, mut bytes) = valid_uly4_8x8();
    // Single slice: the offset table is one u32 LE at byte 256.
    // Set it to a value far larger than any remaining bytes.
    let off = 256usize;
    bytes[off..off + 4].copy_from_slice(&0x0010_0000u32.to_le_bytes()); // 1 MiB
    let res = decode_frame(&cfg, &bytes);
    assert!(
        matches!(res, Err(Error::ChunkTooShort { .. })),
        "got {res:?}"
    );
}

// ---------------------------------------------------------------------
// Slice-end-offset table invariants
// ---------------------------------------------------------------------

/// Slice-end offsets must be monotonically non-decreasing (`spec/02`
/// §5). A 2-slice frame whose second offset is *smaller* than the
/// first is `NonMonotonicSliceOffsets`.
#[test]
fn non_monotonic_slice_offsets() {
    // 2-slice ULY4 8×8. Each plane: descriptor[256] + offsets[8] + data.
    let y = noise_plane(0x11, 8 * 8);
    let u = noise_plane(0x12, 8 * 8);
    let v = noise_plane(0x13, 8 * 8);
    let mut bytes = build_frame(Fourcc::Uly4, &[y, u, v], 8, 8, Predictor::Left, 2);
    let cfg = cfg(Fourcc::Uly4, 8, 8, 2);
    // Plane 0's offset table is two u32 LE at byte 256.
    // Read the real (monotonic) values, then swap them so off[1] < off[0]
    // while both stay word-aligned (the real values already are).
    let off0 = u32::from_le_bytes(bytes[256..260].try_into().unwrap());
    let off1 = u32::from_le_bytes(bytes[260..264].try_into().unwrap());
    // Only meaningful if the slices actually differ in size; if they
    // are equal the swap is a no-op and we instead force off[1] = 0.
    let (new0, new1) = if off0 != off1 {
        (off1, off0)
    } else {
        (off0, 0u32)
    };
    // Ensure non-monotonic: new1 must be strictly less than new0.
    assert!(new1 < new0, "test setup: need a strictly decreasing pair");
    bytes[256..260].copy_from_slice(&new0.to_le_bytes());
    bytes[260..264].copy_from_slice(&new1.to_le_bytes());
    let res = decode_frame(&cfg, &bytes);
    assert!(
        matches!(res, Err(Error::NonMonotonicSliceOffsets)),
        "got {res:?}"
    );
}

/// A slice byte-length that is not a multiple of 4 is rejected
/// (`spec/05` §4.1: slice data is 32-bit-word aligned). Bump plane 0's
/// single slice-end-offset by 1 (the real value is a multiple of 4,
/// so +1 is guaranteed non-aligned) and keep it within the available
/// span by shrinking nothing else — the alignment check fires before
/// the span check.
#[test]
fn slice_not_word_aligned() {
    let (cfg, mut bytes) = valid_uly4_8x8();
    let off = 256usize;
    let real = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    assert_eq!(real % 4, 0, "encoder must emit word-aligned slice lengths");
    // real + 1 is non-aligned. The monotonicity + alignment loop runs
    // before the slice-data span guard, so this trips alignment.
    bytes[off..off + 4].copy_from_slice(&(real + 1).to_le_bytes());
    let res = decode_frame(&cfg, &bytes);
    assert!(
        matches!(res, Err(Error::SliceNotWordAligned(n)) if n == (real + 1) as usize),
        "got {res:?}"
    );
}

// ---------------------------------------------------------------------
// Huffman / slice-data decode errors (huffman::decode_slice)
// ---------------------------------------------------------------------

/// Corrupt entropy data is surfaced as a diagnosable error, never a
/// silent mis-decode and never a panic. A *single* flipped bit in a
/// Huffman stream often still produces a syntactically valid symbol
/// count (the decoder resynchronises), so that is not the invariant we
/// can pin. The deterministic corruption is zeroing the whole
/// slice-data span: a multi-length canonical table assigns the
/// **all-zero prefix to the longest code** (`spec/05` §2.2 — codes run
/// longest = `0…0` to shortest = `1…1`), so an all-zero bit stream
/// emits the max-length symbol every pixel. The slice's byte budget was
/// sized for the real (mixed-length) stream, so emitting the longest
/// code 256 times exhausts the bits before all pixels are produced →
/// `SliceTruncated` (or `HuffmanDecodeFailure` if the all-zero prefix
/// is unmatched). Either is a valid rejection; the load-bearing
/// invariant is that it is *rejected*, with no panic.
#[test]
fn zeroed_slice_data_is_rejected() {
    // A high-entropy plane forces a multi-length Huffman table (no
    // single-symbol fast path) whose codes span several lengths.
    let y = noise_plane(0xA5A5, 16 * 16);
    let u = noise_plane(0x5A5A, 16 * 16);
    let v = noise_plane(0x3C3C, 16 * 16);
    let bytes = build_frame(Fourcc::Uly4, &[y, u, v], 16, 16, Predictor::None, 1);
    let cfg = cfg(Fourcc::Uly4, 16, 16, 1);
    // Plane 0 layout: descriptor[0..256], offset-table[256..260] (one
    // u32 LE for the single slice), slice-data[260..260+off0]. Corrupt
    // ONLY plane 0's slice-data span — corrupting beyond it would hit
    // plane 1's descriptor and trip a Kraft / sentinel error instead.
    let (data_start, data_end) = plane0_slice_data_span(&bytes);
    assert!(
        data_end > data_start,
        "plane 0 must have non-empty slice data"
    );
    let mut corrupt = bytes.clone();
    for b in &mut corrupt[data_start..data_end] {
        *b = 0;
    }
    let res = decode_frame(&cfg, &corrupt);
    assert!(
        matches!(
            res,
            Err(Error::SliceTruncated { .. }) | Err(Error::HuffmanDecodeFailure { .. })
        ),
        "zeroed multi-length slice data must be rejected, got {res:?}"
    );
}

/// A single-byte flip in Huffman slice data must never panic and never
/// produce an error variant outside the slice-data family — it either
/// decodes (to a valid-but-wrong stream, a resync) or is rejected as
/// `SliceTruncated` / `HuffmanDecodeFailure`. This sweeps every byte of
/// a real slice-data span and asserts the no-panic / no-spurious-variant
/// contract across the whole span (the round-4 smoke only checked one
/// truncation).
#[test]
fn single_byte_flip_never_panics_or_mis_typed() {
    let y = noise_plane(0xA5A5, 16 * 16);
    let u = noise_plane(0x5A5A, 16 * 16);
    let v = noise_plane(0x3C3C, 16 * 16);
    let bytes = build_frame(Fourcc::Uly4, &[y, u, v], 16, 16, Predictor::None, 1);
    let cfg = cfg(Fourcc::Uly4, 16, 16, 1);
    let (data_start, data_end) = plane0_slice_data_span(&bytes);
    assert!(data_end > data_start);
    for flip in data_start..data_end {
        let mut corrupt = bytes.clone();
        corrupt[flip] ^= 0xFF;
        match decode_frame(&cfg, &corrupt) {
            Ok(decoded) => {
                // Resync: still a structurally complete frame.
                assert_eq!(decoded.planes.len(), 3);
                assert_eq!(decoded.planes[0].samples.len(), 16 * 16);
            }
            Err(Error::SliceTruncated { .. }) | Err(Error::HuffmanDecodeFailure { .. }) => {}
            other => panic!("byte {flip}: unexpected error variant {other:?}"),
        }
    }
}

/// Truncating a frame mid-slice-data (after a valid header) yields a
/// slice-data span shortfall: `ChunkTooShort` if the declared offset
/// now exceeds the bytes present. This pins the round-4 smoke test to a
/// specific variant and across both the serial and parallel paths.
#[test]
fn truncated_slice_data_span_serial_and_parallel() {
    // Large enough that decode_frame auto-selects the parallel path
    // (320×240 = 76 800 px > PARALLEL_PIXEL_THRESHOLD), 4 slices.
    const _: () = assert!(320 * 240 > PARALLEL_PIXEL_THRESHOLD);
    let y = noise_plane(0xdead, 320 * 240);
    let u = noise_plane(0xbeef, 160 * 120);
    let v = noise_plane(0xface, 160 * 120);
    let cfg = cfg(Fourcc::Uly0, 320, 240, 4);
    let bytes = build_frame(Fourcc::Uly0, &[y, u, v], 320, 240, Predictor::Left, 4);
    // Truncate 8 bytes off the end — into the last plane's slice data,
    // before the frame-info dword. The declared slice-end-offset for the
    // last plane now exceeds the present bytes → ChunkTooShort.
    let mut short = bytes.clone();
    short.truncate(short.len() - 8);
    for label in ["auto", "serial", "parallel"] {
        let res = match label {
            "auto" => decode_frame(&cfg, &short),
            "serial" => decode_frame_serial(&cfg, &short),
            _ => decode_frame_parallel(&cfg, &short),
        };
        assert!(
            matches!(res, Err(Error::ChunkTooShort { .. })),
            "{label}: truncated slice-data span must be ChunkTooShort, got {res:?}"
        );
    }
}

// ---------------------------------------------------------------------
// Trailing-byte / exact-length invariant
// ---------------------------------------------------------------------

/// Extra trailing bytes between the last plane's slice data and the
/// frame-info dword make the running `offset` land short of
/// `frame_info_off`, which the parser rejects as `ChunkTooShort`
/// (`spec/02` §7: the payload is exactly the planes + the 4-byte
/// frame-info; nothing else). Insert 4 junk bytes before the frame-info
/// dword.
#[test]
fn trailing_junk_before_frame_info_rejected() {
    let (cfg, bytes) = valid_uly4_8x8();
    let n = bytes.len();
    // Re-form: [planes..][JUNK 4][frame_info 4]. The parser walks the
    // planes, leaves `offset` at the end of plane data, then expects
    // `offset == frame_info_off`; the 4 junk bytes break that equality.
    let mut padded = Vec::with_capacity(n + 4);
    padded.extend_from_slice(&bytes[..n - 4]); // planes
    padded.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x01]); // junk
    padded.extend_from_slice(&bytes[n - 4..]); // frame_info
    let res = decode_frame(&cfg, &padded);
    assert!(
        matches!(res, Err(Error::ChunkTooShort { .. })),
        "trailing junk before frame-info must be rejected, got {res:?}"
    );
}

// ---------------------------------------------------------------------
// Positive control: the unmutated frame still decodes cleanly.
// ---------------------------------------------------------------------

/// Sanity: the base fixtures the negative tests mutate are themselves
/// valid and round-trip. Guards against a test that "passes" only
/// because the base frame was already broken.
#[test]
fn base_fixtures_decode_clean() {
    let (cfg_a, bytes) = valid_uly4_8x8();
    let decoded = decode_frame(&cfg_a, &bytes).expect("base ULY4 8×8 frame must decode");
    assert_eq!(decoded.fourcc, Fourcc::Uly4);
    assert_eq!(decoded.planes.len(), 3);

    let y = noise_plane(0xdead, 320 * 240);
    let u = noise_plane(0xbeef, 160 * 120);
    let v = noise_plane(0xface, 160 * 120);
    let cfg_b = cfg(Fourcc::Uly0, 320, 240, 4);
    let bytes = build_frame(
        Fourcc::Uly0,
        &[y.clone(), u.clone(), v.clone()],
        320,
        240,
        Predictor::Left,
        4,
    );
    let decoded = decode_frame(&cfg_b, &bytes).expect("base ULY0 320×240/4 frame must decode");
    assert_eq!(decoded.planes[0].samples, y);
    assert_eq!(decoded.planes[1].samples, u);
    assert_eq!(decoded.planes[2].samples, v);
}
