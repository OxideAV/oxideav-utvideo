//! Pure-Rust Ut Video classic-family lossless decoder + encoder.
//!
//! **Round 1 — clean-room rebuild.** Implements the five 8-bit
//! Ut Video FourCCs documented in `docs/video/utvideo/spec/`:
//! [`Fourcc::Uly0`], [`Fourcc::Uly2`], [`Fourcc::Uly4`],
//! [`Fourcc::Ulrg`], [`Fourcc::Ulra`].
//!
//! The crate is **codec-only**: AVI / VfW container handling
//! (including the FourCC + extradata that identifies a Ut Video
//! stream on the wire) lives in `oxideav-avi`. Callers hand us
//! parsed [`StreamConfig`] + frame bytes; we hand back per-plane
//! samples or — encode side — chunk-payload bytes.
//!
//! ## Pipeline
//!
//! 1. **Identification** ([`fourcc::Fourcc`] + [`fourcc::Extradata`]):
//!    parse the FourCC, validate the 16-byte extradata, derive the
//!    `num_slices` value from the flags top byte. `spec/01`.
//! 2. **Frame walk** ([`decoder::decode_frame`]): per-plane Huffman
//!    descriptor (256 bytes) + slice-end-offset table + slice data;
//!    trailing 4-byte `frame_info` dword. `spec/02`.
//! 3. **Per-plane Huffman** ([`huffman::HuffmanTable::build`]): the
//!    "RFC 1951 mirrored" canonical-code construction of `spec/05`
//!    §2.2; bit reader is 32-bit LE words, MSB-first inside each
//!    word.
//! 4. **Per-slice predictor** ([`predict`]): None / Left / Gradient /
//!    Median per `spec/04` §§3, 4, 7, 5. The per-slice +128
//!    first-pixel seed is universal.
//! 5. **RGB inverse decorrelation** for ULRG / ULRA: `B = B' + G - 128`,
//!    `R = R' + G - 128` per `spec/04` §6.
//!
//! ## Public API
//!
//! - [`decoder::decode_frame`] — decode one chunk payload.
//! - [`encoder::encode_frame`] — encode one frame from per-plane
//!   pixel buffers; produces a chunk payload.
//! - [`Error`] / [`Result`] — crate-local error type.
//!
//! ## Cargo features
//!
//! - **`registry`** (default): wire the crate into `oxideav-core`'s
//!   codec registry.

#![forbid(unsafe_code)]

pub mod decoder;
pub mod encoder;
pub mod error;
pub mod fourcc;
pub mod huffman;
pub mod predict;
#[cfg(feature = "registry")]
pub mod registry;
#[cfg(test)]
mod roundtrip_tests;

pub use crate::decoder::{decode_frame, DecodedFrame, DecodedPlane, PlaneLabel};
pub use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
pub use crate::error::{Error, Result};
pub use crate::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

// Framework integration — only when the `registry` feature is on.
#[cfg(feature = "registry")]
pub use crate::registry::{register, register_codecs, CODEC_ID_STR};

#[cfg(feature = "registry")]
oxideav_core::register!("oxideav-utvideo", register);
