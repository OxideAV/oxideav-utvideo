//! FOURCC + extradata wire-format definitions per `spec/01`.
//!
//! The codec lives inside an AVI / VfW container; this module owns
//! the typed identification surface that flows from container bytes
//! to the per-frame decoder. AVI demux/mux itself is out of scope —
//! the consumer (e.g. `oxideav-avi`) hands us the FourCC + extradata
//! bytes verbatim and we hand back a typed
//! [`StreamConfig`].

use crate::error::{Error, Result};

/// One of the five Ut Video FourCCs accepted by FFmpeg 7.1.2 per
/// `spec/01` §2. Drives plane count, plane layout, and chroma
/// subsampling for everything downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fourcc {
    /// `ULY0` — YUV 4:2:0 planar (Y, U, V; chroma half W and half H).
    Uly0,
    /// `ULY2` — YUV 4:2:2 planar (Y, U, V; chroma half W, full H).
    Uly2,
    /// `ULY4` — YUV 4:4:4 planar (Y, U, V; chroma == luma dims).
    Uly4,
    /// `ULRG` — RGB planar; on-wire G, B, R per `spec/02` Appendix C.
    Ulrg,
    /// `ULRA` — RGBA planar; on-wire G, B, R, A.
    Ulra,
}

impl Fourcc {
    /// Parse a 4-byte FourCC array. Returns
    /// [`Error::UnknownFourcc`] if `code` is not one of the five
    /// accepted values.
    pub fn from_bytes(code: [u8; 4]) -> Result<Self> {
        match &code {
            b"ULY0" => Ok(Fourcc::Uly0),
            b"ULY2" => Ok(Fourcc::Uly2),
            b"ULY4" => Ok(Fourcc::Uly4),
            b"ULRG" => Ok(Fourcc::Ulrg),
            b"ULRA" => Ok(Fourcc::Ulra),
            _ => Err(Error::UnknownFourcc(code)),
        }
    }

    /// 4 ASCII bytes in file order, matching `fccHandler` /
    /// `biCompression`.
    pub fn as_bytes(self) -> &'static [u8; 4] {
        match self {
            Fourcc::Uly0 => b"ULY0",
            Fourcc::Uly2 => b"ULY2",
            Fourcc::Uly4 => b"ULY4",
            Fourcc::Ulrg => b"ULRG",
            Fourcc::Ulra => b"ULRA",
        }
    }

    /// Plane count as it appears on the wire. 3 for the YUV trio +
    /// ULRG; 4 for ULRA. `spec/02` §3.
    pub fn plane_count(self) -> usize {
        match self {
            Fourcc::Uly0 | Fourcc::Uly2 | Fourcc::Uly4 | Fourcc::Ulrg => 3,
            Fourcc::Ulra => 4,
        }
    }

    /// `true` for the RGB family (ULRG, ULRA). The G plane is direct
    /// and B/R are stored as `(B-G+128)`/`(R-G+128) mod 256`
    /// (`spec/04` §6).
    pub fn is_rgb_family(self) -> bool {
        matches!(self, Fourcc::Ulrg | Fourcc::Ulra)
    }

    /// `true` for ULRA only. Adds a direct A plane on the wire after
    /// R, with no decorrelation transform.
    pub fn has_alpha(self) -> bool {
        matches!(self, Fourcc::Ulra)
    }

    /// `(width, height)` of plane `idx` for a frame of dimensions
    /// `(W, H)`. `spec/02` §3.1.
    pub fn plane_dim(self, idx: usize, width: u32, height: u32) -> (u32, u32) {
        match self {
            Fourcc::Uly0 => match idx {
                0 => (width, height),
                1 | 2 => (width / 2, height / 2),
                _ => (0, 0),
            },
            Fourcc::Uly2 => match idx {
                0 => (width, height),
                1 | 2 => (width / 2, height),
                _ => (0, 0),
            },
            Fourcc::Uly4 => (width, height),
            Fourcc::Ulrg | Fourcc::Ulra => (width, height),
        }
    }

    /// The 4-byte source-format tag FFmpeg 7.1.2 writes at extradata
    /// offset `+0x04` for this FOURCC, per `spec/01` §2.2 + §5 (test
    /// set `T1`):
    ///
    /// | FOURCC | Tag bytes      | ASCII / hex            |
    /// |--------|----------------|------------------------|
    /// | ULY0   | `59 56 31 32`  | `"YV12"`                |
    /// | ULY2   | `59 55 59 32`  | `"YUY2"`                |
    /// | ULY4   | `59 56 32 34`  | `"YV24"`                |
    /// | ULRG   | `00 00 01 18`  | `0x18010000` LE-loaded |
    /// | ULRA   | `00 00 02 18`  | `0x18020000` LE-loaded |
    ///
    /// Encoders MAY mirror this tag for AVI / VfW interop; decoders MAY
    /// ignore it (round-1 audit §5.2: implementer-resolvable open
    /// question 2 — RGB source-format tag structure is hypothesis-only,
    /// but the four bytes themselves are FFmpeg-pinned).
    pub fn ffmpeg_source_format_tag(self) -> [u8; 4] {
        match self {
            Fourcc::Uly0 => *b"YV12",
            Fourcc::Uly2 => *b"YUY2",
            Fourcc::Uly4 => *b"YV24",
            Fourcc::Ulrg => [0x00, 0x00, 0x01, 0x18],
            Fourcc::Ulra => [0x00, 0x00, 0x02, 0x18],
        }
    }

    /// Validate `(W, H)` against the FOURCC's chroma-subsampling
    /// constraints (`spec/02` §3.2):
    /// - ULY0 requires even width AND even height,
    /// - ULY2 requires even width,
    /// - others accept any positive dimensions.
    pub fn validate_dims(self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::DimensionConstraint("width/height must be > 0"));
        }
        match self {
            Fourcc::Uly0 if width % 2 != 0 || height % 2 != 0 => Err(Error::DimensionConstraint(
                "ULY0 requires even width and height",
            )),
            Fourcc::Uly2 if width % 2 != 0 => {
                Err(Error::DimensionConstraint("ULY2 requires even width"))
            }
            _ => Ok(()),
        }
    }
}

