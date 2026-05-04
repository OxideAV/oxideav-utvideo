//! Extradata parser for the three Ut Video families.
//!
//! Per the trace report (§3.2), classic-family extradata is 16 bytes:
//!
//! ```text
//! word0 LE32 = version
//! word1 BE32 = original_format (informational; not used by decoder)
//! word2 LE32 = frame_info_size (always 4 in our corpus)
//! word3 LE32 = flags
//!     bit 0       = compression (1 = COMP_HUFF)
//!     bit 11 0x800 = interlaced
//!     bits 24..31 = slices_minus_one  →  slices = (flags >> 24) + 1
//! ```
//!
//! Pro family is 8 bytes (`version + original_format`); slice count and
//! interlaced flag move into the per-frame header.
//!
//! Pack family is 16 bytes with `compression == 2` plus a slices-minus-one
//! byte; the rest is unused/version padding.

use oxideav_core::{Error, Result};

use crate::fourcc::{Family, FourCc, PlaneShape};

/// Per-frame layout flags from the classic-family extradata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    pub compression: bool,
    pub interlaced: bool,
    pub slices: u32,
}

#[derive(Debug, Clone)]
pub struct ExtraData {
    pub fourcc: FourCc,
    pub family: Family,
    pub shape: PlaneShape,
    pub version: u32,
    pub original_format: u32,
    pub frame_info_size: u32,
    pub flags: Flags,
}

impl ExtraData {
    pub fn parse(fourcc: FourCc, raw: &[u8]) -> Result<ExtraData> {
        let shape = PlaneShape::from_fourcc(fourcc)?;
        match shape.family {
            Family::Classic => parse_classic(fourcc, shape, raw),
            Family::Pro => parse_pro(fourcc, shape, raw),
            Family::Pack => parse_pack(fourcc, shape, raw),
        }
    }
}

fn parse_classic(fourcc: FourCc, shape: PlaneShape, raw: &[u8]) -> Result<ExtraData> {
    if raw.len() < 16 {
        return Err(Error::invalid(format!(
            "Ut Video {}: extradata is {} bytes, classic family needs 16",
            fourcc.as_str(),
            raw.len()
        )));
    }
    let version = read_u32_le(raw, 0);
    let original_format = read_u32_be(raw, 4);
    let frame_info_size = read_u32_le(raw, 8);
    let flags_word = read_u32_le(raw, 12);
    let flags = Flags {
        compression: (flags_word & 1) != 0,
        interlaced: (flags_word & 0x800) != 0,
        slices: ((flags_word >> 24) & 0xFF) + 1,
    };
    if !flags.compression {
        return Err(Error::unsupported(
            "Ut Video: extradata compression bit is 0 (raw / non-Huffman classic frames)",
        ));
    }
    if frame_info_size != 4 {
        return Err(Error::unsupported(format!(
            "Ut Video: frame_info_size = {frame_info_size} (only 4 supported)"
        )));
    }
    Ok(ExtraData {
        fourcc,
        family: shape.family,
        shape,
        version,
        original_format,
        frame_info_size,
        flags,
    })
}

fn parse_pro(fourcc: FourCc, shape: PlaneShape, raw: &[u8]) -> Result<ExtraData> {
    if raw.len() < 8 {
        return Err(Error::invalid(format!(
            "Ut Video {}: extradata is {} bytes, pro family needs 8",
            fourcc.as_str(),
            raw.len()
        )));
    }
    let version = read_u32_le(raw, 0);
    let original_format = read_u32_be(raw, 4);
    Ok(ExtraData {
        fourcc,
        family: shape.family,
        shape,
        version,
        original_format,
        // Pro family carries these per-frame (see trace report §6).
        frame_info_size: 0,
        flags: Flags {
            compression: true,
            interlaced: false,
            slices: 0,
        },
    })
}

