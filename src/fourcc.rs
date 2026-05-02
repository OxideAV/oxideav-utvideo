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
            // Classic 8-bit.
            b"ULRG" => Ok(rgb_shape(false)),
            b"ULRA" => Ok(rgb_shape(true)),
            b"ULY0" | b"ULH0" => Ok(yuv_shape(PixelFormat::Yuv420P, [1, 2, 2, 1], [1, 2, 2, 1])),
            b"ULY2" | b"ULH2" => Ok(yuv_shape(PixelFormat::Yuv422P, [1, 2, 2, 1], [1, 1, 1, 1])),
            b"ULY4" | b"ULH4" => Ok(yuv_shape(PixelFormat::Yuv444P, [1, 1, 1, 1], [1, 1, 1, 1])),
            // Pro 10-bit (extradata layout differs; decode NYI).
            b"UQRG" | b"UQRA" | b"UQY0" | b"UQY2" => Err(Error::unsupported(format!(
                "Ut Video pro family ({}) not yet implemented",
                fourcc.as_str()
            ))),
            // SymPack (no Huffman; decode NYI).
            b"UMRG" | b"UMRA" | b"UMY2" | b"UMH2" | b"UMY4" | b"UMH4" => {
                Err(Error::unsupported(format!(
                    "Ut Video pack family ({}) not yet implemented",
                    fourcc.as_str()
                )))
            }
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

fn rgb_shape(alpha: bool) -> PlaneShape {
    PlaneShape {
        family: Family::Classic,
        pixel_format: if alpha {
            PixelFormat::Rgba
        } else {
            PixelFormat::Rgb24
        },
        h_subsample: [1, 1, 1, 1],
        v_subsample: [1, 1, 1, 1],
        planes: if alpha { 4 } else { 3 },
        is_rgb: true,
        has_alpha: alpha,
        bit_depth: 8,
    }
}

fn yuv_shape(pixel_format: PixelFormat, h: [u8; 4], v: [u8; 4]) -> PlaneShape {
    PlaneShape {
        family: Family::Classic,
        pixel_format,
        h_subsample: h,
        v_subsample: v,
        planes: 3,
        is_rgb: false,
        has_alpha: false,
        bit_depth: 8,
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
}
