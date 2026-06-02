//! `oxideav-core` framework integration: codec registration plus the
//! [`oxideav_core::Decoder`] and [`oxideav_core::Encoder`]
//! implementations wrapping the crate's `decode_frame` /
//! `encode_frame`.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) skip
//! this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder,
    Encoder, Error as CoreError, Frame, Packet, PixelFormat, Result as CoreResult, RuntimeContext,
    TimeBase, VideoFrame, VideoPlane,
};

use crate::decoder::{decode_frame, DecodedFrame};
use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
use crate::fourcc::{Extradata, Fourcc, Predictor, StreamConfig};

/// Try to derive a [`Fourcc`] from `CodecParameters.tag`. The container
/// crate sets `tag` to the on-wire FourCC (`spec/01` §2) during demux;
/// this is the path the framework expects for container-routed streams.
fn fourcc_from_params(params: &CodecParameters) -> Option<Fourcc> {
    match params.tag.as_ref()? {
        CodecTag::Fourcc(bytes) => Fourcc::from_bytes(*bytes).ok(),
        _ => None,
    }
}

/// Canonical codec id. `oxideav-meta::register_all` calls
/// `crate::__oxideav_entry`, which delegates here.
pub const CODEC_ID_STR: &str = "utvideo";

/// Register the Ut Video codec with `reg`. Claims the five
/// classic-family FourCCs documented in `spec/01` §2.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("utvideo_sw")
        .with_decode()
        .with_encode()
        .with_lossless(true)
        .with_intra_only(true);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .tags([
                CodecTag::fourcc(b"ULRG"),
                CodecTag::fourcc(b"ULRA"),
                CodecTag::fourcc(b"ULY0"),
                CodecTag::fourcc(b"ULY2"),
                CodecTag::fourcc(b"ULY4"),
            ]),
    );
}

/// Unified entry point invoked by the macro-generated wrapper.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

// ──────────────────────── Decoder impl ────────────────────────

fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    // Round 14 — build the StreamConfig at factory time from the typed
    // `CodecParameters` surface the container (e.g. `oxideav-avi`) fills:
    //
    //   * `params.tag` carries the on-wire FourCC (`spec/01` §2),
    //   * `params.extradata` carries the 16-byte block (`spec/01` §4),
    //   * `params.width` / `params.height` carry the frame dims.
    //
    // If any of those is missing we leave `cfg` as `None` and surface a
    // diagnosable `Error::InvalidData` at `receive_frame` time. This
    // mirrors the `oxideav-huffyuv` registry pattern (see that crate's
    // `make_decoder`) so trait-driven decode works without callers
    // having to downcast and call `configure()`.
    let cfg = build_stream_config(params)?;
    Ok(Box::new(UtVideoDecoder {
        codec_id: params.codec_id.clone(),
        cfg,
        pending: None,
        eof: false,
    }))
}

/// Assemble a [`StreamConfig`] from `params` when every piece is present;
/// returns `None` if the container has not yet supplied FourCC / dims /
/// extradata so the decoder can be paired with a deferred `configure()`
/// call. Returns `Err` only when the supplied pieces are inconsistent
/// (malformed extradata, wrong FourCC, dimension constraint violation).
fn build_stream_config(params: &CodecParameters) -> CoreResult<Option<StreamConfig>> {
    let Some(fourcc) = fourcc_from_params(params) else {
        return Ok(None);
    };
    let (Some(width), Some(height)) = (params.width, params.height) else {
        return Ok(None);
    };
    if params.extradata.is_empty() {
        return Ok(None);
    }
    let extradata = Extradata::parse(&params.extradata)
        .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
    let cfg = StreamConfig::new(fourcc, width, height, extradata)
        .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
    Ok(Some(cfg))
}

struct UtVideoDecoder {
    codec_id: CodecId,
    /// Parsed identification surface. Built at factory time from the
    /// `CodecParameters` the container handed in. Left as `None` when
    /// the container has not yet supplied tag/dims/extradata — the
    /// hidden `configure()` hook (or a future `set_params` call) fills
    /// it in before the first `receive_frame`.
    cfg: Option<StreamConfig>,
    pending: Option<Packet>,
    eof: bool,
}

impl Decoder for UtVideoDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if self.pending.is_some() {
            return Err(CoreError::other(
                "oxideav-utvideo: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(CoreError::Eof)
            } else {
                Err(CoreError::NeedMore)
            };
        };
        let cfg = self
            .cfg
            .as_ref()
            .ok_or_else(|| CoreError::invalid("oxideav-utvideo: stream config not configured"))?;
        let frame = decode_frame(cfg, &pkt.data)
            .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
        Ok(Frame::Video(map_to_video_frame(frame, pkt.pts)))
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.eof = true;
        Ok(())
    }
}

