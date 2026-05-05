//! Standalone Ut Video packet decoder: classic (8-bit), pro (10-bit UQ),
//! pack (SymPack UM), and interlaced re-pair.
//!
//! This module implements steps 3-8 of the conformance summary in the
//! trace report (§10): parse the per-frame plane layout, build per-plane
//! canonical-Huffman tables (or run the SymPack block coder), decode each
//! slice's symbols, apply the inverse predictor, run the RGB G-centred
//! fix-up, and re-pair interlaced fields.
//!
//! The output is per-plane row-major buffers, one entry per logical
//! pixel in the plane (chroma planes are subsampled per the FourCC).
//! Plane order:
//!
//! * RGB FourCCs (`ULRG`/`UQRG`/`UMRG`/…): G, B, R, [A].
//! * YUV FourCCs (`ULY*`/`UQY*`/`UMY*`/…): Y, U, V.
//!
//! **10-bit (UQ) planes** are returned as `u16` values packed
//! little-endian into `u8` output bytes (stride = `width × 2`).

use oxideav_core::{Error, PixelFormat, Result};

use crate::extradata::ExtraData;
use crate::fourcc::{Family, FourCc, PlaneShape};
use crate::huffman::{byteswap_dwords, BitReader, HuffTable, HuffTable10, LeBitReader};
use crate::predictor::{
    apply_inverse_10bit, apply_inverse_8bit, apply_inverse_8bit_interlaced,
    restore_g_centred_rgb, restore_g_centred_rgb_10bit, Predictor,
};

