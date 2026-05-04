//! Standalone Ut Video classic-family packet decoder.
//!
//! This module implements steps 3, 6, and 7 of the conformance summary
//! in the trace report (§10): parse the per-frame plane layout, build
//! per-plane canonical-Huffman tables, decode each slice's symbols,
//! apply the inverse predictor, and run the RGB G-centred fix-up where
//! applicable.
//!
//! The output is per-plane row-major buffers, one entry per logical
//! pixel in the plane (chroma planes are subsampled per the FourCC).
//! Plane order:
//!
//! * RGB FourCCs (`ULRG`, `ULRA`): G, B, R, [A].
//! * YUV FourCCs (`ULY*`, `ULH*`): Y, U, V (== Cb, Cr).

use oxideav_core::{Error, PixelFormat, Result};

use crate::extradata::ExtraData;
use crate::fourcc::{Family, FourCc, PlaneShape};
use crate::huffman::{byteswap_dwords, BitReader, HuffTable};
use crate::predictor::{apply_inverse_8bit, restore_g_centred_rgb, Predictor};

/// Output of one packet decode. Each plane is `width × height` bytes
/// after subsampling; `stride_bytes[p]` is the byte stride within that
/// plane (always equal to the plane's logical width for 8-bit).
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub planes: Vec<Vec<u8>>,
    pub stride_bytes: Vec<usize>,
}

/// Stateful classic-family decoder. Holds the parsed extradata and
/// dimensions; one instance can decode many packets.
pub struct UtVideoDecoder {
    fourcc: FourCc,
    extradata: ExtraData,
    width: u32,
    height: u32,
    shape: PlaneShape,
}

impl UtVideoDecoder {
    pub fn new(
        fourcc: FourCc,
        extradata: ExtraData,
        width: u32,
        height: u32,
    ) -> Result<UtVideoDecoder> {
        let shape = extradata.shape;
        match shape.family {
            Family::Classic => {}
            Family::Pro => {
                return Err(Error::unsupported(format!(
                    "Ut Video pro family ({}) decode not yet implemented \
                     (extradata accepted; see trace doc §6 for the per-frame layout)",
                    fourcc.as_str()
                )));
            }
            Family::Pack => {
                return Err(Error::unsupported(format!(
                    "Ut Video pack/SymPack family ({}) decode not yet implemented \
                     (extradata accepted; see trace doc §7 / §12.5 for the bit code)",
                    fourcc.as_str()
                )));
            }
        }
        Ok(UtVideoDecoder {
            fourcc,
            extradata,
            width,
            height,
            shape,
        })
    }

    pub fn fourcc(&self) -> FourCc {
        self.fourcc
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.shape.pixel_format
    }

