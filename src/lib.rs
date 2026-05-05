//! Pure-Rust decoder for **Ut Video**, Takeshi Umezawa's lossless
//! intra-only codec.
//!
//! Ut Video is a fast lossless intra-only codec built on per-plane
//! canonical Huffman over fixed inverse predictors (NONE / LEFT /
//! GRADIENT / MEDIAN). Each frame is split into N equal-row slices
//! that decode independently. Three FourCC families exist:
//!
//! * **classic UL** (8-bit) — `ULRG`, `ULRA`, `ULY0/2/4`, `ULH0/2/4`.
//! * **pro UQ** (10-bit) — `UQRG`, `UQRA`, `UQY0`, `UQY2`.
//! * **pack UM (SymPack)** — `UMRG`, `UMRA`, `UMY2/4`, `UMH2/4`.
//!
//! ## What this crate decodes today
//!
//! - All three families (classic UL, pro UQ, pack UM/SymPack).
//! - Classic 8-bit: predictors NONE / LEFT / GRADIENT / MEDIAN.
//! - Pro 10-bit: predictors NONE / LEFT (GRADIENT/MEDIAN silently
//!   treated as NONE per the decoder spec); 1024-symbol Huffman;
//!   10-bit mod-1024 arithmetic; header at packet start.
//! - Pack SymPack: two-stream block-of-8 LE bit coder; GRADIENT
//!   predictor hardcoded; no byte swap.
//! - Interlaced re-pairing for classic family.
//! - G-centred RGB inverse colour transform (8-bit and 10-bit).
//!
//! Verified against `ffmpeg -c:v utvideo` output for classic family
//! and against ffmpeg's utvideo decoder for UQ/UM self-encoded
//! fixtures.
//!
//! See `README.md` for the full coverage matrix.

#![deny(unsafe_code)]
#![allow(clippy::needless_range_loop)]

pub mod decoder;
pub mod extradata;
pub mod fourcc;
pub mod huffman;
pub mod predictor;

pub use decoder::{decode_packet, DecodedFrame, UtVideoDecoder};
pub use extradata::{ExtraData, Flags};
pub use fourcc::{Family, FourCc, PlaneShape};

use oxideav_core::frame::VideoPlane;
use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder,
    Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

pub const CODEC_ID_STR: &str = "utvideo";

/// Register the Ut Video decoder with a [`CodecRegistry`]. The decoder
/// is keyed by the `"utvideo"` codec id and matches every classic-family
/// FourCC tag (`ULRG`, `ULRA`, `ULY0/2/4`, `ULH0/2/4`). The container
/// resolves a FourCC tag to this codec id; the decoder then reconstructs
/// the FourCC from the [`CodecParameters::pixel_format`] hint (gbrp →
/// `ULRG`, yuv422p → `ULY2`, ...).
pub fn register(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("utvideo_sw")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_size(65535, 65535);
    let mut info = CodecInfo::new(CodecId::new(CODEC_ID_STR))
        .capabilities(caps)
        .decoder(make_decoder);
    for tag in [
        b"ULRG", b"ULRA", b"ULY0", b"ULY2", b"ULY4", b"ULH0", b"ULH2", b"ULH4",
    ] {
        info = info.tag(CodecTag::fourcc(tag));
    }
    reg.register(info);
}

/// Decoder factory consumed by [`CodecRegistry`]. Infers the FourCC
/// from [`CodecParameters::pixel_format`] (the only signal available in
/// `CodecParameters` today). For ambiguous cases (`ULY*` vs `ULH*`,
/// which differ only in colourspace and produce identical bitstreams)
/// we always pick the BT.601 FourCC.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let width = params
        .width
        .ok_or_else(|| Error::invalid("Ut Video: missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("Ut Video: missing height"))?;
    let fourcc = fourcc_from_pixel_format(params.pixel_format)?;
    let extradata = ExtraData::parse(fourcc, &params.extradata)?;
    Ok(Box::new(UtVideoDecoderHandle {
        codec_id: params.codec_id.clone(),
        decoder: UtVideoDecoder::new(fourcc, extradata, width, height)?,
        pending: None,
        eof: false,
    }))
}

/// Map a declared [`PixelFormat`] to the matching Ut Video FourCC.
/// `gbrp` is not yet a [`PixelFormat`] variant — the demuxer reports
/// `Rgb24` for the post-conversion shape — so callers wanting `ULRG`
/// must use [`UtVideoDecoder::new`] directly with `FourCc(*b"ULRG")`.
fn fourcc_from_pixel_format(pf: Option<PixelFormat>) -> Result<FourCc> {
    match pf {
        Some(PixelFormat::Yuv420P) => Ok(FourCc(*b"ULY0")),
        Some(PixelFormat::Yuv422P) => Ok(FourCc(*b"ULY2")),
        Some(PixelFormat::Yuv444P) => Ok(FourCc(*b"ULY4")),
        Some(PixelFormat::Rgb24) => Ok(FourCc(*b"ULRG")),
        Some(PixelFormat::Rgba) => Ok(FourCc(*b"ULRA")),
        Some(other) => Err(Error::unsupported(format!(
            "Ut Video: cannot derive FourCC from pixel format {other:?}"
        ))),
        None => Err(Error::invalid(
            "Ut Video: CodecParameters.pixel_format is required to disambiguate FourCC",
        )),
    }
}

struct UtVideoDecoderHandle {
    codec_id: CodecId,
    decoder: UtVideoDecoder,
    pending: Option<VideoFrame>,
    eof: bool,
}

impl Decoder for UtVideoDecoderHandle {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let DecodedFrame {
            width,
            height,
            planes,
            stride_bytes,
        } = self.decoder.decode(&packet.data)?;
        let frame = VideoFrame {
            pts: packet.pts,
            planes: planes
                .into_iter()
                .zip(stride_bytes)
                .map(|(data, stride)| VideoPlane { stride, data })
                .collect(),
        };
        let _ = (width, height);
        self.pending = Some(frame);
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Video(f)),
            None => {
                if self.eof {
                    Err(Error::Eof)
                } else {
                    Err(Error::NeedMore)
                }
            }
        }
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}
