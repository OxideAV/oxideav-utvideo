//! Per-frame decoder for Ut Video classic-family streams.
//!
//! The pipeline at this layer is:
//!
//! 1. Walk the chunk payload plane-by-plane per `spec/02` §7,
//!    extracting the 256-byte Huffman descriptor + slice-end-offset
//!    table + slice data for each plane in turn.
//! 2. Build a [`HuffmanTable`](crate::huffman::HuffmanTable) for each
//!    plane and decode every slice's residuals via the bit reader.
//! 3. Inverse-predict each slice using the predictor named by
//!    `frame_info & 0x300`.
//! 4. For RGB streams, undo the +128 / G-subtraction decorrelation.
//!
//! The decoded frame leaves this module as one [`DecodedPlane`] per
//! wire plane, in the on-wire order. Downstream consumers do their
//! own per-pixel reordering (e.g. R, G, B for an interleaved BGR
//! buffer) — this is consistent with `oxideav-magicyuv`'s decoded
//! plane API and keeps the codec free of pixel-packing policy.

use crate::error::{Error, Result};
use crate::fourcc::{Fourcc, Predictor, StreamConfig};
use crate::huffman::HuffmanTable;
use crate::predict;

/// One decoded plane: width × height in `samples`, plus the symbolic
/// label for the plane (Y, U, V, G, B, R, A) for the FOURCC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPlane {
    pub label: PlaneLabel,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<u8>,
}

/// Symbolic plane labels; reflect on-wire order, not BGR/RGB display
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneLabel {
    Y,
    U,
    V,
    G,
    B,
    R,
    A,
}

impl PlaneLabel {
    pub fn for_fourcc(fc: Fourcc, idx: usize) -> Self {
        match fc {
            Fourcc::Uly0 | Fourcc::Uly2 | Fourcc::Uly4 => match idx {
                0 => PlaneLabel::Y,
                1 => PlaneLabel::U,
                _ => PlaneLabel::V,
            },
            Fourcc::Ulrg => match idx {
                0 => PlaneLabel::G,
                1 => PlaneLabel::B,
                _ => PlaneLabel::R,
            },
            Fourcc::Ulra => match idx {
                0 => PlaneLabel::G,
                1 => PlaneLabel::B,
                2 => PlaneLabel::R,
                _ => PlaneLabel::A,
            },
        }
    }
}

/// Output of a successful frame decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub fourcc: Fourcc,
    pub width: u32,
    pub height: u32,
    pub predictor: Predictor,
    pub planes: Vec<DecodedPlane>,
    /// Trailing 4-byte frame-info dword as parsed off the wire. The
    /// prediction-mode bits have already been folded into `predictor`;
    /// other bits surface here for diagnostics.
    pub frame_info: u32,
}

