//! ffmpeg cross-decode bit-exact for the encoder.
//!
//! Strategy: for each (FourCC, predictor) combo we
//! 1. synthesize a deterministic input frame,
//! 2. encode it through `oxideav_utvideo::encode_frame`,
//! 3. wrap the encoded packet in a minimal AVI we write to a
//!    temp file,
//! 4. ask the system `ffmpeg` to decode that AVI back to raw
//!    planar bytes,
//! 5. assert that ffmpeg's output equals our input.
//!
//! The test is skipped (with a printed notice) when `ffmpeg` is not
//! found on `PATH` — keeps CI green on hosts that lack the binary,
//! while still exercising the full encode → external-decode loop on
//! developer boxes.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use oxideav_utvideo::predictor::Predictor;
use oxideav_utvideo::{encode_frame, EncoderConfig, FourCc};

const W: u32 = 64;
const H: u32 = 48;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn deterministic_plane(seed: u8, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let mut s = seed as u32;
    for px in out.iter_mut() {
        s = s.wrapping_mul(1103515245).wrapping_add(12345);
        *px = (s >> 16) as u8;
    }
    out
}

fn subsampled(dim: u32, factor: u8) -> u32 {
    dim.div_ceil(factor as u32)
}

/// Write a minimal AVI file holding one Ut Video frame.
///
/// Layout matches what FFmpeg writes for `-c:v utvideo -frames:v 1`,
/// trimmed to the chunks our parser (and FFmpeg's own demuxer) need:
/// `RIFF / AVI ` → `LIST hdrl` (avih + `LIST strl`) → `LIST movi`
/// (single `00dc` chunk holding `packet`). No index — FFmpeg auto-
/// generates one if missing.
fn write_minimal_avi(
    fourcc: [u8; 4],
    extradata: &[u8],
    packet: &[u8],
    width: u32,
    height: u32,
    bit_count: u16,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();

    // strf body: BITMAPINFOHEADER (40 bytes) + extradata.
    let bi_size = 40u32 + extradata.len() as u32;
    let mut strf_body: Vec<u8> = Vec::with_capacity(bi_size as usize);
    strf_body.extend_from_slice(&bi_size.to_le_bytes()); // biSize
    strf_body.extend_from_slice(&width.to_le_bytes()); // biWidth
    strf_body.extend_from_slice(&height.to_le_bytes()); // biHeight
    strf_body.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf_body.extend_from_slice(&bit_count.to_le_bytes()); // biBitCount
    strf_body.extend_from_slice(&fourcc); // biCompression (FourCC)
    strf_body.extend_from_slice(&(width * height * (bit_count as u32) / 8).to_le_bytes()); // biSizeImage
    strf_body.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    strf_body.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    strf_body.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf_body.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    strf_body.extend_from_slice(extradata);

    // strh body (AVISTREAMHEADER).
    let mut strh_body: Vec<u8> = Vec::with_capacity(64);
    strh_body.extend_from_slice(b"vids");
    strh_body.extend_from_slice(&fourcc);
    strh_body.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh_body.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh_body.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh_body.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh_body.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh_body.extend_from_slice(&1u32.to_le_bytes()); // dwRate (fps numerator)
    strh_body.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh_body.extend_from_slice(&1u32.to_le_bytes()); // dwLength (1 frame)
    strh_body.extend_from_slice(&(packet.len() as u32).to_le_bytes()); // dwSuggestedBufferSize
    strh_body.extend_from_slice(&u32::MAX.to_le_bytes()); // dwQuality (-1)
    strh_body.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
                                                      // rcFrame: left, top, right, bottom (i16 each)
    strh_body.extend_from_slice(&0u16.to_le_bytes());
    strh_body.extend_from_slice(&0u16.to_le_bytes());
    strh_body.extend_from_slice(&(width as u16).to_le_bytes());
    strh_body.extend_from_slice(&(height as u16).to_le_bytes());

    // strl LIST = "strl" + strh-chunk + strf-chunk
    let mut strl: Vec<u8> = Vec::new();
    strl.extend_from_slice(b"strl");
    push_chunk(&mut strl, b"strh", &strh_body);
    push_chunk(&mut strl, b"strf", &strf_body);

    // avih body (MainAVIHeader).
    let mut avih_body: Vec<u8> = Vec::with_capacity(56);
    avih_body.extend_from_slice(&1_000_000u32.to_le_bytes()); // dwMicroSecPerFrame (1 fps)
    avih_body.extend_from_slice(&0u32.to_le_bytes()); // dwMaxBytesPerSec
    avih_body.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih_body.extend_from_slice(&0x10u32.to_le_bytes()); // dwFlags = AVIF_HASINDEX off / WASCAPTUREFILE
    avih_body.extend_from_slice(&1u32.to_le_bytes()); // dwTotalFrames
    avih_body.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih_body.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih_body.extend_from_slice(&(packet.len() as u32).to_le_bytes()); // dwSuggestedBufferSize
    avih_body.extend_from_slice(&width.to_le_bytes()); // dwWidth
    avih_body.extend_from_slice(&height.to_le_bytes()); // dwHeight
    avih_body.extend_from_slice(&[0u8; 16]); // dwReserved[4]

    // hdrl LIST = "hdrl" + avih-chunk + strl-list
    let mut hdrl: Vec<u8> = Vec::new();
    hdrl.extend_from_slice(b"hdrl");
    push_chunk(&mut hdrl, b"avih", &avih_body);
    push_list(&mut hdrl, &strl);

    // movi LIST = "movi" + 00dc-chunk
    let mut movi: Vec<u8> = Vec::new();
    movi.extend_from_slice(b"movi");
    push_chunk(&mut movi, b"00dc", packet);

    // Top-level: RIFF "AVI " + hdrl-list + movi-list.
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    push_list(&mut riff_body, &hdrl);
    push_list(&mut riff_body, &movi);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0); // word-pad
    }
}