impl UtVideoDecoder {
    /// Configure the stream from a FourCC + dimensions + extradata.
    /// `oxideav-avi` (or any other container) calls this once before
    /// dispatching packets through the [`Decoder`] trait. Reachable
    /// from tests through downcasting; future rounds expose this via
    /// a typed registry hook.
    #[allow(dead_code)]
    pub fn configure(
        &mut self,
        fourcc: Fourcc,
        width: u32,
        height: u32,
        extradata_bytes: &[u8],
    ) -> CoreResult<()> {
        let extradata = Extradata::parse(extradata_bytes)
            .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
        let cfg = StreamConfig::new(fourcc, width, height, extradata)
            .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
        self.cfg = Some(cfg);
        Ok(())
    }
}

fn map_to_video_frame(frame: DecodedFrame, pts: Option<i64>) -> VideoFrame {
    let planes = frame
        .planes
        .into_iter()
        .map(|p| VideoPlane {
            stride: p.width as usize,
            data: p.samples,
        })
        .collect();
    VideoFrame { pts, planes }
}

// ──────────────────────── Encoder impl ────────────────────────

/// Map a [`PixelFormat`] to the corresponding classic-family
/// [`Fourcc`]. The YUV trio routes by chroma subsampling
/// (`spec/02` §3.1). RGB / packed formats are not mapped: ULRG and
/// ULRA carry **planar** GBR / GBRA on the wire (`spec/04` §6 +
/// `spec/02` §3.1), so a caller asking for those must declare it via
/// `params.tag` and hand in three / four 8-bit planes.
fn fourcc_from_pixel_format(fmt: PixelFormat) -> Option<Fourcc> {
    match fmt {
        PixelFormat::Yuv420P => Some(Fourcc::Uly0),
        PixelFormat::Yuv422P => Some(Fourcc::Uly2),
        PixelFormat::Yuv444P => Some(Fourcc::Uly4),
        _ => None,
    }
}

/// Build the encoder-side identification surface. The container is
/// expected to supply either `params.tag` (FourCC) or
/// `params.pixel_format` together with `params.width` / `params.height`
/// and `params.extradata` (16 bytes per `spec/01` §4). When the tag is
/// absent we derive it from the pixel format; when extradata is empty
/// we synthesise the FFmpeg-pinned 16-byte block via
/// [`Extradata::ffmpeg_for`] so encoder construction stays a
/// single-call API for callers driving us through the framework's
/// [`Encoder`] trait without staging a separate extradata builder.
fn build_encoder_config(params: &CodecParameters) -> CoreResult<StreamConfig> {
    let fourcc = match fourcc_from_params(params) {
        Some(fc) => fc,
        None => match params.pixel_format {
            Some(fmt) => fourcc_from_pixel_format(fmt).ok_or_else(|| {
                CoreError::invalid(format!(
                    "oxideav-utvideo: encoder cannot derive FourCC from pixel format {fmt:?} — \
                     set CodecParameters::tag to a Ut Video FourCC (ULRG/ULRA/ULY0/ULY2/ULY4)"
                ))
            })?,
            None => {
                return Err(CoreError::invalid(
                    "oxideav-utvideo: encoder needs CodecParameters::tag or pixel_format",
                ));
            }
        },
    };
    let (Some(width), Some(height)) = (params.width, params.height) else {
        return Err(CoreError::invalid(
            "oxideav-utvideo: encoder needs CodecParameters::width / height",
        ));
    };
    let extradata = if params.extradata.is_empty() {
        // Synthesise a default-slice (single-slice) extradata so callers
        // can drive the encoder without first plumbing an extradata
        // builder. Containers that round-trip a populated extradata
        // through demux → re-encode get exact byte-equality with the
        // input via the populated branch below.
        Extradata::ffmpeg_for(fourcc, 1)
            .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?
    } else {
        Extradata::parse(&params.extradata)
            .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?
    };
    StreamConfig::new(fourcc, width, height, extradata)
        .map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))
}

fn make_encoder(params: &CodecParameters) -> CoreResult<Box<dyn Encoder>> {
    let cfg = build_encoder_config(params)?;
    let mut out_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
    out_params.width = Some(cfg.width);
    out_params.height = Some(cfg.height);
    out_params.pixel_format = params.pixel_format;
    out_params.tag = Some(CodecTag::fourcc(cfg.fourcc.as_bytes()));
    out_params.extradata = cfg.extradata.to_bytes().to_vec();
    Ok(Box::new(UtVideoEncoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        cfg,
        // Round 15/16 perf wins live on the Gradient predictor; pick it
        // as the trait-path default. Callers that want None / Left /
        // Median use the direct `encode_frame` API.
        predictor: Predictor::Gradient,
        out_params,
        pending: None,
        eof: false,
    }))
}

