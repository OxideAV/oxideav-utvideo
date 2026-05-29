//! `oxideav-core` framework integration: codec registration plus the
//! [`oxideav_core::Decoder`] implementation wrapping the crate's
//! `decode_frame`.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) skip
//! this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder,
    Error as CoreError, Frame, Packet, Result as CoreResult, RuntimeContext, VideoFrame,
    VideoPlane,
};

use crate::decoder::{decode_frame, DecodedFrame};
use crate::fourcc::{Extradata, Fourcc, StreamConfig};

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
        .with_lossless(true)
        .with_intra_only(true);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
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