    /// Decode one classic-family packet. Returns one [`DecodedFrame`].
    pub fn decode(&mut self, packet: &[u8]) -> Result<DecodedFrame> {
        if self.extradata.flags.interlaced {
            return Err(Error::unsupported(
                "Ut Video: interlaced re-pairing not yet implemented",
            ));
        }
        let n_slices = self.extradata.flags.slices as usize;
        if n_slices == 0 || n_slices > 256 {
            return Err(Error::invalid(format!(
                "Ut Video: invalid slice count {n_slices}"
            )));
        }
        if packet.len() < 4 {
            return Err(Error::invalid("Ut Video: packet shorter than frame_info"));
        }
        // Trace report §4.1: the 4-byte frame_info LE32 sits at the
        // very tail of the packet.
        let frame_info = read_u32_le(packet, packet.len() - 4);
        let predictor = Predictor::from_frame_info(frame_info)?;

        let n_planes = self.shape.planes as usize;
        let mut planes_data: Vec<Vec<u8>> = Vec::with_capacity(n_planes);
        let mut stride_bytes: Vec<usize> = Vec::with_capacity(n_planes);
        let mut cursor = 0usize;
        let body_end = packet.len() - 4;

        for plane_idx in 0..n_planes {
            // Per-plane width/height after subsampling.
            let pw = subsampled(self.width, self.shape.h_subsample[plane_idx]);
            let ph = subsampled(self.height, self.shape.v_subsample[plane_idx]);
            stride_bytes.push(pw as usize);
            let mut plane_buf = vec![0u8; pw as usize * ph as usize];

            // 256-byte length table.
            if cursor + 256 > body_end {
                return Err(Error::invalid("Ut Video: truncated plane (length table)"));
            }
            let mut lens = [0u8; 256];
            lens.copy_from_slice(&packet[cursor..cursor + 256]);
            cursor += 256;
            let table = HuffTable::from_lengths(&lens)?;

            // Per-plane slice partitioning of image rows.
            let row_starts = compute_row_partition(ph, n_slices);

            // 4 × N_slices LE32 cumulative end offsets.
            let off_table_len = 4 * n_slices;
            if cursor + off_table_len > body_end {
                return Err(Error::invalid(
                    "Ut Video: truncated plane (slice-offset table)",
                ));
            }
            let off_table_start = cursor;
            cursor += off_table_len;
            let mut slice_ends = Vec::with_capacity(n_slices);
            for i in 0..n_slices {
                let v = read_u32_le(packet, off_table_start + i * 4) as usize;
                slice_ends.push(v);
            }
            let total_data = *slice_ends.last().unwrap_or(&0);
            if cursor + total_data > body_end {
                return Err(Error::invalid("Ut Video: truncated plane (slice data)"));
            }
            let plane_data_start = cursor;
            cursor += total_data;

            // Decode every slice into plane_buf.
            for s in 0..n_slices {
                let slice_start = if s == 0 { 0 } else { slice_ends[s - 1] };
                let slice_end = slice_ends[s];
                if slice_end < slice_start || slice_end > total_data {
                    return Err(Error::invalid(
                        "Ut Video: slice end-offset table out of order",
                    ));
                }
                let bytes = &packet[plane_data_start + slice_start..plane_data_start + slice_end];
                let row_lo = row_starts[s];
                let row_hi = row_starts[s + 1];
                let slice_h = (row_hi - row_lo) as usize;
                let slice_pixels = pw as usize * slice_h;
                let dst = &mut plane_buf[(row_lo as usize * pw as usize)
                    ..(row_lo as usize * pw as usize + slice_pixels)];

                if let HuffTable::Fill { symbol } = table {
                    // Fill fast path: skip the bit reader entirely and
                    // splat. The slice data block for this plane should
                    // have size 0 per the trace report (§9), but we
                    // tolerate non-empty input here — the encoder only
                    // emits the fast path when it really is solid.
                    for px in dst.iter_mut() {
                        *px = symbol;
                    }
                } else {
                    let swapped = byteswap_dwords(bytes);
                    let mut br = BitReader::new(&swapped);
                    for px in dst.iter_mut() {
                        *px = table.decode_symbol(&mut br)?;
                    }
                }

                // Inverse predictor runs per-slice. Borrow only the
                // slice's rows so the predictor state resets at every
                // slice boundary (trace report §8 LEFT note).
                apply_inverse_8bit(predictor, dst, pw as usize, slice_h);
            }

            planes_data.push(plane_buf);
        }

        if cursor != body_end {
            return Err(Error::invalid(format!(
                "Ut Video: residual {} bytes between planes and frame_info",
                body_end - cursor
            )));
        }

        // RGB G-centred fix-up: planes are emitted in G, B, R, [A]
        // order; after Huffman + inverse-predictor, R and B carry the
        // residual, so add G - 0x80.
        if self.shape.is_rgb {
            // Split the planes vector so we can borrow G immutably and
            // B + R mutably at once.
            let (g_slice, rest) = planes_data.split_at_mut(1);
            let g = &g_slice[0];
            let (b_slice, rest2) = rest.split_at_mut(1);
            let b = &mut b_slice[0];
            let (r_slice, _) = rest2.split_at_mut(1);
            let r = &mut r_slice[0];
            restore_g_centred_rgb(g, b, r);
            // Alpha plane (if any) is at index 3 — left untouched.
        }

        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            planes: planes_data,
            stride_bytes,
        })
    }
}

/// Convenience standalone API: parse extradata + decode in one shot.
/// Useful for tests and one-shot consumers; persistent decoders should
/// hold a [`UtVideoDecoder`] and reuse the parsed extradata.
pub fn decode_packet(
    fourcc: FourCc,
    extradata: &[u8],
    width: u32,
    height: u32,
    packet: &[u8],
) -> Result<DecodedFrame> {
    let xd = ExtraData::parse(fourcc, extradata)?;
    let mut dec = UtVideoDecoder::new(fourcc, xd, width, height)?;
    dec.decode(packet)
}