struct UtVideoEncoder {
    codec_id: CodecId,
    cfg: StreamConfig,
    predictor: Predictor,
    out_params: CodecParameters,
    pending: Option<Vec<u8>>,
    eof: bool,
}

impl Encoder for UtVideoEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> CoreResult<()> {
        if self.pending.is_some() {
            return Err(CoreError::other(
                "oxideav-utvideo: receive_packet must be called before sending another frame",
            ));
        }
        let vf = match frame {
            Frame::Video(v) => v,
            _ => {
                return Err(CoreError::invalid(
                    "oxideav-utvideo: encoder expected a video frame",
                ));
            }
        };
        let expected_planes = self.cfg.fourcc.plane_count();
        if vf.planes.len() != expected_planes {
            return Err(CoreError::invalid(format!(
                "oxideav-utvideo: encoder expected {expected_planes} planes for FourCC {:?}, got {}",
                self.cfg.fourcc,
                vf.planes.len()
            )));
        }
        // Repack each plane onto a tight `width * height` buffer so
        // `encode_frame` sees the layout it documents (one row per
        // `plane_dim().0` bytes, no stride padding). When the caller
        // already supplies a tight buffer (`stride == plane_width`)
        // this is a single `Vec::clone` per plane; padded strides
        // copy row-by-row.
        let mut planes: Vec<PlaneInput> = Vec::with_capacity(expected_planes);
        for (idx, plane) in vf.planes.iter().enumerate() {
            let (pw, ph) = self
                .cfg
                .fourcc
                .plane_dim(idx, self.cfg.width, self.cfg.height);
            let pw = pw as usize;
            let ph = ph as usize;
            let expected = pw * ph;
            let samples = if plane.stride == pw {
                if plane.data.len() < expected {
                    return Err(CoreError::invalid(format!(
                        "oxideav-utvideo: plane {idx} has {} bytes, expected {expected} \
                         ({pw}x{ph})",
                        plane.data.len()
                    )));
                }
                plane.data[..expected].to_vec()
            } else if plane.stride >= pw {
                // Stride-padded buffer — copy `pw` bytes per row.
                if plane.data.len() < plane.stride * ph {
                    return Err(CoreError::invalid(format!(
                        "oxideav-utvideo: plane {idx} has {} bytes, expected at least \
                         stride*height = {}",
                        plane.data.len(),
                        plane.stride * ph
                    )));
                }
                let mut tight = Vec::with_capacity(expected);
                for r in 0..ph {
                    let row_start = r * plane.stride;
                    tight.extend_from_slice(&plane.data[row_start..row_start + pw]);
                }
                tight
            } else {
                return Err(CoreError::invalid(format!(
                    "oxideav-utvideo: plane {idx} stride {} is below plane width {pw}",
                    plane.stride
                )));
            };
            planes.push(PlaneInput { samples });
        }
        let efr = EncodedFrame {
            fourcc: self.cfg.fourcc,
            width: self.cfg.width,
            height: self.cfg.height,
            predictor: self.predictor,
            num_slices: self.cfg.num_slices(),
            planes,
        };
        let bytes =
            encode_frame(&efr).map_err(|e| CoreError::invalid(format!("oxideav-utvideo: {e}")))?;
        self.pending = Some(bytes);
        Ok(())
    }

    fn receive_packet(&mut self) -> CoreResult<Packet> {
        match self.pending.take() {
            Some(bytes) => {
                let mut pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
                // All Ut Video frames are intra-only (the codec is
                // lossless and stateless across frames — `spec/02` §1),
                // so every emitted packet is a keyframe.
                pkt.flags.keyframe = true;
                Ok(pkt)
            }
            None => {
                if self.eof {
                    Err(CoreError::Eof)
                } else {
                    Err(CoreError::NeedMore)
                }
            }
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.eof = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::ProbeContext;

    #[test]
    fn register_via_runtime_context_installs_codec() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let codec_id = CodecId::new(CODEC_ID_STR);
        assert!(
            ctx.codecs.has_decoder(&codec_id),
            "codec registration should install a decoder factory"
        );
        assert!(
            ctx.codecs.has_encoder(&codec_id),
            "codec registration should install an encoder factory"
        );
    }

    #[test]
    fn register_claims_all_five_classic_fourccs() {
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        for fc in [b"ULRG", b"ULRA", b"ULY0", b"ULY2", b"ULY4"] {
            let tag = CodecTag::fourcc(fc);
            let resolved = reg
                .resolve_tag_ref(&ProbeContext::new(&tag))
                .map(|c| c.as_str());
            assert_eq!(
                resolved,
                Some(CODEC_ID_STR),
                "FourCC {:?} did not resolve to utvideo",
                std::str::from_utf8(fc).unwrap_or("????"),
            );
        }
    }
}
