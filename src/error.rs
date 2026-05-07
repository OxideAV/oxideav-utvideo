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
            Error::InvalidSliceCount => f.write_str("oxideav-utvideo: num_slices == 0"),
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
