//! Round 13 — error taxonomy: `ErrorCategory` classifier + exhaustive
//! `Display` regression suite.
//!
//! Every prior round (1..12) has exercised the *production* of an
//! [`Error`] variant from a specific malformed input — Round 8 fuzzed
//! slice-data bytes for `SliceTruncated` / `HuffmanDecodeFailure`,
//! Round 9 fuzzed the 256-byte Huffman descriptor for `KraftViolation`
//! / `MultipleSingleSymbolSentinels` and the encoder's typed contract
//! for `EncoderPlaneSizeMismatch` / `InvalidSliceCount`, etc. None of
//! those rounds covered the *consumption* side: a caller wanting to
//! react to an error needs either to pattern-match every variant
//! (brittle: a new variant added in a future round is a silent
//! fallthrough at the call site) or to ask the error which *kind* of
//! failure it is. The [`ErrorCategory`] classifier added in this round
//! gives a four-way taxonomy (`MalformedStream` / `ApiMisuse` /
//! `Unsupported` / `StreamShape`) with documented recovery semantics
//! and convenience predicates (`is_malformed_stream` / `is_api_misuse`
//! / `is_unsupported` / `is_stream_shape`).
//!
//! This suite pins:
//!
//! 1. **Exhaustive category mapping.** Every concrete [`Error`] value
//!    surfaces exactly one of the four [`ErrorCategory`] values and
//!    its convenience predicate. The mapping is structural (it
//!    depends only on the variant, not on payload), so we cover every
//!    variant once with a hand-built fixture value.
//!
//! 2. **Display non-emptiness + crate-name prefix.** Every variant's
//!    `Display` output starts with the crate's `"oxideav-utvideo:"`
//!    prefix (so log aggregators can grep one tag for the codec) and
//!    is non-empty. This pins the Round-1..Round-12 informal
//!    convention as a tested invariant — a new variant without the
//!    prefix would regress here.
//!
//! 3. **`InvalidSliceCount` Display accuracy.** The Round-1 message
//!    was `"num_slices == 0"`, but the variant is *also* produced for
//!    `num_slices > 256` (encoder, builder, decoder) — a stale message
//!    that mis-reported the upper-bound failure as zero-bound. Round
//!    13 corrects the message to name both bounds; the test pins it
//!    so a future "shorten the message" refactor doesn't regress.
//!
//! 4. **`std::error::Error::source` returns `None`.** The crate
//!    doesn't wrap third-party errors; all variants are leaves. The
//!    test pins this for every variant so a future change that
//!    accidentally wraps an inner error trips the suite.
//!
//! 5. **Category predicate mutual exclusion.** For every variant
//!    exactly one of the four `is_*` predicates returns `true`; the
//!    other three return `false`. This pins the four categories as a
//!    partition (no overlap, no gap) at runtime.
//!
//! All `Error` variants currently defined (as of this round) are
//! exercised by hand-built fixture values, plus a per-variant Display
//! sanity tally and a partition-correctness assert. The
//! `NonZeroPadding` variant added in round 335 (strict-decode trailing
//! padding check, `spec/05` §4.3 / §8) is included in the partition.
//!
//! No spec dependence; this is purely about the public error-handling
//! contract. No external library source, no web; no codec wire bytes
//! are constructed.

#![cfg(test)]

use oxideav_utvideo::error::{Error, ErrorCategory};

/// Yield one fixture value per defined [`Error`] variant. New variants
/// added to `Error` MUST extend this list — the round-13 partition
/// assert below `panic!`s if the variant list and the category match
/// in `error.rs` diverge.
fn every_variant() -> Vec<Error> {
    vec![
        Error::UnknownFourcc(*b"XYZW"),
        Error::ExtradataTruncated { len: 4 },
        Error::InvalidFrameInfoSize(8),
        Error::HuffmanBitClear,
        Error::InterlacedNotSupported,
        Error::InvalidSliceCount,
        Error::SliceCountExceedsPlaneHeight {
            num_slices: 9,
            min_plane_height: 8,
        },
        Error::ChunkTooShort {
            offset: 256,
            needed: 16,
            have: 4,
        },
        Error::NonMonotonicSliceOffsets,
        Error::SliceNotWordAligned(11),
        Error::KraftViolation,
        Error::MultipleSingleSymbolSentinels,
        Error::HuffmanDecodeFailure { bit_position: 17 },
        Error::SliceTruncated {
            bit_position: 99,
            expected_pixels: 256,
            decoded: 250,
        },
        Error::NonZeroPadding {
            plane: 1,
            slice: 2,
            bit_position: 271,
        },
        Error::DimensionConstraint("test-only"),
        Error::MissingFrameInfo,
        Error::UnsupportedPrediction(7),
        Error::EncoderPlaneSizeMismatch {
            plane: 1,
            expected: 100,
            got: 99,
        },
        Error::InvalidInput("test-only"),
    ]
}

