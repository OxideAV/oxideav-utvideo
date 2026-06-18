//! Crate-local error type.
//!
//! Variants here track the specific failure points of the Ut Video
//! pipeline as documented in `docs/video/utvideo/spec/01..05`. The
//! pipeline itself is small (extradata parse → per-plane Huffman →
//! per-slice predictor → optional RGB inverse decorrelation), so the
//! error surface is correspondingly small.
#![allow(clippy::manual_range_contains)]

/// Errors produced by the decoder + encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `BITMAPINFOHEADER.biCompression` (or the encoder caller's
    /// FourCC argument) was not one of the five `ULRG`/`ULRA`/`ULY0`/
    /// `ULY2`/`ULY4` codes specified in `spec/01` §2.
    UnknownFourcc([u8; 4]),

    /// Extradata block was shorter than the 16-byte minimum
    /// (`spec/01` §3 + §4).
    ExtradataTruncated { len: usize },

    /// `frame_info_size` in extradata was not 4. The decoder rejects
    /// per `spec/01` §4.3.
    InvalidFrameInfoSize(u32),

    /// Encoding flags' Huffman bit (`0x00000001`) was clear. The raw
    /// slice mode is not specified by `spec/01` §4.4.1.
    HuffmanBitClear,

    /// Encoding flags' interlaced bit (`0x00000800`) was set. Not
    /// supported in round 1; deferred per `spec/01` §4.4.2.
    InterlacedNotSupported,

    /// `num_slices` derived from extradata flags was 0. The wire
    /// formula always yields >= 1; this guards against caller misuse.
    InvalidSliceCount,

    /// The `00dc` chunk payload ended before the per-plane structure
    /// (256-byte Huffman descriptor + slice-end offsets table + slice
    /// data) finished parsing. `spec/02` §7.
    ChunkTooShort {
        offset: usize,
        needed: usize,
        have: usize,
    },

    /// `slice_end_offsets` was not monotonically non-decreasing.
    /// `spec/02` §5.
    NonMonotonicSliceOffsets,

    /// A slice's compressed-byte length was not a multiple of 4.
    /// `spec/05` §4.1 word alignment is wire-confirmed.
    SliceNotWordAligned(usize),

    /// The Huffman code-length descriptor failed Kraft equality —
    /// i.e. `Σ 2^(-code_length[s]) != 1` over assigned symbols. The
    /// canonical-completeness invariant of `spec/02` §4 + `spec/05` §2.
    KraftViolation,

    /// More than one symbol carried the single-symbol sentinel
    /// `code_length = 0`. Malformed; rejected per `spec/05` §11.
    MultipleSingleSymbolSentinels,

    /// A bit-prefix in slice data did not match any code in the
    /// constructed Huffman table. The slice is malformed.
    HuffmanDecodeFailure { bit_position: usize },

    /// Decoded residual stream stopped at the slice boundary before
    /// every expected sample was produced (i.e. slice ran out of
    /// bits mid-symbol).
    SliceTruncated {
        bit_position: usize,
        expected_pixels: usize,
        decoded: usize,
    },

    /// A slice's trailing bit-stream padding was not all zero. Per
    /// `spec/05` §4.3 the bits after a slice's last Huffman code up to
    /// the next 32-bit word boundary are zero-padded, and `spec/05` §8
    /// names a non-zero padding bit a SHOULD-warn for defensive
    /// decoders. This variant is produced **only** by the opt-in
    /// strict decode path ([`crate::decode_frame_strict`]); the
    /// default decoder follows the spec's "MUST NOT consume padding"
    /// rule and ignores trailing bits. `bit_position` is the absolute
    /// bit offset (within the offending slice's data) of the first
    /// non-zero padding bit; `plane` / `slice` locate it in the frame.
    NonZeroPadding {
        plane: usize,
        slice: usize,
        bit_position: usize,
    },

    /// Caller-supplied frame dimensions disagreed with the FOURCC's
    /// chroma-subsampling rules (`spec/02` §3.2). E.g. odd width on
    /// ULY0 or ULY2.
    DimensionConstraint(&'static str),

    /// A trailing 4-byte frame-info dword was missing from the chunk
    /// payload (`spec/02` §6). Effectively a length mismatch.
    MissingFrameInfo,

    /// The 4-byte frame-info dword named a prediction mode whose
    /// implementation is deferred. Round 1 supports modes 0/1/2/3
    /// per `spec/04` — this variant is reserved for future revisions.
    UnsupportedPrediction(u32),

    /// The encoder was asked to emit a plane whose pixel count did
    /// not match the FOURCC-derived `(plane_width * plane_height)`.
    EncoderPlaneSizeMismatch {
        plane: usize,
        expected: usize,
        got: usize,
    },

    /// Generic catch-all for caller mistakes the type system did not
    /// stop. Carries a `&'static str` so we don't pull `String` into
    /// `no_std`-friendly callers.
    InvalidInput(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::UnknownFourcc(b) => write!(
                f,
                "oxideav-utvideo: unknown FourCC {:02x}{:02x}{:02x}{:02x}",
                b[0], b[1], b[2], b[3]
            ),
            Error::ExtradataTruncated { len } => write!(
                f,
                "oxideav-utvideo: extradata too short ({len} bytes; require >= 16)"
            ),
            Error::InvalidFrameInfoSize(n) => {
                write!(f, "oxideav-utvideo: extradata frame_info_size != 4 ({n})")
            }
            Error::HuffmanBitClear => f.write_str(
                "oxideav-utvideo: extradata flags Huffman bit clear (raw slice mode unsupported)",
            ),
            Error::InterlacedNotSupported => {
                f.write_str("oxideav-utvideo: extradata interlaced bit set (round 1 unsupported)")
            }
            Error::InvalidSliceCount => f.write_str(
                "oxideav-utvideo: num_slices out of range (must be 1..=256 per spec/01 §4.4.3)",
            ),
            Error::ChunkTooShort {
                offset,
                needed,
                have,
            } => write!(
                f,
                "oxideav-utvideo: chunk truncated at offset {offset} (need {needed}, have {have})"
            ),
            Error::NonMonotonicSliceOffsets => {
                f.write_str("oxideav-utvideo: non-monotonic slice end offsets")
            }
            Error::SliceNotWordAligned(n) => write!(
                f,
                "oxideav-utvideo: slice byte-length {n} not a multiple of 4"
            ),
            Error::KraftViolation => f.write_str(
                "oxideav-utvideo: Huffman code-length descriptor violates Kraft equality",
            ),
            Error::MultipleSingleSymbolSentinels => {
                f.write_str("oxideav-utvideo: multiple code_length=0 sentinels")
            }
            Error::HuffmanDecodeFailure { bit_position } => write!(
                f,
                "oxideav-utvideo: Huffman bit-prefix unmatched at bit {bit_position}"
            ),
            Error::SliceTruncated {
                bit_position,
                expected_pixels,
                decoded,
            } => write!(
                f,
                "oxideav-utvideo: slice truncated at bit {bit_position} (decoded {decoded}/{expected_pixels} pixels)"
            ),
            Error::NonZeroPadding {
                plane,
                slice,
                bit_position,
            } => write!(
                f,
                "oxideav-utvideo: non-zero slice padding in plane {plane} slice {slice} at bit {bit_position}"
            ),
            Error::DimensionConstraint(s) => {
                write!(f, "oxideav-utvideo: dimension constraint violated: {s}")
            }
            Error::MissingFrameInfo => {
                f.write_str("oxideav-utvideo: chunk missing trailing frame_info dword")
            }
            Error::UnsupportedPrediction(n) => write!(
                f,
                "oxideav-utvideo: prediction mode {n} not supported in this build"
            ),
            Error::EncoderPlaneSizeMismatch {
                plane,
                expected,
                got,
            } => write!(
                f,
                "oxideav-utvideo: encoder plane {plane} size mismatch (expected {expected}, got {got})"
            ),
            Error::InvalidInput(s) => write!(f, "oxideav-utvideo: invalid input: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// Crate-local result alias.
pub type Result<T> = core::result::Result<T, Error>;

/// Coarse-grained taxonomy of [`Error`] variants for callers that want
/// a recovery / retry policy that doesn't pattern-match on every
/// variant individually.
///
/// The four categories partition every [`Error`] variant exactly once
/// (verified by `tests/round13_error_taxonomy.rs`). A new variant
/// added to `Error` MUST land in one of the four categories; the
/// classifier is `#[non_exhaustive]` so adding a fifth category in a
/// future round is a non-breaking change.
///
/// ## Recovery semantics
///
/// - [`MalformedStream`](ErrorCategory::MalformedStream) — the input
///   bytes do not match the wire format documented in
///   `docs/video/utvideo/spec/01..05`. The caller cannot recover by
///   retrying with the same input; the producer's bytes are
///   corrupt. A muxer driving the decoder over a packet stream MAY
///   skip the offending packet and resync at the next keyframe.
///   The `00dc`-chunk single-byte-flip sweep in
///   `tests/round8_malformed_decode.rs` is the wire-format-bit-flip
///   audit of this category.
/// - [`ApiMisuse`](ErrorCategory::ApiMisuse) — the caller violated
///   the typed contract of [`crate::encode_frame`] /
///   [`crate::StreamConfig::new`] / [`crate::Extradata::ffmpeg_for`]
///   (e.g. wrong plane count, mis-sized per-plane buffer, slice
///   count outside `1..=256`, zero width/height). These are
///   programming bugs the caller can fix with a corrected call;
///   they do not indicate corrupt wire data.
/// - [`Unsupported`](ErrorCategory::Unsupported) — the wire data is
///   structurally valid but exercises a code path this build doesn't
///   implement (raw / non-Huffman slice mode per `spec/01` §4.4.1;
///   interlaced bit set per `spec/01` §4.4.2; future prediction
///   modes per `spec/04`). These are bounded out-of-corpus paths
///   documented in `audit/00-report.md` §5.2.
/// - [`StreamShape`](ErrorCategory::StreamShape) — extradata or
///   stream-level identification metadata was malformed (unknown
///   FourCC, truncated extradata, wrong `frame_info_size`,
///   dimension constraint). Sits between `MalformedStream` (per-frame
///   wire) and `ApiMisuse` (caller-side typed misuse); a demuxer
///   should reject the stream rather than re-attempt per-frame
///   decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCategory {
    /// Per-frame wire bytes do not match `spec/02` + `spec/05`.
    /// Examples: `ChunkTooShort`, `NonMonotonicSliceOffsets`,
    /// `SliceNotWordAligned`, `KraftViolation`,
    /// `MultipleSingleSymbolSentinels`, `HuffmanDecodeFailure`,
    /// `SliceTruncated`, `NonZeroPadding`, `MissingFrameInfo`.
    MalformedStream,
    /// Caller-side typed contract violation. Examples:
    /// `EncoderPlaneSizeMismatch`, `InvalidSliceCount`,
    /// `InvalidInput`.
    ApiMisuse,
    /// Structurally valid wire data on a code path this build doesn't
    /// implement. Examples: `HuffmanBitClear` (raw slice mode),
    /// `InterlacedNotSupported`, `UnsupportedPrediction`.
    Unsupported,
    /// Stream-level identification metadata malformed (extradata /
    /// FourCC / dims). Examples: `UnknownFourcc`,
    /// `ExtradataTruncated`, `InvalidFrameInfoSize`,
    /// `DimensionConstraint`.
    StreamShape,
}

impl Error {
    /// Coarse classification of this error for recovery / retry logic
    /// (see [`ErrorCategory`] for the four categories and their
    /// recovery semantics).
    ///
    /// The mapping is documented per-variant on [`Error`] and pinned
    /// exhaustively by `tests/round13_error_taxonomy.rs` — adding a
    /// new `Error` variant requires extending the `match` here in the
    /// same commit (no `_ =>` fallback by design).
    pub fn category(&self) -> ErrorCategory {
        match self {
            // Stream-level identification metadata.
            Error::UnknownFourcc(_)
            | Error::ExtradataTruncated { .. }
            | Error::InvalidFrameInfoSize(_)
            | Error::DimensionConstraint(_) => ErrorCategory::StreamShape,

            // Caller-side typed contract violations.
            Error::InvalidSliceCount
            | Error::EncoderPlaneSizeMismatch { .. }
            | Error::InvalidInput(_) => ErrorCategory::ApiMisuse,

            // Structurally valid wire on a code path we don't implement.
            Error::HuffmanBitClear
            | Error::InterlacedNotSupported
            | Error::UnsupportedPrediction(_) => ErrorCategory::Unsupported,

            // Per-frame wire bytes do not match spec/02 + spec/05.
            Error::ChunkTooShort { .. }
            | Error::NonMonotonicSliceOffsets
            | Error::SliceNotWordAligned(_)
            | Error::KraftViolation
            | Error::MultipleSingleSymbolSentinels
            | Error::HuffmanDecodeFailure { .. }
            | Error::SliceTruncated { .. }
            | Error::NonZeroPadding { .. }
            | Error::MissingFrameInfo => ErrorCategory::MalformedStream,
        }
    }

    /// True if this error indicates corrupt per-frame wire bytes
    /// (category [`ErrorCategory::MalformedStream`]). A muxer-level
    /// caller MAY skip the offending packet and resync.
    pub fn is_malformed_stream(&self) -> bool {
        matches!(self.category(), ErrorCategory::MalformedStream)
    }

    /// True if this error indicates the caller violated the typed
    /// contract of the public API (category [`ErrorCategory::ApiMisuse`]).
    /// These are programming bugs; the call cannot succeed without
    /// caller-side fixes.
    pub fn is_api_misuse(&self) -> bool {
        matches!(self.category(), ErrorCategory::ApiMisuse)
    }

    /// True if this error indicates a structurally-valid wire path
    /// this build doesn't implement (category
    /// [`ErrorCategory::Unsupported`]). Bounded out-of-corpus paths
    /// documented in `audit/00-report.md` §5.2.
    pub fn is_unsupported(&self) -> bool {
        matches!(self.category(), ErrorCategory::Unsupported)
    }

    /// True if this error indicates stream-level identification
    /// metadata is malformed (category [`ErrorCategory::StreamShape`]).
    /// A demuxer should reject the stream rather than retry the frame.
    pub fn is_stream_shape(&self) -> bool {
        matches!(self.category(), ErrorCategory::StreamShape)
    }
}