/// Wire prediction mode pulled from the per-frame info dword,
/// `frame_info & 0x300`. See `spec/02` §6.1 + `spec/04`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Predictor {
    /// 0 — identity; residual IS the (decorrelated) sample.
    None,
    /// 1 — left neighbour, continuous within slice; first-pixel
    /// seed is 128 per slice.
    Left,
    /// 2 — modular gradient (`P = left + top - top_left mod 256`),
    /// `P = top` at column-0 edge inside slice, `P = left` on first
    /// row, +128 first-pixel seed.
    Gradient,
    /// 3 — JPEG-LS MED median, with per-slice +128 seed; column-0
    /// edge inside slice uses continuous-wrap MED.
    Median,
}

impl Predictor {
    /// Build from `(frame_info >> 8) & 3`.
    pub fn from_frame_info(info: u32) -> Self {
        match (info >> 8) & 0x3 {
            0 => Predictor::None,
            1 => Predictor::Left,
            2 => Predictor::Gradient,
            _ => Predictor::Median,
        }
    }

    /// `(mode << 8)` ready to OR into a frame_info dword.
    pub fn as_frame_info_bits(self) -> u32 {
        match self {
            Predictor::None => 0x000,
            Predictor::Left => 0x100,
            Predictor::Gradient => 0x200,
            Predictor::Median => 0x300,
        }
    }
}

/// Decoded view of the 16-byte extradata block per `spec/01` §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extradata {
    /// `+0x00`: encoder version (LE u32). Inspected for diagnostics
    /// only; do not reject.
    pub encoder_version: u32,
    /// `+0x04`: source-format tag, 4 bytes verbatim. Decoders MAY
    /// ignore. Stored as raw bytes since the wiki says "BE" but the
    /// observed values are not always printable.
    pub source_format_tag: [u8; 4],
    /// `+0x08`: frame-info-size — must be 4 in the FFmpeg corpus.
    pub frame_info_size: u32,
    /// `+0x0c`: encoding flags — Huffman bit, interlaced bit, slice
    /// count high byte.
    pub flags: u32,
}