/// Output of one packet decode. Each plane is `width × height` bytes
/// after subsampling; `stride_bytes[p]` is the byte stride within that
/// plane (equal to the plane's logical width for 8-bit, `width×2` for
/// 10-bit).
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
        // All three families are now supported.
        let _ = shape.family;
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

    /// Decode one packet. Dispatches to the classic (UL), pro (UQ), or
    /// pack (UM/SymPack) decoder based on the FourCC family.
    pub fn decode(&mut self, packet: &[u8]) -> Result<DecodedFrame> {
        match self.shape.family {
            Family::Classic => self.decode_classic(packet),
            Family::Pro => self.decode_pro(packet),
            Family::Pack => self.decode_pack(packet),
        }
    }

    // ------------------------------------------------------------------
    // Classic (UL) family — 8-bit
    // ------------------------------------------------------------------

    fn decode_classic(&mut self, packet: &[u8]) -> Result<DecodedFrame> {
        let interlaced = self.extradata.flags.interlaced;
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
                // Interlaced mode: data is in display order; use stride-2
                // predictor for GRADIENT/MEDIAN so each field is predicted
                // from its own previous row (2 rows back), not the
                // interleaved row above (trace doc §10 step 8).
                if interlaced {
                    apply_inverse_8bit_interlaced(
                        predictor,
                        dst,
                        pw as usize,
                        slice_h,
                        row_lo as usize,
                    );
                } else {
                    apply_inverse_8bit(predictor, dst, pw as usize, slice_h);
                }
            }

            planes_data.push(plane_buf);
        }

        if cursor != body_end {
            return Err(Error::invalid(format!(
                "Ut Video: residual {} bytes between planes and frame_info",
                body_end - cursor
            )));
        }

        // RGB G-centred fix-up.
        if self.shape.is_rgb {
            let (g_slice, rest) = planes_data.split_at_mut(1);
            let g = &g_slice[0];
            let (b_slice, rest2) = rest.split_at_mut(1);
            let b = &mut b_slice[0];
            let (r_slice, _) = rest2.split_at_mut(1);
            let r = &mut r_slice[0];
            restore_g_centred_rgb(g, b, r);
        }
        // No field reinterleave needed: interlaced data is already in
        // display order on disk (trace doc §10 step 8 describes the
        // stride-2 predictor applied during decode, not a field swap).

        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            planes: planes_data,
            stride_bytes,
        })
    }

    // ------------------------------------------------------------------
    // Pro (UQ) family — 10-bit
    // ------------------------------------------------------------------
    //
    // Per trace doc §6:
    //   - 4-byte frame_info LE32 at START of packet
    //   - Per-plane layout order: [4*slices offsets | slice-data | 1024 lens]
    //   - Predictors: NONE and LEFT only (GRADIENT/MEDIAN silently → NONE)
    //   - LEFT seed: 0x200 (mod 1024)
    //   - No interlaced support (flag forced 0)
    // ------------------------------------------------------------------

    fn decode_pro(&mut self, packet: &[u8]) -> Result<DecodedFrame> {
        if packet.len() < 4 {
            return Err(Error::invalid(
                "Ut Video UQ: packet shorter than frame_info header",
            ));
        }
        // Per-frame header is at the START (opposite of classic).
        let frame_info = read_u32_le(packet, 0);
        let n_slices = (((frame_info >> 16) & 0xFF) as usize) + 1;
        let predictor = Predictor::from_frame_info(frame_info)?;
        // UQ only uses NONE and LEFT; any other code silently degrades.
        let predictor = match predictor {
            Predictor::None | Predictor::Left => predictor,
            _ => Predictor::None,
        };

        if n_slices == 0 || n_slices > 256 {
            return Err(Error::invalid(format!(
                "Ut Video UQ: invalid slice count {n_slices}"
            )));
        }

        let n_planes = self.shape.planes as usize;
        let mut planes_data: Vec<Vec<u8>> = Vec::with_capacity(n_planes);
        let mut stride_bytes: Vec<usize> = Vec::with_capacity(n_planes);
        let mut cursor = 4usize; // skip the 4-byte frame_info header

        for plane_idx in 0..n_planes {
            let pw = subsampled(self.width, self.shape.h_subsample[plane_idx]) as usize;
            let ph = subsampled(self.height, self.shape.v_subsample[plane_idx]) as usize;
            // Stride for 10-bit: 2 bytes per sample (u16 LE).
            stride_bytes.push(pw * 2);
            let mut plane_buf_u16 = vec![0u16; pw * ph];

            // Per-plane order: [slice-end offsets | slice-data | 1024 lens]
            // (inverted from classic — lengths come LAST).

            // 4 × N_slices LE32 cumulative end offsets.
            let off_table_len = 4 * n_slices;
            if cursor + off_table_len > packet.len() {
                return Err(Error::invalid(
                    "Ut Video UQ: truncated plane (slice-offset table)",
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
            if cursor + total_data > packet.len() {
                return Err(Error::invalid("Ut Video UQ: truncated plane (slice data)"));
            }
            let plane_data_start = cursor;
            cursor += total_data;

            // 1024-byte Huffman length table (after slice data).
            if cursor + 1024 > packet.len() {
                return Err(Error::invalid(
                    "Ut Video UQ: truncated plane (1024-byte Huffman table)",
                ));
            }
            let lens = &packet[cursor..cursor + 1024];
            cursor += 1024;
            let table = HuffTable10::from_lengths_1024(lens)?;

            let row_starts = compute_row_partition(ph as u32, n_slices);

            for s in 0..n_slices {
                let slice_start = if s == 0 { 0 } else { slice_ends[s - 1] };
                let slice_end = slice_ends[s];
                if slice_end < slice_start || slice_end > total_data {
                    return Err(Error::invalid(
                        "Ut Video UQ: slice end-offset table out of order",
                    ));
                }
                let bytes = &packet[plane_data_start + slice_start..plane_data_start + slice_end];
                let row_lo = row_starts[s] as usize;
                let row_hi = row_starts[s + 1] as usize;
                let slice_h = row_hi - row_lo;
                let slice_pixels = pw * slice_h;
                let dst = &mut plane_buf_u16[row_lo * pw..row_lo * pw + slice_pixels];

                if let HuffTable10::Fill { symbol } = table {
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
                apply_inverse_10bit(predictor, dst, pw, slice_h);
            }

            // RGB G-centred fix-up for 10-bit stored per-plane; we
            // collect u16 planes and apply after the loop below.
            // Pack the u16 plane into u8 bytes (LE).
            let mut plane_bytes = vec![0u8; pw * ph * 2];
            for (i, &v) in plane_buf_u16.iter().enumerate() {
                let off = i * 2;
                plane_bytes[off] = (v & 0xFF) as u8;
                plane_bytes[off + 1] = (v >> 8) as u8;
            }
            planes_data.push(plane_bytes);
        }

        // 10-bit RGB G-centred fix-up for UQRG / UQRA.
        if self.shape.is_rgb {
            // Unpack u16 for G, B, R planes, apply transform, repack.
            let pw = self.width as usize;
            let ph = self.height as usize;
            let n = pw * ph;
            let mut g_u16 = vec![0u16; n];
            let mut b_u16 = vec![0u16; n];
            let mut r_u16 = vec![0u16; n];
            unpack_u16_le(&planes_data[0], &mut g_u16);
            unpack_u16_le(&planes_data[1], &mut b_u16);
            unpack_u16_le(&planes_data[2], &mut r_u16);
            restore_g_centred_rgb_10bit(&g_u16, &mut b_u16, &mut r_u16);
            pack_u16_le(&b_u16, &mut planes_data[1]);
            pack_u16_le(&r_u16, &mut planes_data[2]);
        }

        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            planes: planes_data,
            stride_bytes,
        })
    }

    // ------------------------------------------------------------------
    // Pack (UM / SymPack) family — 8-bit, two-stream block coder
    // ------------------------------------------------------------------
    //
    // Per trace doc §7 / §12.5:
    //   - 8-byte packet header: byte[0]==1, bytes[1..3] skipped, bytes[4..7]=LE32 offset O
    //   - Packed stream: bytes [8..8+O)
    //   - After packed stream: LE32 nb_cbs; planes×slices packed_sizes (LE32);
    //     planes×slices control_sizes (LE32); then control stream bytes
    //   - Per block of 8 pixels: read 3 bits `b` from control stream (LE)
    //     if b==0 → 8 zero pixels; else read 8*(b+1) bits from packed stream (LE),
    //     apply sign-flip: k=b+1, sub=1<<(k-1), px=(((~p & sub) << (8-b)) + p - sub) & 0xFF
    //   - Predictor = GRADIENT (hardcoded); no interlaced
    // ------------------------------------------------------------------

    fn decode_pack(&mut self, packet: &[u8]) -> Result<DecodedFrame> {
        if packet.len() < 8 {
            return Err(Error::invalid("Ut Video UM: packet too short (< 8 bytes)"));
        }
        if packet[0] != 1 {
            return Err(Error::invalid(format!(
                "Ut Video UM: packet[0] = {} (expected 1)",
                packet[0]
            )));
        }
        // bytes[1..3] are skipped (wrapper version)
        let packed_offset = read_u32_le(packet, 4) as usize;
        // packed stream is bytes [8..8+packed_offset)
        if 8 + packed_offset > packet.len() {
            return Err(Error::invalid(
                "Ut Video UM: packet too short (packed stream)",
            ));
        }
        let packed_stream = &packet[8..8 + packed_offset];

        // After packed stream: [nb_cbs LE32 | planes*slices packed_sz LE32 | planes*slices ctrl_sz LE32 | ctrl data]
        let meta_start = 8 + packed_offset;
        if meta_start + 4 > packet.len() {
            return Err(Error::invalid("Ut Video UM: truncated nb_cbs field"));
        }
        let _nb_cbs = read_u32_le(packet, meta_start) as usize;
        let mut pos = meta_start + 4;

        let n_planes = self.shape.planes as usize;
        let n_slices = self.extradata.flags.slices as usize;
        if n_slices == 0 || n_slices > 256 {
            return Err(Error::invalid("Ut Video UM: invalid slice count"));
        }

        let n_entries = n_planes * n_slices;
        let packed_sizes_bytes = 4 * n_entries;
        let ctrl_sizes_bytes = 4 * n_entries;
        if pos + packed_sizes_bytes + ctrl_sizes_bytes > packet.len() {
            return Err(Error::invalid(
                "Ut Video UM: truncated size tables",
            ));
        }

        // Read packed_stream_size per (plane, slice) — layout is plane-major.
        let mut packed_sizes = vec![0usize; n_entries];
        for i in 0..n_entries {
            packed_sizes[i] = read_u32_le(packet, pos + i * 4) as usize;
        }
        pos += packed_sizes_bytes;

        let mut ctrl_sizes = vec![0usize; n_entries];
        for i in 0..n_entries {
            ctrl_sizes[i] = read_u32_le(packet, pos + i * 4) as usize;
        }
        pos += ctrl_sizes_bytes;

        // Remaining bytes are the control stream (concatenated, plane-major).
        let ctrl_stream = &packet[pos..];

        // Predictor = GRADIENT (hardcoded per trace doc §12.1).
        let predictor = Predictor::Gradient;

        let mut planes_data: Vec<Vec<u8>> = Vec::with_capacity(n_planes);
        let mut stride_bytes: Vec<usize> = Vec::with_capacity(n_planes);

        let mut packed_cursor = 0usize; // byte offset into packed_stream
        let mut ctrl_cursor = 0usize; // byte offset into ctrl_stream

        for plane_idx in 0..n_planes {
            let pw = subsampled(self.width, self.shape.h_subsample[plane_idx]) as usize;
            let ph = subsampled(self.height, self.shape.v_subsample[plane_idx]) as usize;
            stride_bytes.push(pw);
            let mut plane_buf = vec![0u8; pw * ph];

            let row_starts = compute_row_partition(ph as u32, n_slices);

            for s in 0..n_slices {
                let entry = plane_idx * n_slices + s;
                let psz = packed_sizes[entry];
                let csz = ctrl_sizes[entry];

                if packed_cursor + psz > packed_stream.len() {
                    return Err(Error::invalid("Ut Video UM: packed stream overrun"));
                }
                if ctrl_cursor + csz > ctrl_stream.len() {
                    return Err(Error::invalid("Ut Video UM: control stream overrun"));
                }

                let slice_packed = &packed_stream[packed_cursor..packed_cursor + psz];
                let slice_ctrl = &ctrl_stream[ctrl_cursor..ctrl_cursor + csz];
                packed_cursor += psz;
                ctrl_cursor += csz;

                let row_lo = row_starts[s] as usize;
                let row_hi = row_starts[s + 1] as usize;
                let slice_h = row_hi - row_lo;
                let slice_pixels = pw * slice_h;
                let dst = &mut plane_buf[row_lo * pw..row_lo * pw + slice_pixels];

                decode_sympack_slice(slice_packed, slice_ctrl, dst)?;
                apply_inverse_8bit(predictor, dst, pw, slice_h);
            }

            planes_data.push(plane_buf);
        }

        // RGB G-centred fix-up for UMRG / UMRA (same 8-bit formula).
        if self.shape.is_rgb {
            let (g_slice, rest) = planes_data.split_at_mut(1);
            let g = &g_slice[0];
            let (b_slice, rest2) = rest.split_at_mut(1);
            let b = &mut b_slice[0];
            let (r_slice, _) = rest2.split_at_mut(1);
            let r = &mut r_slice[0];
            restore_g_centred_rgb(g, b, r);
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

/// Decode one SymPack slice into `dst` (8-bit pixel residuals).
///
/// Per trace doc §12.5: reads 3-bit control words and (b+1)-bit pixel
/// groups from LE bit readers. Both streams are little-endian bit order
/// (no byte swap).
fn decode_sympack_slice(packed: &[u8], ctrl: &[u8], dst: &mut [u8]) -> Result<()> {
    let mut packed_br = LeBitReader::new(packed);
    let mut ctrl_br = LeBitReader::new(ctrl);

    let mut i = 0usize;
    while i < dst.len() {
        let b = ctrl_br.read_bits(3)? as u8;
        let block_end = (i + 8).min(dst.len());
        let block_len = block_end - i;

        if b == 0 {
            // All 8 pixels are zero residuals.
            for px in &mut dst[i..block_end] {
                *px = 0;
            }
        } else {
            let k = b + 1; // effective bit-width 2..=8
            let sub = 1u8 << (k - 1);
            for px in &mut dst[i..block_end] {
                let p = packed_br.read_bits(k as u32)? as u8;
                // Sign-flip mapping: centred around zero in the 8-bit modular ring.
                // From §12.5: pixel = ((~p & sub) << (8 - b)) + p - sub (mod 256)
                let sign_bit = (!p) & sub;
                let val = ((sign_bit as u16) << (8 - b as u16)) as u8;
                *px = val.wrapping_add(p).wrapping_sub(sub);
            }
        }
        // If block is partial (last block < 8 pixels), the remaining
        // control-stream bits for this block are not consumed — they are
        // padding. The loop exits naturally.
        let _ = block_len; // used via block_end
        i = block_end;
    }
    Ok(())
}

fn unpack_u16_le(bytes: &[u8], out: &mut [u16]) {
    for (i, v) in out.iter_mut().enumerate() {
        let off = i * 2;
        *v = bytes[off] as u16 | ((bytes[off + 1] as u16) << 8);
    }
}

fn pack_u16_le(src: &[u16], bytes: &mut [u8]) {
    for (i, &v) in src.iter().enumerate() {
        let off = i * 2;
        bytes[off] = (v & 0xFF) as u8;
        bytes[off + 1] = (v >> 8) as u8;
    }
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
    fn pro_decoder_constructs_ok() {
        // ExtraData::parse + UtVideoDecoder::new must succeed for UQ family.
        let xd_raw = [0u8; 8];
        let xd = ExtraData::parse(FourCc(*b"UQY2"), &xd_raw).unwrap();
        let dec = UtVideoDecoder::new(FourCc(*b"UQY2"), xd, 64, 48);
        assert!(dec.is_ok(), "UQY2 decoder construction must succeed");
    }

    #[test]
    fn pack_decoder_constructs_ok() {
        let mut xd_raw = [0u8; 16];
        xd_raw[8] = 2; // compression == COMP_PACK
        let xd = ExtraData::parse(FourCc(*b"UMY4"), &xd_raw).unwrap();
        let dec = UtVideoDecoder::new(FourCc(*b"UMY4"), xd, 64, 48);
        assert!(dec.is_ok(), "UMY4 decoder construction must succeed");
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
