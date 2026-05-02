//! Bit-exact decode of `ffmpeg -c:v utvideo` output.
//!
//! Each fixture is a single-frame AVI produced by:
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc=size=64x48:duration=1:rate=1 \
//!        -frames:v 1 -c:v utvideo -pix_fmt <pixfmt> -pred <pred> out.avi
//! ```
//!
//! plus the matching raw planar dump:
//!
//! ```text
//! ffmpeg -i out.avi -f rawvideo -pix_fmt <pixfmt> out.<pixfmt>
//! ```
//!
//! The test parses the AVI to extract the codec extradata (`strf` body
//! after the BMI header) and the first compressed frame (`00dc` chunk),
//! decodes that packet through `oxideav_utvideo::decode_packet`, and
//! compares plane-by-plane against the raw planar dump.

use std::fs;
use std::path::PathBuf;

use oxideav_utvideo::{decode_packet, FourCc};

const W: u32 = 64;
const H: u32 = 48;

#[test]
fn ulrg_none_matches_ffmpeg_raw() {
    run_ulrg("utv_64x48_ulrg_none");
}

#[test]
fn ulrg_left_matches_ffmpeg_raw() {
    run_ulrg("utv_64x48_ulrg_left");
}

#[test]
fn ulrg_median_matches_ffmpeg_raw() {
    run_ulrg("utv_64x48_ulrg_median");
}

#[test]
fn uly2_none_matches_ffmpeg_raw() {
    run_uly2("utv_64x48_uly2_none");
}

#[test]
fn uly2_left_matches_ffmpeg_raw() {
    run_uly2("utv_64x48_uly2_left");
}

#[test]
fn uly2_median_matches_ffmpeg_raw() {
    run_uly2("utv_64x48_uly2_median");
}

fn run_ulrg(stem: &str) {
    let avi_path = fixtures_dir().join(format!("{stem}.avi"));
    let raw_path = fixtures_dir().join(format!("{stem}.gbrp"));
    let avi = fs::read(&avi_path).expect("read avi fixture");
    let raw = fs::read(&raw_path).expect("read gbrp dump");

    let parsed = parse_avi(&avi);
    let frame = decode_packet(FourCc(*b"ULRG"), &parsed.extradata, W, H, &parsed.packet)
        .expect("decode ULRG packet");

    assert_eq!(frame.planes.len(), 3, "ULRG produces 3 planes (G, B, R)");
    let pw = W as usize;
    let ph = H as usize;
    assert_eq!(frame.planes[0].len(), pw * ph);
    assert_eq!(frame.planes[1].len(), pw * ph);
    assert_eq!(frame.planes[2].len(), pw * ph);

    // ffmpeg's `gbrp` raw layout is G plane, then B plane, then R plane,
    // all stored as packed full-resolution byte arrays.
    let expected_g = &raw[0..pw * ph];
    let expected_b = &raw[pw * ph..2 * pw * ph];
    let expected_r = &raw[2 * pw * ph..3 * pw * ph];
    assert_planes_eq("G", &frame.planes[0], expected_g, pw, ph);
    assert_planes_eq("B", &frame.planes[1], expected_b, pw, ph);
    assert_planes_eq("R", &frame.planes[2], expected_r, pw, ph);
}

