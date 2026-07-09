//! Round 404 — single-symbol plane must carry zero slice-data bytes.
//!
//! `spec/05` §6.1 fixes the single-symbol plane's on-wire shape: a
//! Huffman descriptor with exactly one `code_length = 0` sentinel (all
//! others `255`), an **all-zero** slice-end-offset table, and a
//! slice-data byte count of exactly **0**. The lone symbol consumes no
//! bits and is emitted for every pixel of the plane.
//!
//! A codelen-0 descriptor paired with a non-empty slice-data segment is
//! self-inconsistent: the descriptor says "no bits", the offset table
//! says "N bytes of bits". The reference-authentic streams never carry
//! this shape (a real single-symbol plane always has `slice_end_offsets
//! = (0, …)`), so accepting it would mean silently discarding stray
//! bytes. Both the full decoder and the decode-free inspector now reject
//! it with `Error::SingleSymbolPlaneHasSliceData`, staying in lockstep.
//!
//! This test hand-crafts the malformed shape (the in-crate encoder can
//! no longer produce it — single-symbol planes emit an empty blob) and
//! pins rejection on both surfaces, plus a valid single-symbol frame
//! that both surfaces still accept.

use oxideav_utvideo::{decode_frame, peek_frame, Error, Extradata, Fourcc, StreamConfig};

/// Build a ULY4 W×H, 1-slice `StreamConfig`.
fn cfg_uly4(w: u32, h: u32) -> StreamConfig {
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *b"YV24",
        frame_info_size: 4,
        flags: 0x0000_0001, // Huffman bit set, 1 slice.
    };
    StreamConfig::new(Fourcc::Uly4, w, h, extradata).unwrap()
}

/// A 256-byte descriptor with a single `code_length = 0` sentinel at
/// `sym` and every other byte `255` (`spec/05` §6.1 single-symbol shape).
fn single_symbol_descriptor(sym: u8) -> [u8; 256] {
    let mut d = [255u8; 256];
    d[sym as usize] = 0;
    d
}

/// Assemble a 1-slice ULY4 payload where plane 0 optionally carries
/// `plane0_slice_bytes` of (bogus) slice data behind a single-symbol
/// descriptor. Planes 1 and 2 are always well-formed single-symbol
/// planes with an empty slice-data segment.
fn build_payload(plane0_slice_bytes: usize) -> Vec<u8> {
    assert_eq!(
        plane0_slice_bytes % 4,
        0,
        "slice bytes must be word-aligned"
    );
    let mut out = Vec::new();

    // --- Plane 0 (G→Y): single-symbol descriptor, `plane0_slice_bytes`
    //     of trailing slice data behind a non-zero end offset. ---
    out.extend_from_slice(&single_symbol_descriptor(42));
    out.extend_from_slice(&(plane0_slice_bytes as u32).to_le_bytes()); // slice_end_offsets[0]
    out.extend(std::iter::repeat(0u8).take(plane0_slice_bytes)); // stray slice data

    // --- Planes 1, 2: valid single-symbol, zero slice data. ---
    for sym in [10u8, 20u8] {
        out.extend_from_slice(&single_symbol_descriptor(sym));
        out.extend_from_slice(&0u32.to_le_bytes()); // slice_end_offsets[0] = 0
    }

    // --- Trailing frame-info dword: predictor None (0x0). ---
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

#[test]
fn decode_rejects_single_symbol_plane_with_slice_data() {
    let cfg = cfg_uly4(2, 2);
    let payload = build_payload(4);
    match decode_frame(&cfg, &payload) {
        Err(Error::SingleSymbolPlaneHasSliceData {
            plane,
            slice_data_len,
        }) => {
            assert_eq!(plane, 0);
            assert_eq!(slice_data_len, 4);
        }
        other => panic!("expected SingleSymbolPlaneHasSliceData, got {other:?}"),
    }
}

#[test]
fn inspect_rejects_single_symbol_plane_with_slice_data() {
    let cfg = cfg_uly4(2, 2);
    let payload = build_payload(4);
    match peek_frame(&cfg, &payload) {
        Err(Error::SingleSymbolPlaneHasSliceData {
            plane,
            slice_data_len,
        }) => {
            assert_eq!(plane, 0);
            assert_eq!(slice_data_len, 4);
        }
        other => panic!("expected SingleSymbolPlaneHasSliceData, got {other:?}"),
    }
}

#[test]
fn the_rejection_is_a_malformed_stream_error() {
    let err = Error::SingleSymbolPlaneHasSliceData {
        plane: 0,
        slice_data_len: 4,
    };
    assert!(err.is_malformed_stream());
    assert!(!err.is_api_misuse());
}

#[test]
fn valid_single_symbol_frame_still_decodes_on_both_surfaces() {
    // Zero-length plane-0 slice data (the spec/05 §6.1 shape) is accepted
    // and reconstructs the constant plane; the inspector agrees.
    let cfg = cfg_uly4(2, 2);
    let payload = build_payload(0);

    let decoded = decode_frame(&cfg, &payload).expect("valid single-symbol frame decodes");
    assert_eq!(decoded.planes.len(), 3);
    // Plane 0 emits symbol 42 for every pixel (predictor None => raw).
    assert!(decoded.planes[0].samples.iter().all(|&s| s == 42));

    let layout = peek_frame(&cfg, &payload).expect("valid single-symbol frame inspects");
    assert!(layout.planes[0].is_single_symbol);
    assert_eq!(layout.planes[0].slice_data_total(), 0);
}