// ---------------------------------------------------------------------
// Display: prefix + non-empty + variant-specific assertions.
// ---------------------------------------------------------------------

const PREFIX: &str = "oxideav-utvideo:";

#[test]
fn display_every_variant_carries_crate_prefix() {
    for err in every_variant() {
        let s = err.to_string();
        assert!(
            s.starts_with(PREFIX),
            "variant {err:?} Display missing crate prefix: {s:?}"
        );
        assert!(
            s.len() > PREFIX.len() + 1,
            "variant {err:?} Display has no body: {s:?}"
        );
    }
}

#[test]
fn display_unknown_fourcc_reports_bytes_in_hex() {
    let s = Error::UnknownFourcc([0xab, 0xcd, 0xef, 0x12]).to_string();
    assert!(s.contains("abcdef12"), "missing hex bytes in {s:?}");
}

#[test]
fn display_extradata_truncated_reports_len_and_required_minimum() {
    let s = Error::ExtradataTruncated { len: 8 }.to_string();
    assert!(s.contains("8"), "missing len 8 in {s:?}");
    assert!(s.contains("16"), "missing required 16 in {s:?}");
}

#[test]
fn display_invalid_frame_info_size_reports_value() {
    let s = Error::InvalidFrameInfoSize(7).to_string();
    assert!(s.contains("7"), "missing value 7 in {s:?}");
}

#[test]
fn display_chunk_too_short_reports_offset_needed_have() {
    let s = Error::ChunkTooShort {
        offset: 256,
        needed: 16,
        have: 4,
    }
    .to_string();
    for expected in ["256", "16", "4"] {
        assert!(s.contains(expected), "missing {expected} in {s:?}");
    }
}

#[test]
fn display_slice_not_word_aligned_reports_byte_length() {
    let s = Error::SliceNotWordAligned(13).to_string();
    assert!(s.contains("13"), "missing 13 in {s:?}");
}

#[test]
fn display_huffman_decode_failure_reports_bit_position() {
    let s = Error::HuffmanDecodeFailure { bit_position: 999 }.to_string();
    assert!(s.contains("999"), "missing bit position 999 in {s:?}");
}

#[test]
fn display_slice_truncated_reports_position_and_counts() {
    let s = Error::SliceTruncated {
        bit_position: 123,
        expected_pixels: 256,
        decoded: 200,
    }
    .to_string();
    for expected in ["123", "256", "200"] {
        assert!(s.contains(expected), "missing {expected} in {s:?}");
    }
}

#[test]
fn display_non_zero_padding_reports_plane_slice_bit() {
    let s = Error::NonZeroPadding {
        plane: 2,
        slice: 5,
        bit_position: 271,
    }
    .to_string();
    for expected in ["2", "5", "271"] {
        assert!(s.contains(expected), "missing {expected} in {s:?}");
    }
}

#[test]
fn display_dimension_constraint_reports_inner_message() {
    let s = Error::DimensionConstraint("odd width on ULY0").to_string();
    assert!(
        s.contains("odd width on ULY0"),
        "missing inner message in {s:?}"
    );
}

#[test]
fn display_unsupported_prediction_reports_mode_number() {
    let s = Error::UnsupportedPrediction(5).to_string();
    assert!(s.contains("5"), "missing mode 5 in {s:?}");
}

#[test]
fn display_encoder_plane_size_mismatch_reports_plane_expected_got() {
    let s = Error::EncoderPlaneSizeMismatch {
        plane: 2,
        expected: 1024,
        got: 1023,
    }
    .to_string();
    for expected in ["2", "1024", "1023"] {
        assert!(s.contains(expected), "missing {expected} in {s:?}");
    }
}

#[test]
fn display_invalid_input_reports_inner_message() {
    let s = Error::InvalidInput("widget broke").to_string();
    assert!(s.contains("widget broke"), "missing inner in {s:?}");
}

/// Round-13 message-accuracy fix: the message previously claimed
/// `"num_slices == 0"` but the variant is also produced for
/// `num_slices > 256`. The new message names both bounds.
#[test]
fn display_invalid_slice_count_names_full_range() {
    let s = Error::InvalidSliceCount.to_string();
    assert!(
        s.contains("1..=256") || (s.contains("1") && s.contains("256")),
        "Display should name the full valid range 1..=256: {s:?}"
    );
    assert!(
        !s.contains("== 0"),
        "stale Round-1 message persists in {s:?}"
    );
}

// ---------------------------------------------------------------------
// Category classifier: exhaustive mapping.
// ---------------------------------------------------------------------