fn run_uly2(stem: &str) {
    let avi_path = fixtures_dir().join(format!("{stem}.avi"));
    let raw_path = fixtures_dir().join(format!("{stem}.yuv422p"));
    let avi = fs::read(&avi_path).expect("read avi fixture");
    let raw = fs::read(&raw_path).expect("read yuv422p dump");

    let parsed = parse_avi(&avi);
    let frame = decode_packet(FourCc(*b"ULY2"), &parsed.extradata, W, H, &parsed.packet)
        .expect("decode ULY2 packet");

    assert_eq!(frame.planes.len(), 3, "ULY2 produces 3 planes (Y, U, V)");
    let yp = (W as usize) * (H as usize);
    let cp = (W as usize / 2) * (H as usize); // 4:2:2 — half-width, full-height chroma.
    assert_eq!(frame.planes[0].len(), yp);
    assert_eq!(frame.planes[1].len(), cp);
    assert_eq!(frame.planes[2].len(), cp);

    // ffmpeg's `yuv422p` raw layout: Y, then U, then V.
    let expected_y = &raw[0..yp];
    let expected_u = &raw[yp..yp + cp];
    let expected_v = &raw[yp + cp..yp + 2 * cp];
    assert_planes_eq("Y", &frame.planes[0], expected_y, W as usize, H as usize);
    assert_planes_eq(
        "U",
        &frame.planes[1],
        expected_u,
        W as usize / 2,
        H as usize,
    );
    assert_planes_eq(
        "V",
        &frame.planes[2],
        expected_v,
        W as usize / 2,
        H as usize,
    );
}

fn assert_planes_eq(name: &str, got: &[u8], want: &[u8], width: usize, height: usize) {
    if got == want {
        return;
    }
    // Find the first differing byte and report it in (x, y) coordinates
    // so failures are easy to localise.
    let pos = got.iter().zip(want).position(|(a, b)| a != b).unwrap();
    let y = pos / width;
    let x = pos % width;
    panic!(
        "plane {name}: first mismatch at (x={x}, y={y}, byte#{pos}): got 0x{:02X}, want 0x{:02X} \
        ({} pixels total)",
        got[pos],
        want[pos],
        width * height
    );
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

// ---------------------------------------------------------------------------
// Tiny AVI parser, scoped to what these fixtures need: extract the
// extradata sitting after the BITMAPINFOHEADER inside the `strf` chunk,
// and the first `00dc` movie chunk's body. Production-quality AVI
// support lives in `oxideav-avi`; we don't depend on it here so this
// test stays self-contained.
// ---------------------------------------------------------------------------

struct ParsedAvi {
    extradata: Vec<u8>,
    packet: Vec<u8>,
}

fn parse_avi(buf: &[u8]) -> ParsedAvi {
    assert!(&buf[0..4] == b"RIFF", "not a RIFF file");
    assert!(&buf[8..12] == b"AVI ", "RIFF form is not AVI");
    let mut pos = 12usize;
    let mut extradata: Option<Vec<u8>> = None;
    let mut packet: Option<Vec<u8>> = None;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        let body = &buf[pos + 8..pos + 8 + size];
        if id == b"LIST" {
            // Recurse into the list — its first 4 bytes are the form type.
            let form = &body[0..4];
            if form == b"hdrl" || form == b"strl" || form == b"movi" {
                walk_list(body, &mut extradata, &mut packet);
            }
        }
        // Skip body + word-pad.
        pos += 8 + size + (size & 1);
    }
    ParsedAvi {
        extradata: extradata.expect("strf extradata not found"),
        packet: packet.expect("no 00dc movie chunk found"),
    }
}

fn walk_list(buf: &[u8], extradata: &mut Option<Vec<u8>>, packet: &mut Option<Vec<u8>>) {
    let form = &buf[0..4];
    let mut pos = 4usize;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        let body_start = pos + 8;
        let body_end = body_start + size;
        if body_end > buf.len() {
            break;
        }
        let body = &buf[body_start..body_end];
        if id == b"LIST" {
            let inner_form = &body[0..4];
            if inner_form == b"strl" || inner_form == b"movi" {
                walk_list(body, extradata, packet);
            }
        } else if id == b"strf" {
            // BITMAPINFOHEADER is 40 bytes; everything after is extradata.
            if body.len() > 40 {
                *extradata = Some(body[40..].to_vec());
            } else {
                *extradata = Some(Vec::new());
            }
        } else if form == b"movi"
            && id.len() == 4
            && id[2] == b'd'
            && id[3] == b'c'
            && packet.is_none()
        {
            *packet = Some(body.to_vec());
        }
        pos += 8 + size + (size & 1);
    }
}