impl Extradata {
    /// Parse a 16-byte `BITMAPINFOHEADER`-trailing extradata block.
    /// Round 1 rejects:
    /// - frame_info_size != 4 (`spec/01` §4.3),
    /// - Huffman bit clear (`spec/01` §4.4.1; raw mode unspecified),
    /// - interlaced bit set (`spec/01` §4.4.2; deferred).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 {
            return Err(Error::ExtradataTruncated { len: bytes.len() });
        }
        let encoder_version = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let source_format_tag = bytes[4..8].try_into().unwrap();
        let frame_info_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let flags = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if frame_info_size != 4 {
            return Err(Error::InvalidFrameInfoSize(frame_info_size));
        }
        if flags & 0x0000_0001 == 0 {
            return Err(Error::HuffmanBitClear);
        }
        if flags & 0x0000_0800 != 0 {
            return Err(Error::InterlacedNotSupported);
        }
        Ok(Self {
            encoder_version,
            source_format_tag,
            frame_info_size,
            flags,
        })
    }

    /// `num_slices = ((flags >> 24) & 0xff) + 1` per `spec/01`
    /// §4.4.3.
    pub fn num_slices(&self) -> usize {
        (((self.flags >> 24) & 0xff) as usize) + 1
    }

    /// Build a 16-byte extradata block matching what FFmpeg 7.1.2's
    /// `utvideo` encoder writes for `fourcc` with `num_slices` slices,
    /// per `spec/01` §5 (test set `T1`) + §4.4.3 (slice-count formula):
    ///
    /// - `encoder_version = 0x0100_00f0` (constant across the FFmpeg
    ///   corpus, per `spec/01` §4.1).
    /// - `source_format_tag = ffmpeg_source_format_tag(fourcc)` per
    ///   `spec/01` §2.2 + §5.
    /// - `frame_info_size = 4` per `spec/01` §4.3.
    /// - `flags = 0x0000_0001 | ((num_slices - 1) << 24)` — Huffman bit
    ///   set, interlaced bit clear, slice-count high byte per
    ///   `spec/01` §4.4.
    ///
    /// Returns [`Error::InvalidSliceCount`] if `num_slices == 0` or
    /// `> 256` (the wire formula caps the encoded high byte at
    /// `0xff` → 256 slices).
    ///
    /// This builder closes audit/00-report.md §5.2 open items 1 and 2
    /// in the implementer-resolvable direction: mirror the FFmpeg
    /// values exactly so a synthesised stream is byte-identical to
    /// what FFmpeg would have written at the extradata level.
    pub fn ffmpeg_for(fourcc: Fourcc, num_slices: usize) -> Result<Self> {
        if num_slices == 0 || num_slices > 256 {
            return Err(Error::InvalidSliceCount);
        }
        Ok(Self {
            encoder_version: 0x0100_00f0,
            source_format_tag: fourcc.ffmpeg_source_format_tag(),
            frame_info_size: 4,
            flags: 0x0000_0001 | (((num_slices as u32 - 1) & 0xff) << 24),
        })
    }

    /// Serialise to 16 bytes in the order
    /// `encoder_version | source_format_tag | frame_info_size | flags`.
    /// Used by the test-only encoder (mod `encoder`) to mirror what
    /// FFmpeg writes.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.encoder_version.to_le_bytes());
        out[4..8].copy_from_slice(&self.source_format_tag);
        out[8..12].copy_from_slice(&self.frame_info_size.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out
    }
}

/// A fully-parsed identification surface for one Ut Video stream:
/// the FourCC + extradata together. This is what the codec layer
/// hands the per-frame decoder; no AVI bytes appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    pub fourcc: Fourcc,
    pub width: u32,
    pub height: u32,
    pub extradata: Extradata,
}

impl StreamConfig {
    pub fn new(fourcc: Fourcc, width: u32, height: u32, extradata: Extradata) -> Result<Self> {
        fourcc.validate_dims(width, height)?;
        if extradata.num_slices() == 0 {
            return Err(Error::InvalidSliceCount);
        }
        Ok(Self {
            fourcc,
            width,
            height,
            extradata,
        })
    }