fn push_list(out: &mut Vec<u8>, list_body: &[u8]) {
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(list_body.len() as u32).to_le_bytes());
    out.extend_from_slice(list_body);
}

/// Run ffmpeg to decode `avi_path` to raw planar bytes in `pix_fmt`.
fn ffmpeg_decode(avi_path: &PathBuf, pix_fmt: &str) -> Vec<u8> {
    let out_path = avi_path.with_extension(format!("decoded.{pix_fmt}"));
    let _ = fs::remove_file(&out_path);
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(avi_path)
        .args(["-f", "rawvideo", "-pix_fmt", pix_fmt])
        .arg(&out_path)
        .status()
        .expect("ffmpeg invocation");
    assert!(status.success(), "ffmpeg failed");
    let raw = fs::read(&out_path).expect("ffmpeg output");
    let _ = fs::remove_file(&out_path);
    raw
}

fn temp_avi_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oxideav_utvideo_enc_{name}.avi"))
}

fn assert_planes_eq(name: &str, got: &[u8], want: &[u8], width: usize) {
    if got == want {
        return;
    }
    let pos = got.iter().zip(want).position(|(a, b)| a != b).unwrap();
    let y = pos / width;
    let x = pos % width;
    panic!(
        "plane {name}: first mismatch at (x={x}, y={y}, byte#{pos}): got 0x{:02X}, want 0x{:02X}",
        got[pos], want[pos],
    );
}

fn run_rgb(fourcc_bytes: [u8; 4], pix_fmt: &str, alpha: bool, predictor: Predictor) {
    if !ffmpeg_available() {
        eprintln!("skipping ffmpeg cross-decode: ffmpeg not on PATH");
        return;
    }
    let pw = W as usize;
    let ph = H as usize;
    let g = deterministic_plane(7, pw, ph);
    let b = deterministic_plane(11, pw, ph);
    let r = deterministic_plane(19, pw, ph);
    let a = deterministic_plane(29, pw, ph);

    let cfg = EncoderConfig::new(FourCc(fourcc_bytes), W, H)
        .with_slices(2)
        .with_predictor(predictor);
    let planes_refs: Vec<&[u8]> = if alpha {
        vec![&g, &b, &r, &a]
    } else {
        vec![&g, &b, &r]
    };
    let enc = encode_frame(&cfg, &planes_refs).unwrap();

    let bit_count: u16 = if alpha { 32 } else { 24 };
    let avi_bytes = write_minimal_avi(fourcc_bytes, &enc.extradata, &enc.packet, W, H, bit_count);
    let path = temp_avi_path(&format!(
        "{}_{:?}",
        String::from_utf8_lossy(&fourcc_bytes),
        predictor
    ));
    fs::write(&path, &avi_bytes).expect("write avi");
    let raw = ffmpeg_decode(&path, pix_fmt);
    let _ = fs::remove_file(&path);

    let plane_size = pw * ph;
    let exp_g = &raw[0..plane_size];
    let exp_b = &raw[plane_size..2 * plane_size];
    let exp_r = &raw[2 * plane_size..3 * plane_size];
    assert_planes_eq("G", exp_g, &g, pw);
    assert_planes_eq("B", exp_b, &b, pw);
    assert_planes_eq("R", exp_r, &r, pw);
    if alpha {
        let exp_a = &raw[3 * plane_size..4 * plane_size];
        assert_planes_eq("A", exp_a, &a, pw);
    }
}