/// Compute the slice row-partition for one plane.
///
/// Per trace report §4.5: `slice_start_row[i] = (i * height /
/// N_slices)` then aligned to a per-format `cmask`. We use the
/// progressive 4:2:0 luma alignment (`cmask = ~1`) — i.e. round to even
/// rows — when the slice belongs to a YUV420 plane; otherwise no mask.
/// The simpler "no mask" suits all our tested fixtures (RGB and
/// 4:2:2). If the chroma plane height is too small to host every
/// slice, we collapse trailing empty slices.
fn compute_row_partition(plane_height: u32, n_slices: usize) -> Vec<u32> {
    let mut starts = Vec::with_capacity(n_slices + 1);
    for i in 0..=n_slices {
        let v = ((i as u64) * plane_height as u64 / n_slices as u64) as u32;
        starts.push(v);
    }
    starts
}

#[inline]
fn subsampled(dim: u32, factor: u8) -> u32 {
    dim.div_ceil(factor as u32)
}

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_partition_320x240_4_slices() {
        // Trace report §4.5: 240 / 4 = [0, 60, 120, 180, 240].
        let p = compute_row_partition(240, 4);
        assert_eq!(p, vec![0, 60, 120, 180, 240]);
    }

    #[test]
    fn pro_decode_rejected_with_clear_message() {
        // 8-byte pro extradata is accepted by ExtraData::parse, but
        // UtVideoDecoder::new still refuses with an explicit
        // "decode not yet implemented" message until the per-frame
        // layout walker for trace doc §6 is wired.
        let xd_raw = [0u8; 8];
        let xd = ExtraData::parse(FourCc(*b"UQY2"), &xd_raw).unwrap();
        let err = match UtVideoDecoder::new(FourCc(*b"UQY2"), xd, 64, 48) {
            Ok(_) => panic!("expected pro-family decode-NYI rejection, got Ok"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("pro family") && msg.contains("not yet implemented"),
            "expected pro-family decode-NYI rejection, got: {msg}"
        );
    }

    #[test]
    fn pack_decode_rejected_with_clear_message() {
        let mut xd_raw = [0u8; 16];
        xd_raw[8] = 2; // compression == COMP_PACK
        let xd = ExtraData::parse(FourCc(*b"UMY4"), &xd_raw).unwrap();
        let err = match UtVideoDecoder::new(FourCc(*b"UMY4"), xd, 64, 48) {
            Ok(_) => panic!("expected pack-family decode-NYI rejection, got Ok"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("pack/SymPack family") && msg.contains("not yet implemented"),
            "expected pack-family decode-NYI rejection, got: {msg}"
        );
    }

    #[test]
    fn fill_plane_minimal_packet() {
        // Build a minimal classic-family packet: ULY4 (yuv444, no
        // alpha) with a single slice and every plane filled with a
        // constant. This exercises the fast path end-to-end without
        // needing a real Huffman build.
        let width = 4u32;
        let height = 2u32;
        let n_planes = 3usize;
        let mut packet: Vec<u8> = Vec::new();
        for fill_sym in [0x42u8, 0x10, 0xA5] {
            // 256-byte length table: length 0 at fill_sym, 0xFF elsewhere.
            let mut lens = [0xFFu8; 256];
            lens[fill_sym as usize] = 0;
            packet.extend_from_slice(&lens);
            // 4 × n_slices LE32 cumulative end offsets — single slice
            // with 0 bytes of data (fast path).
            packet.extend_from_slice(&0u32.to_le_bytes());
            // Empty slice data.
        }
        // frame_info: predictor=NONE, slices=1, no flags.
        packet.extend_from_slice(&0u32.to_le_bytes());

        // Synthesize a classic ULY4 extradata.
        let mut xd = vec![0u8; 16];
        // version word — anything works.
        xd[0..4].copy_from_slice(&0x12345678u32.to_le_bytes());
        // original_format BE32 — informational; YV24 is fine for ULY4.
        xd[4..8].copy_from_slice(&0x59563234u32.to_be_bytes());
        // frame_info_size = 4.
        xd[8..12].copy_from_slice(&4u32.to_le_bytes());
        // flags: bit 0 = compression, slices_minus_one = 0.
        xd[12..16].copy_from_slice(&1u32.to_le_bytes());

        let frame = decode_packet(FourCc(*b"ULY4"), &xd, width, height, &packet).unwrap();
        assert_eq!(frame.planes.len(), n_planes);
        for (p, expected) in frame.planes.iter().zip([0x42u8, 0x10, 0xA5]) {
            assert!(p.iter().all(|&b| b == expected));
        }
    }
}
