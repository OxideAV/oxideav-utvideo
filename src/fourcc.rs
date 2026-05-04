//! Ut Video FourCC catalogue. Each FourCC fully determines the family
//! (classic UL / pro UQ / pack UM), the plane count, the chroma
//! subsampling, and whether an alpha plane is carried. Colourspace
//! (BT.601 vs BT.709) is also encoded in the FourCC — `ULY*` and `ULH*`
//! are bitwise identical streams that only differ in the matrix the
//! decoder hands to its consumer.

use oxideav_core::{Error, PixelFormat, Result};

/// 4-character ASCII tag (e.g. `b"ULRG"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub [u8; 4]);

impl FourCc {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("????")
    }
}

/// Decoder family selected by FourCC. Determines extradata layout,
/// per-frame header location, and entropy primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// 8-bit canonical-Huffman + predictor (`UL*`).
    Classic,
    /// 10-bit canonical-Huffman, header order swapped (`UQ*`).
    Pro,
    /// SymPack two-stream "block-of-8 raw bits" coder (`UM*`).
    Pack,
}

/// Plane shape selected by FourCC: chroma subsampling + alpha + RGB
/// vs YUV. Plane count = 3 (Y/U/V or G/B/R) or 4 (with alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneShape {
    pub family: Family,
    /// Logical pixel format produced by the decoder.
    pub pixel_format: PixelFormat,
    /// Per-plane horizontal subsampling (1 = full, 2 = half).
    pub h_subsample: [u8; 4],
    /// Per-plane vertical subsampling (1 = full, 2 = half).
    pub v_subsample: [u8; 4],
    /// Number of planes (3 or 4).
    pub planes: u8,
    /// True for RGB FourCCs (G plane runs first; B/R are stored as
    /// G-centred residuals on disk; alpha — when present — is plane 3).
    pub is_rgb: bool,
    /// True if a fourth plane carries alpha.
    pub has_alpha: bool,
    /// Number of bits per sample (8 for classic, 10 for pro).
    pub bit_depth: u8,
}

impl PlaneShape {
    /// Resolve the FourCC into its decoder shape.
    ///
    /// Returns [`Error::Unsupported`] for the pro and pack families
    /// (parsed but not yet decoded by this crate) and for completely
    /// unknown FourCCs.
    pub fn from_fourcc(fourcc: FourCc) -> Result<PlaneShape> {
        match &fourcc.0 {
            // --- Classic UL (8-bit) ---
            b"ULRG" => Ok(rgb_shape(Family::Classic, false, 8)),
            b"ULRA" => Ok(rgb_shape(Family::Classic, true, 8)),
            b"ULY0" | b"ULH0" => Ok(yuv_shape(
                Family::Classic,
                PixelFormat::Yuv420P,
                [1, 2, 2, 1],
                [1, 2, 2, 1],
                8,
            )),
            b"ULY2" | b"ULH2" => Ok(yuv_shape(
                Family::Classic,
                PixelFormat::Yuv422P,
                [1, 2, 2, 1],
                [1, 1, 1, 1],
                8,
            )),
            b"ULY4" | b"ULH4" => Ok(yuv_shape(
                Family::Classic,
                PixelFormat::Yuv444P,
                [1, 1, 1, 1],
                [1, 1, 1, 1],
                8,
            )),
            // --- Pro UQ (10-bit) ---
            // No `PixelFormat::Gbrp10*` variant exists in oxideav-core
            // today, so RGB-like UQ tags fall back to `Rgb48Le` as the
            // closest packed-16-bit-per-channel placeholder; a future
            // `Gbrp10Le`/`Gbrap10Le` PixelFormat addition can refine
            // this without a behavioural change.
            b"UQRG" => Ok(rgb_shape(Family::Pro, false, 10)),
            b"UQRA" => Ok(rgb_shape(Family::Pro, true, 10)),
            b"UQY0" => Ok(yuv_shape(
                Family::Pro,
                PixelFormat::Yuv420P10Le,
                [1, 2, 2, 1],
                [1, 2, 2, 1],
                10,
            )),
            b"UQY2" => Ok(yuv_shape(
                Family::Pro,
                PixelFormat::Yuv422P10Le,
                [1, 2, 2, 1],
                [1, 1, 1, 1],
                10,
            )),
            // --- Pack UM (SymPack, 8-bit) ---
            // Per trace doc §7 / §12.1, no UMY0/UMH0 exists.
            b"UMRG" => Ok(rgb_shape(Family::Pack, false, 8)),
            b"UMRA" => Ok(rgb_shape(Family::Pack, true, 8)),
            b"UMY2" | b"UMH2" => Ok(yuv_shape(
                Family::Pack,
                PixelFormat::Yuv422P,
                [1, 2, 2, 1],
                [1, 1, 1, 1],
                8,
            )),
            b"UMY4" | b"UMH4" => Ok(yuv_shape(
                Family::Pack,
                PixelFormat::Yuv444P,
                [1, 1, 1, 1],
                [1, 1, 1, 1],
                8,
            )),
            other => Err(Error::invalid(format!(
                "Ut Video: unknown FourCC {:?}",
                std::str::from_utf8(other).unwrap_or("????")
            ))),
        }
    }

