//! Shared helpers for the tests that drive the black-box `ffmpeg`
//! executable: tool detection, process runs, a scratch directory, and a
//! minimal write-only AVI envelope to hand this crate's frames to the
//! tool. Included with `mod support;` from each test crate that needs
//! it (not every helper is used by every includer).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ───────────────────────── tool detection ─────────────────────────

pub fn tool_available() -> bool {
    let Ok(out) = Command::new("ffmpeg")
        .args(["-hide_banner", "-codecs"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    // The codec listing prints a capability column ("DEV.L." etc.) and
    // then the codec name; require both decode and encode capability.
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let mut it = l.split_whitespace();
        match (it.next(), it.next()) {
            (Some(caps), Some("utvideo")) => caps.starts_with("DE"),
            _ => false,
        }
    })
}

// ───────────────────────── process helpers ─────────────────────────

pub struct Run {
    pub ok: bool,
    pub stderr: String,
}

pub fn run_tool(args: &[String]) -> Run {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    Run {
        ok: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

pub fn s(v: &str) -> String {
    v.to_string()
}

pub fn p(v: &Path) -> String {
    v.to_string_lossy().into_owned()
}

pub struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "oxideav-utvideo-bbm-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn clear_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).expect("create cell dir");
}

// ───────────────────────── AVI envelope (write-only) ─────────────────────────

fn le32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn le16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    le32(out, body.len() as u32);
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

fn list(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(b"LIST");
    le32(out, body.len() as u32 + 4);
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
}

/// Minimal one-video-stream AVI: main header, stream header, stream
/// format (bitmap header + the 16-byte codec cookie), frame chunks and
/// an index. Write-only transport towards the black-box decoder.
pub fn write_avi(
    path: &Path,
    fourcc: &[u8; 4],
    bit_count: u16,
    w: u32,
    h: u32,
    extradata: &[u8; 16],
    frames: &[Vec<u8>],
) {
    let n = frames.len() as u32;
    let max_frame = frames.iter().map(|f| f.len()).max().unwrap_or(0) as u32;

    let mut avih = Vec::new();
    le32(&mut avih, 1_000); // microseconds per frame
    le32(&mut avih, 0);
    le32(&mut avih, 0);
    le32(&mut avih, 0x10); // index present
    le32(&mut avih, n);
    le32(&mut avih, 0);
    le32(&mut avih, 1); // streams
    le32(&mut avih, max_frame);
    le32(&mut avih, w);
    le32(&mut avih, h);
    avih.extend_from_slice(&[0u8; 16]);

    let mut strh = Vec::new();
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(fourcc);
    le32(&mut strh, 0);
    le32(&mut strh, 0);
    le32(&mut strh, 0);
    le32(&mut strh, 1); // scale
    le32(&mut strh, 1000); // rate
    le32(&mut strh, 0);
    le32(&mut strh, n);
    le32(&mut strh, max_frame);
    le32(&mut strh, u32::MAX);
    le32(&mut strh, 0);
    le16(&mut strh, 0);
    le16(&mut strh, 0);
    le16(&mut strh, w as u16);
    le16(&mut strh, h as u16);

    let mut strf = Vec::new();
    le32(&mut strf, 40 + 16);
    le32(&mut strf, w);
    le32(&mut strf, h);
    le16(&mut strf, 1);
    le16(&mut strf, bit_count);
    strf.extend_from_slice(fourcc);
    le32(&mut strf, (w * h * u32::from(bit_count)).div_ceil(8));
    le32(&mut strf, 0);
    le32(&mut strf, 0);
    le32(&mut strf, 0);
    le32(&mut strf, 0);
    strf.extend_from_slice(extradata);

    let mut strl = Vec::new();
    chunk(&mut strl, b"strh", &strh);
    chunk(&mut strl, b"strf", &strf);

    let mut hdrl = Vec::new();
    chunk(&mut hdrl, b"avih", &avih);
    list(&mut hdrl, b"strl", &strl);

    let mut movi = Vec::new();
    let mut idx = Vec::new();
    for f in frames {
        idx.extend_from_slice(b"00dc");
        le32(&mut idx, 0x10); // key frame
        le32(&mut idx, movi.len() as u32 + 4); // offset from the 'movi' tag
        le32(&mut idx, f.len() as u32);
        chunk(&mut movi, b"00dc", f);
    }

    let mut body = Vec::new();
    body.extend_from_slice(b"AVI ");
    list(&mut body, b"hdrl", &hdrl);
    list(&mut body, b"movi", &movi);
    chunk(&mut body, b"idx1", &idx);

    let mut file = Vec::with_capacity(body.len() + 8);
    file.extend_from_slice(b"RIFF");
    le32(&mut file, body.len() as u32);
    file.extend_from_slice(&body);
    fs::write(path, file).expect("write avi");
}