fn run_yuv(fourcc_bytes: [u8; 4], pix_fmt: &str, hsub: u8, vsub: u8, predictor: Predictor) {
    if !ffmpeg_available() {
        eprintln!("skipping ffmpeg cross-decode: ffmpeg not on PATH");
        return;
    }
    let yp = (W as usize) * (H as usize);
    let cw = subsampled(W, hsub) as usize;
    let chh = subsampled(H, vsub) as usize;
    let cp = cw * chh;
    let y = deterministic_plane(2, W as usize, H as usize);
    let u = deterministic_plane(5, cw, chh);
    let v = deterministic_plane(13, cw, chh);

    // ULY2 / 4:2:2 needs an even slice height in chroma; use 1 slice
    // for tiny H to keep partition trivial.
    let cfg = EncoderConfig::new(FourCc(fourcc_bytes), W, H)
        .with_slices(2)
        .with_predictor(predictor);
    let planes_refs: Vec<&[u8]> = vec![&y, &u, &v];
    let enc = encode_frame(&cfg, &planes_refs).unwrap();

    let bit_count: u16 = match (hsub, vsub) {
        (1, 1) => 24, // ULY4
        (2, 1) => 16, // ULY2 → reported as YUY2
        (2, 2) => 12, // ULY0
        _ => 24,
    };
    let avi_bytes = write_minimal_avi(fourcc_bytes, &enc.extradata, &enc.packet, W, H, bit_count);
    let path = temp_avi_path(&format!(
        "{}_{:?}",
        String::from_utf8_lossy(&fourcc_bytes),
        predictor
    ));
    fs::write(&path, &avi_bytes).expect("write avi");
    let raw = ffmpeg_decode(&path, pix_fmt);
    let _ = fs::remove_file(&path);

    let exp_y = &raw[0..yp];
    let exp_u = &raw[yp..yp + cp];
    let exp_v = &raw[yp + cp..yp + 2 * cp];
    assert_planes_eq("Y", exp_y, &y, W as usize);
    assert_planes_eq("U", exp_u, &u, cw);
    assert_planes_eq("V", exp_v, &v, cw);
}

#[test]
fn ffmpeg_decodes_our_ulrg_left() {
    run_rgb(*b"ULRG", "gbrp", false, Predictor::Left);
}

#[test]
fn ffmpeg_decodes_our_ulrg_median() {
    run_rgb(*b"ULRG", "gbrp", false, Predictor::Median);
}

#[test]
fn ffmpeg_decodes_our_ulrg_gradient() {
    run_rgb(*b"ULRG", "gbrp", false, Predictor::Gradient);
}

#[test]
fn ffmpeg_decodes_our_ulrg_none() {
    run_rgb(*b"ULRG", "gbrp", false, Predictor::None);
}

#[test]
fn ffmpeg_decodes_our_ulra_left() {
    run_rgb(*b"ULRA", "gbrap", true, Predictor::Left);
}

#[test]
fn ffmpeg_decodes_our_ulra_median() {
    run_rgb(*b"ULRA", "gbrap", true, Predictor::Median);
}

#[test]
fn ffmpeg_decodes_our_uly2_left() {
    run_yuv(*b"ULY2", "yuv422p", 2, 1, Predictor::Left);
}

#[test]
fn ffmpeg_decodes_our_uly2_median() {
    run_yuv(*b"ULY2", "yuv422p", 2, 1, Predictor::Median);
}

#[test]
fn ffmpeg_decodes_our_uly4_left() {
    run_yuv(*b"ULY4", "yuv444p", 1, 1, Predictor::Left);
}

#[test]
fn ffmpeg_decodes_our_uly4_median() {
    run_yuv(*b"ULY4", "yuv444p", 1, 1, Predictor::Median);
}

#[test]
fn ffmpeg_decodes_our_uly0_left() {
    run_yuv(*b"ULY0", "yuv420p", 2, 2, Predictor::Left);
}

#[test]
fn ffmpeg_decodes_our_uly0_median() {
    run_yuv(*b"ULY0", "yuv420p", 2, 2, Predictor::Median);
}