    pub fn num_slices(&self) -> usize {
        self.extradata.num_slices()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_round_trip_all_five() {
        for code in [b"ULY0", b"ULY2", b"ULY4", b"ULRG", b"ULRA"] {
            let f = Fourcc::from_bytes(*code).unwrap();
            assert_eq!(f.as_bytes(), code);
        }
    }

    #[test]
    fn fourcc_unknown_rejected() {
        assert!(matches!(
            Fourcc::from_bytes(*b"ULZZ"),
            Err(Error::UnknownFourcc(_))
        ));
    }

    #[test]
    fn plane_count_per_fourcc() {
        assert_eq!(Fourcc::Uly0.plane_count(), 3);
        assert_eq!(Fourcc::Uly2.plane_count(), 3);
        assert_eq!(Fourcc::Uly4.plane_count(), 3);
        assert_eq!(Fourcc::Ulrg.plane_count(), 3);
        assert_eq!(Fourcc::Ulra.plane_count(), 4);
    }

    #[test]
    fn plane_dim_subsampling() {
        // ULY0 16×16: Y 16×16, U/V 8×8.
        assert_eq!(Fourcc::Uly0.plane_dim(0, 16, 16), (16, 16));
        assert_eq!(Fourcc::Uly0.plane_dim(1, 16, 16), (8, 8));
        assert_eq!(Fourcc::Uly0.plane_dim(2, 16, 16), (8, 8));
        // ULY2 16×16: Y 16×16, U/V 8×16.
        assert_eq!(Fourcc::Uly2.plane_dim(0, 16, 16), (16, 16));
        assert_eq!(Fourcc::Uly2.plane_dim(1, 16, 16), (8, 16));
        // ULY4 16×16: all 16×16.
        assert_eq!(Fourcc::Uly4.plane_dim(2, 16, 16), (16, 16));
        // ULRG/ULRA all planes are full-size.
        assert_eq!(Fourcc::Ulra.plane_dim(3, 16, 16), (16, 16));
    }

    #[test]
    fn dim_constraints_match_spec() {
        assert!(Fourcc::Uly0.validate_dims(15, 16).is_err());
        assert!(Fourcc::Uly0.validate_dims(16, 15).is_err());
        assert!(Fourcc::Uly0.validate_dims(16, 16).is_ok());
        assert!(Fourcc::Uly2.validate_dims(15, 17).is_err());
        assert!(Fourcc::Uly2.validate_dims(16, 17).is_ok());
        assert!(Fourcc::Uly4.validate_dims(15, 15).is_ok());
        assert!(Fourcc::Ulrg.validate_dims(15, 15).is_ok());
        assert!(Fourcc::Ulra.validate_dims(15, 15).is_ok());
    }

    #[test]
    fn extradata_parse_ffmpeg_uly0_fixture() {
        // Per spec/01 §5: T1-uly0 extradata bytes.
        let raw = [
            0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        let ed = Extradata::parse(&raw).unwrap();
        assert_eq!(ed.encoder_version, 0x0100_00f0);
        assert_eq!(&ed.source_format_tag, b"YV12");
        assert_eq!(ed.frame_info_size, 4);
        assert_eq!(ed.flags, 0x0000_0001);
        assert_eq!(ed.num_slices(), 1);
    }

    #[test]
    fn extradata_slice_count_decoded() {
        // T6-uly0-slices4: flags = 0x03000001 -> 4 slices.
        let raw = [
            0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x03,
        ];
        let ed = Extradata::parse(&raw).unwrap();
        assert_eq!(ed.num_slices(), 4);
    }

    #[test]
    fn extradata_rejects_bad_frame_info_size() {
        let mut raw = [
            0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        raw[8] = 8; // frame_info_size = 8
        assert!(matches!(
            Extradata::parse(&raw),
            Err(Error::InvalidFrameInfoSize(8))
        ));
    }

    #[test]
    fn extradata_rejects_huffman_clear() {
        let raw = [
            0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        assert!(matches!(
            Extradata::parse(&raw),
            Err(Error::HuffmanBitClear)
        ));
    }

    #[test]
    fn extradata_rejects_interlaced() {
        let raw = [
            0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x08,
            0x00, 0x00,
        ];
        assert!(matches!(
            Extradata::parse(&raw),
            Err(Error::InterlacedNotSupported)
        ));
    }

    #[test]
    fn extradata_rejects_truncated() {
        assert!(matches!(
            Extradata::parse(&[0; 8]),
            Err(Error::ExtradataTruncated { len: 8 })
        ));
    }

    #[test]
    fn predictor_round_trip_via_frame_info() {
        for p in [
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            assert_eq!(Predictor::from_frame_info(p.as_frame_info_bits()), p);
        }
    }

    #[test]
    fn predictor_ignores_other_bits() {
        // Other bits in frame_info must be ignored per spec/02 §6.2.
        assert_eq!(Predictor::from_frame_info(0xffff_ffff), Predictor::Median);
        assert_eq!(Predictor::from_frame_info(0x1234_5101), Predictor::Left);
    }

    #[test]
    fn ffmpeg_source_format_tag_pinned_per_spec01_t1() {
        // spec/01 §2.2 + §5 test-set T1.
        assert_eq!(&Fourcc::Uly0.ffmpeg_source_format_tag(), b"YV12");
        assert_eq!(&Fourcc::Uly2.ffmpeg_source_format_tag(), b"YUY2");
        assert_eq!(&Fourcc::Uly4.ffmpeg_source_format_tag(), b"YV24");
        assert_eq!(
            Fourcc::Ulrg.ffmpeg_source_format_tag(),
            [0x00, 0x00, 0x01, 0x18]
        );
        assert_eq!(
            Fourcc::Ulra.ffmpeg_source_format_tag(),
            [0x00, 0x00, 0x02, 0x18]
        );
    }

    #[test]
    fn extradata_ffmpeg_for_builder_matches_spec01_t1_uly0() {
        // spec/01 §5 T1-uly0 reference extradata bytes (1 slice):
        // f0 00 00 01 59 56 31 32 04 00 00 00 01 00 00 00
        let ed = Extradata::ffmpeg_for(Fourcc::Uly0, 1).unwrap();
        assert_eq!(
            ed.to_bytes(),
            [
                0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x31, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
        assert_eq!(ed.num_slices(), 1);
    }

    #[test]
    fn extradata_ffmpeg_for_builder_matches_spec01_t1_uly2() {
        let ed = Extradata::ffmpeg_for(Fourcc::Uly2, 1).unwrap();
        assert_eq!(
            ed.to_bytes(),
            [
                0xf0, 0x00, 0x00, 0x01, 0x59, 0x55, 0x59, 0x32, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn extradata_ffmpeg_for_builder_matches_spec01_t1_uly4() {
        let ed = Extradata::ffmpeg_for(Fourcc::Uly4, 1).unwrap();
        assert_eq!(
            ed.to_bytes(),
            [
                0xf0, 0x00, 0x00, 0x01, 0x59, 0x56, 0x32, 0x34, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn extradata_ffmpeg_for_builder_matches_spec01_t1_ulrg() {
        let ed = Extradata::ffmpeg_for(Fourcc::Ulrg, 1).unwrap();
        assert_eq!(
            ed.to_bytes(),
            [
                0xf0, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x18, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn extradata_ffmpeg_for_builder_matches_spec01_t1_ulra() {
        let ed = Extradata::ffmpeg_for(Fourcc::Ulra, 1).unwrap();
        assert_eq!(
            ed.to_bytes(),
            [
                0xf0, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0x18, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn extradata_ffmpeg_for_encodes_slice_count_high_byte() {
        // spec/01 §4.4.3: high byte of `flags` encodes `num_slices - 1`.
        // Slice count 4 → flags top byte 0x03.
        let ed = Extradata::ffmpeg_for(Fourcc::Uly0, 4).unwrap();
        assert_eq!(ed.num_slices(), 4);
        assert_eq!((ed.flags >> 24) & 0xff, 0x03);

        // Slice count 256 → flags top byte 0xff (the maximum).
        let ed = Extradata::ffmpeg_for(Fourcc::Uly4, 256).unwrap();
        assert_eq!(ed.num_slices(), 256);
        assert_eq!((ed.flags >> 24) & 0xff, 0xff);
    }

    #[test]
    fn extradata_ffmpeg_for_rejects_out_of_range_slices() {
        assert!(matches!(
            Extradata::ffmpeg_for(Fourcc::Uly0, 0),
            Err(Error::InvalidSliceCount)
        ));
        assert!(matches!(
            Extradata::ffmpeg_for(Fourcc::Uly0, 257),
            Err(Error::InvalidSliceCount)
        ));
    }

    #[test]
    fn extradata_ffmpeg_for_round_trips_through_parse() {
        // Building via the new helper then re-parsing must reproduce
        // an equal Extradata for every FOURCC at slice counts 1, 16, 256.
        for &fc in &[
            Fourcc::Uly0,
            Fourcc::Uly2,
            Fourcc::Uly4,
            Fourcc::Ulrg,
            Fourcc::Ulra,
        ] {
            for &slices in &[1usize, 16, 256] {
                let ed = Extradata::ffmpeg_for(fc, slices).unwrap();
                let bytes = ed.to_bytes();
                let parsed = Extradata::parse(&bytes).unwrap();
                assert_eq!(parsed, ed, "round-trip fc={fc:?} slices={slices}");
            }
        }
    }
}