    /// Returns [`Family::Classic`] / [`Family::Pro`] / [`Family::Pack`].
    pub fn family(&self) -> Family {
        self.family
    }
}

fn rgb_shape(family: Family, alpha: bool, bit_depth: u8) -> PlaneShape {
    // 8-bit RGB FourCCs report Rgb24 / Rgba; 10-bit (UQ) FourCCs fall
    // back to Rgb48Le / Rgba64Le as the closest packed-16-bit-per-
    // channel `PixelFormat` variants.
    let pixel_format = match (alpha, bit_depth) {
        (false, 8) => PixelFormat::Rgb24,
        (true, 8) => PixelFormat::Rgba,
        (false, 10) => PixelFormat::Rgb48Le,
        (true, 10) => PixelFormat::Rgba64Le,
        _ => PixelFormat::Rgb24, // unreachable in current FourCC catalogue
    };
    PlaneShape {
        family,
        pixel_format,
        h_subsample: [1, 1, 1, 1],
        v_subsample: [1, 1, 1, 1],
        planes: if alpha { 4 } else { 3 },
        is_rgb: true,
        has_alpha: alpha,
        bit_depth,
    }
}

fn yuv_shape(
    family: Family,
    pixel_format: PixelFormat,
    h: [u8; 4],
    v: [u8; 4],
    bit_depth: u8,
) -> PlaneShape {
    PlaneShape {
        family,
        pixel_format,
        h_subsample: h,
        v_subsample: v,
        planes: 3,
        is_rgb: false,
        has_alpha: false,
        bit_depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_yuv422_shape() {
        let s = PlaneShape::from_fourcc(FourCc(*b"ULY2")).unwrap();
        assert_eq!(s.planes, 3);
        assert_eq!(s.h_subsample, [1, 2, 2, 1]);
        assert_eq!(s.v_subsample, [1, 1, 1, 1]);
        assert!(!s.is_rgb);
    }

    #[test]
    fn classic_rgb_alpha_shape() {
        let s = PlaneShape::from_fourcc(FourCc(*b"ULRA")).unwrap();
        assert_eq!(s.planes, 4);
        assert!(s.is_rgb);
        assert!(s.has_alpha);
    }

    #[test]
    fn unknown_fourcc_rejected() {
        assert!(PlaneShape::from_fourcc(FourCc(*b"XXXX")).is_err());
    }

    #[test]
    fn pro_uqy2_shape() {
        let s = PlaneShape::from_fourcc(FourCc(*b"UQY2")).unwrap();
        assert_eq!(s.family, Family::Pro);
        assert_eq!(s.bit_depth, 10);
        assert_eq!(s.planes, 3);
        assert_eq!(s.h_subsample, [1, 2, 2, 1]);
        assert_eq!(s.v_subsample, [1, 1, 1, 1]);
        assert!(!s.is_rgb);
        assert_eq!(s.pixel_format, PixelFormat::Yuv422P10Le);
    }

    #[test]
    fn pro_uqra_shape_carries_alpha() {
        let s = PlaneShape::from_fourcc(FourCc(*b"UQRA")).unwrap();
        assert_eq!(s.family, Family::Pro);
        assert_eq!(s.bit_depth, 10);
        assert_eq!(s.planes, 4);
        assert!(s.is_rgb);
        assert!(s.has_alpha);
    }

    #[test]
    fn pack_umy4_shape() {
        let s = PlaneShape::from_fourcc(FourCc(*b"UMY4")).unwrap();
        assert_eq!(s.family, Family::Pack);
        assert_eq!(s.bit_depth, 8);
        assert_eq!(s.planes, 3);
        assert_eq!(s.h_subsample, [1, 1, 1, 1]);
        assert_eq!(s.pixel_format, PixelFormat::Yuv444P);
    }

    #[test]
    fn pack_umh2_alias_of_umy2() {
        // UMH2 (BT.709) is a colourspace-only twin of UMY2 (BT.601);
        // the bitstream shape is identical.
        let a = PlaneShape::from_fourcc(FourCc(*b"UMY2")).unwrap();
        let b = PlaneShape::from_fourcc(FourCc(*b"UMH2")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn no_umy0_or_umh0() {
        // Per trace doc §7 + §12.1: SymPack does not have a 4:2:0
        // variant.
        assert!(PlaneShape::from_fourcc(FourCc(*b"UMY0")).is_err());
        assert!(PlaneShape::from_fourcc(FourCc(*b"UMH0")).is_err());
    }
}