/// Decode one Ut Video frame given its `00dc` chunk payload bytes
/// and a parsed [`StreamConfig`].
pub fn decode_frame(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<DecodedFrame> {
    let num_slices = cfg.num_slices();
    if num_slices == 0 {
        return Err(Error::InvalidSliceCount);
    }
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let frame_info_off = chunk_payload.len() - 4;

    let mut offset = 0usize;
    let mut planes = Vec::with_capacity(cfg.fourcc.plane_count());

    for plane_idx in 0..cfg.fourcc.plane_count() {
        let (pw, ph) = cfg.fourcc.plane_dim(plane_idx, cfg.width, cfg.height);
        let pw = pw as usize;
        let ph = ph as usize;

        // 256-byte Huffman descriptor.
        if offset + 256 > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: 256,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut code_length = [0u8; 256];
        code_length.copy_from_slice(&chunk_payload[offset..offset + 256]);
        offset += 256;

        // Slice-end offsets table.
        let table_bytes = num_slices * 4;
        if offset + table_bytes > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: table_bytes,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut slice_end_offsets = Vec::with_capacity(num_slices);
        for s in 0..num_slices {
            let v = u32::from_le_bytes(
                chunk_payload[offset + 4 * s..offset + 4 * s + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            slice_end_offsets.push(v);
        }
        offset += table_bytes;

        // Monotonicity + word alignment validation per spec/02 §5 +
        // spec/05 §4.1.
        let mut prev = 0usize;
        for &v in &slice_end_offsets {
            if v < prev {
                return Err(Error::NonMonotonicSliceOffsets);
            }
            if v % 4 != 0 {
                return Err(Error::SliceNotWordAligned(v));
            }
            prev = v;
        }
        let slice_data_total = *slice_end_offsets.last().unwrap();

        if offset + slice_data_total > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: slice_data_total,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let slice_data = &chunk_payload[offset..offset + slice_data_total];
        offset += slice_data_total;

        // Build the per-plane Huffman table and decode each slice.
        let table = HuffmanTable::build(&code_length)?;
        let mut slice_residuals: Vec<Vec<u8>> = Vec::with_capacity(num_slices);
        let mut prev_off = 0usize;
        for s in 0..num_slices {
            let r_start = (ph * s) / num_slices;
            let r_end = (ph * (s + 1)) / num_slices;
            let n_pixels = (r_end - r_start) * pw;
            let sd = &slice_data[prev_off..slice_end_offsets[s]];
            prev_off = slice_end_offsets[s];
            let res = if n_pixels == 0 {
                Vec::new()
            } else {
                table.decode_slice(sd, n_pixels)?
            };
            slice_residuals.push(res);
        }

        // Inverse-predict into a `pw * ph` buffer using the predictor
        // pulled below (we have to peek frame_info first, but we
        // need the residuals out of the per-plane loop). Defer the
        // reconstruct to a second pass with `predictor` resolved.
        planes.push(PendingPlane {
            label: PlaneLabel::for_fourcc(cfg.fourcc, plane_idx),
            width: pw as u32,
            height: ph as u32,
            slice_residuals,
        });
    }

    if offset != frame_info_off {
        return Err(Error::ChunkTooShort {
            offset,
            needed: frame_info_off - offset,
            have: 0,
        });
    }
    let frame_info = u32::from_le_bytes(
        chunk_payload[frame_info_off..frame_info_off + 4]
            .try_into()
            .unwrap(),
    );
    let predictor = Predictor::from_frame_info(frame_info);

    // Second pass: inverse predict.
    let mut decoded_planes: Vec<DecodedPlane> = planes
        .into_iter()
        .map(|p| {
            let mut samples = vec![0u8; (p.width * p.height) as usize];
            predict::apply(
                predictor,
                &mut samples,
                p.width as usize,
                p.height as usize,
                num_slices,
                &p.slice_residuals,
            );
            DecodedPlane {
                label: p.label,
                width: p.width,
                height: p.height,
                samples,
            }
        })
        .collect();

    // RGB inverse decorrelation per `spec/04` §6.
    if cfg.fourcc.is_rgb_family() {
        let g_clone = decoded_planes[0].samples.clone();
        let (_, rest) = decoded_planes.split_first_mut().unwrap();
        let (b_plane, rest2) = rest.split_first_mut().unwrap();
        let r_plane = &mut rest2[0];
        predict::inverse_decorrelate_rgb(&g_clone, &mut b_plane.samples, &mut r_plane.samples);
        // Alpha plane (idx 3) of ULRA is direct — no transform.
    }

    Ok(DecodedFrame {
        fourcc: cfg.fourcc,
        width: cfg.width,
        height: cfg.height,
        predictor,
        planes: decoded_planes,
        frame_info,
    })
}

struct PendingPlane {
    label: PlaneLabel,
    width: u32,
    height: u32,
    slice_residuals: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
    use crate::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

    fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
        let flags = 0x0000_0001 | (((slices as u32 - 1) & 0xff) << 24);
        let extradata = Extradata {
            encoder_version: 0x0100_00f0,
            source_format_tag: *b"YV12",
            frame_info_size: 4,
            flags,
        };
        StreamConfig::new(fc, w, h, extradata).unwrap()
    }

    #[test]
    fn decode_synthesised_uly0_constant_frame() {
        // Build a constant 16×16 ULY0 plane via the encoder and roundtrip.
        let cfg = cfg_for(Fourcc::Uly0, 16, 16, 1);
        let y = vec![123u8; 16 * 16];
        let u = vec![64u8; 8 * 8];
        let v = vec![200u8; 8 * 8];
        let frame = EncodedFrame {
            fourcc: cfg.fourcc,
            width: 16,
            height: 16,
            predictor: Predictor::Left,
            num_slices: 1,
            planes: vec![
                PlaneInput { samples: y.clone() },
                PlaneInput { samples: u.clone() },
                PlaneInput { samples: v.clone() },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        assert_eq!(decoded.fourcc, Fourcc::Uly0);
        assert_eq!(decoded.predictor, Predictor::Left);
        assert_eq!(decoded.planes.len(), 3);
        assert_eq!(decoded.planes[0].samples, y);
        assert_eq!(decoded.planes[1].samples, u);
        assert_eq!(decoded.planes[2].samples, v);
    }
}