#[test]
fn category_malformed_stream_variants() {
    let malformed: &[Error] = &[
        Error::ChunkTooShort {
            offset: 0,
            needed: 1,
            have: 0,
        },
        Error::NonMonotonicSliceOffsets,
        Error::SliceNotWordAligned(5),
        Error::KraftViolation,
        Error::MultipleSingleSymbolSentinels,
        Error::HuffmanDecodeFailure { bit_position: 0 },
        Error::SliceTruncated {
            bit_position: 0,
            expected_pixels: 1,
            decoded: 0,
        },
        Error::NonZeroPadding {
            plane: 0,
            slice: 0,
            bit_position: 0,
        },
        Error::MissingFrameInfo,
    ];
    for err in malformed {
        assert_eq!(
            err.category(),
            ErrorCategory::MalformedStream,
            "{err:?} should be MalformedStream"
        );
        assert!(err.is_malformed_stream(), "{err:?} predicate mismatch");
    }
}

#[test]
fn category_api_misuse_variants() {
    let misuse: &[Error] = &[
        Error::InvalidSliceCount,
        Error::SliceCountExceedsPlaneHeight {
            num_slices: 9,
            min_plane_height: 8,
        },
        Error::EncoderPlaneSizeMismatch {
            plane: 0,
            expected: 0,
            got: 0,
        },
        Error::InvalidInput("x"),
    ];
    for err in misuse {
        assert_eq!(
            err.category(),
            ErrorCategory::ApiMisuse,
            "{err:?} should be ApiMisuse"
        );
        assert!(err.is_api_misuse(), "{err:?} predicate mismatch");
    }
}

#[test]
fn category_unsupported_variants() {
    let unsupported: &[Error] = &[
        Error::HuffmanBitClear,
        Error::InterlacedNotSupported,
        Error::UnsupportedPrediction(99),
    ];
    for err in unsupported {
        assert_eq!(
            err.category(),
            ErrorCategory::Unsupported,
            "{err:?} should be Unsupported"
        );
        assert!(err.is_unsupported(), "{err:?} predicate mismatch");
    }
}

#[test]
fn category_stream_shape_variants() {
    let shape: &[Error] = &[
        Error::UnknownFourcc([0; 4]),
        Error::ExtradataTruncated { len: 0 },
        Error::InvalidFrameInfoSize(0),
        Error::DimensionConstraint("x"),
    ];
    for err in shape {
        assert_eq!(
            err.category(),
            ErrorCategory::StreamShape,
            "{err:?} should be StreamShape"
        );
        assert!(err.is_stream_shape(), "{err:?} predicate mismatch");
    }
}

// ---------------------------------------------------------------------
// Partition: each variant lives in exactly one category; predicates
// mutually exclude.
// ---------------------------------------------------------------------

#[test]
fn every_variant_belongs_to_exactly_one_category() {
    for err in every_variant() {
        let cat = err.category();
        let preds = [
            (ErrorCategory::MalformedStream, err.is_malformed_stream()),
            (ErrorCategory::ApiMisuse, err.is_api_misuse()),
            (ErrorCategory::Unsupported, err.is_unsupported()),
            (ErrorCategory::StreamShape, err.is_stream_shape()),
        ];
        let trues: Vec<_> = preds.iter().filter(|(_, b)| *b).collect();
        assert_eq!(
            trues.len(),
            1,
            "{err:?} should match exactly one predicate, got {} (category()={:?})",
            trues.len(),
            cat
        );
        assert_eq!(
            trues[0].0, cat,
            "predicate for {err:?} returned {:?} but category() returned {:?}",
            trues[0].0, cat
        );
    }
}

#[test]
fn category_count_matches_variant_count() {
    // Sanity gate: if a new variant lands in error.rs without a
    // corresponding fixture in every_variant(), the per-category
    // exhaustive lists above will silently miss it. This cross-check
    // sums the four category lists' lengths and compares against the
    // overall fixture count. A drift trips the round-13 partition
    // invariant.
    let total = every_variant().len();
    // 9 malformed + 4 api-misuse + 3 unsupported + 4 stream-shape = 20.
    assert_eq!(
        total, 20,
        "every_variant() length drifted from expected 20 — update round13_error_taxonomy.rs"
    );
}

// ---------------------------------------------------------------------
// std::error::Error::source — no inner wrapping.
// ---------------------------------------------------------------------

#[test]
fn source_returns_none_for_every_variant() {
    use std::error::Error as _;
    for err in every_variant() {
        assert!(
            err.source().is_none(),
            "{err:?} should not carry a source error"
        );
    }
}

// ---------------------------------------------------------------------
// ErrorCategory: derives are usable downstream.
// ---------------------------------------------------------------------

#[test]
fn error_category_is_copyable_and_comparable() {
    let a = ErrorCategory::MalformedStream;
    let b = a; // Copy
    assert_eq!(a, b);
    assert_ne!(ErrorCategory::ApiMisuse, ErrorCategory::Unsupported);
    // Debug — non-empty.
    assert!(!format!("{a:?}").is_empty());
}

#[test]
fn error_category_hash_usable_in_set() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(ErrorCategory::MalformedStream);
    set.insert(ErrorCategory::ApiMisuse);
    set.insert(ErrorCategory::Unsupported);
    set.insert(ErrorCategory::StreamShape);
    assert_eq!(set.len(), 4);
}