fn parse_pack(fourcc: FourCc, shape: PlaneShape, raw: &[u8]) -> Result<ExtraData> {
    if raw.len() < 16 {
        return Err(Error::invalid(format!(
            "Ut Video {}: extradata is {} bytes, pack family needs 16",
            fourcc.as_str(),
            raw.len()
        )));
    }
    let version = read_u32_le(raw, 0);
    let original_format = read_u32_be(raw, 4);
    let comp = raw[8];
    let slices_m1 = raw[9];
    if comp != 2 {
        return Err(Error::invalid(format!(
            "Ut Video pack: compression = {comp}, expected 2"
        )));
    }
    Ok(ExtraData {
        fourcc,
        family: shape.family,
        shape,
        version,
        original_format,
        frame_info_size: 0,
        flags: Flags {
            compression: true,
            interlaced: false,
            slices: slices_m1 as u32 + 1,
        },
    })
}

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn read_u32_be(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lavc_ulrg_left() {
        // Same blob captured in trace report §3.2.1.
        let raw = [
            0xF0, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x18, 0x04, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x01,
        ];
        let xd = ExtraData::parse(FourCc(*b"ULRG"), &raw).unwrap();
        assert!(xd.flags.compression);
        assert!(!xd.flags.interlaced);
        assert_eq!(xd.flags.slices, 2);
        assert_eq!(xd.frame_info_size, 4);
    }

    #[test]
    fn parses_official_interlaced_gradient() {
        // Trace report §3.2.2 sample.
        let raw = [
            0x01, 0x01, 0x02, 0x12, 0x00, 0x00, 0x06, 0x18, 0x04, 0x00, 0x00, 0x00, 0x01, 0x08,
            0x00, 0x03,
        ];
        let xd = ExtraData::parse(FourCc(*b"ULRG"), &raw).unwrap();
        assert!(xd.flags.compression);
        assert!(xd.flags.interlaced);
        assert_eq!(xd.flags.slices, 4);
    }

    #[test]
    fn rejects_short_classic_extradata() {
        assert!(ExtraData::parse(FourCc(*b"ULRG"), &[0; 8]).is_err());
    }

    #[test]
    fn parses_pro_extradata_8_bytes() {
        // Trace doc §3.2.4: pro family carries only version +
        // original_format. Slice count and predictor live in the
        // per-frame header.
        let raw = [
            0x01, 0x01, 0x02, 0x12, // version (LE32)
            0x59, 0x55, 0x59, 0x32, // original_format BE32 == "YUY2"
        ];
        let xd = ExtraData::parse(FourCc(*b"UQY2"), &raw).unwrap();
        assert_eq!(xd.family, Family::Pro);
        assert_eq!(xd.shape.bit_depth, 10);
        assert_eq!(xd.frame_info_size, 0);
        // Pro family does not advertise slices in extradata (== 0
        // sentinel; per-frame header carries the real value).
        assert_eq!(xd.flags.slices, 0);
        assert!(!xd.flags.interlaced);
    }

    #[test]
    fn rejects_short_pro_extradata() {
        // Pro family needs 8 bytes minimum.
        assert!(ExtraData::parse(FourCc(*b"UQRG"), &[0; 4]).is_err());
    }

    #[test]
    fn parses_pack_extradata_with_compression_2() {
        // Trace doc §7: pack extradata is 16 bytes with compression
        // = 2 at offset 8 and slices_minus_one at offset 9.
        let raw = [
            0x01, 0x01, 0x02, 0x12, // version
            0x59, 0x55, 0x59, 0x32, // original_format BE32
            0x02, // compression == COMP_PACK (2)
            0x03, // slices_minus_one == 3 → slices = 4
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let xd = ExtraData::parse(FourCc(*b"UMY2"), &raw).unwrap();
        assert_eq!(xd.family, Family::Pack);
        assert_eq!(xd.flags.slices, 4);
    }

    #[test]
    fn rejects_pack_extradata_with_wrong_compression() {
        // compression != 2 must fail.
        let mut raw = [0u8; 16];
        raw[8] = 1;
        assert!(ExtraData::parse(FourCc(*b"UMRG"), &raw).is_err());
    }
}
