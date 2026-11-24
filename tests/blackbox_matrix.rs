//! Live black-box interoperability matrix.
//!
//! Two directions, both generated at run time in a temporary directory
//! (nothing is committed) and both **skipped** when the `ffmpeg`
//! executable — the black-box Ut Video encoder + decoder — is absent:
//!
//! 1. **Foreign → ours.** The black-box encoder compresses deterministic
//!    raw pictures across every option it exposes that changes the
//!    bitstream (five pixel layouts, three predictors, every slice count
//!    it accepts, tiny / odd / large sizes, flat / noise / skewed
//!    content, 1-frame and long streams). Each frame body is decoded by
//!    this crate and compared byte-for-byte against both the black-box
//!    decoder's output and the original picture.
//! 2. **Ours → foreign.** This crate's encoder is swept across every
//!    FourCC × all four predictors × slice counts × sizes × content; the
//!    black-box decoder must accept every stream with an empty
//!    warning-level log and reproduce the input byte-for-byte.
//!
//! The codec crate does not parse containers. Frame bodies leave the
//! black-box tool one-file-per-packet (its `image2` writer with stream
//! copy, the 16-byte stream cookie prepended to each packet by its
//! `dump_extra` filter). To hand this crate's frames *to* the tool, the
//! test writes a minimal single-stream AVI envelope (write-only, ~60
//! lines, test-local).

#![cfg(not(miri))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_utvideo::decoder::decode_frame_parallel;
use oxideav_utvideo::{
    EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig, decode_frame,
    decode_frame_strict, encode_frame,
};

/// Serialises the two matrix tests so at most `WORKERS` tool processes
/// run at once regardless of the harness' own thread count.
static MATRIX_LOCK: Mutex<()> = Mutex::new(());

const WORKERS: usize = 4;

// ───────────────────────── tool detection ─────────────────────────

fn tool_available() -> bool {
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

// ───────────────────────── pixel layouts ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    pix_fmt: &'static str,
    fourcc: Fourcc,
    bit_count: u16,
}

const LAYOUTS: [Layout; 5] = [
    Layout {
        pix_fmt: "yuv420p",
        fourcc: Fourcc::Uly0,
        bit_count: 12,
    },
    Layout {
        pix_fmt: "yuv422p",
        fourcc: Fourcc::Uly2,
        bit_count: 16,
    },
    Layout {
        pix_fmt: "yuv444p",
        fourcc: Fourcc::Uly4,
        bit_count: 24,
    },
    Layout {
        pix_fmt: "gbrp",
        fourcc: Fourcc::Ulrg,
        bit_count: 24,
    },
    Layout {
        pix_fmt: "gbrap",
        fourcc: Fourcc::Ulra,
        bit_count: 32,
    },
];

impl Layout {
    fn plane_dims(&self, w: u32, h: u32) -> Vec<(usize, usize)> {
        (0..self.fourcc.plane_count())
            .map(|i| {
                let (pw, ph) = self.fourcc.plane_dim(i, w, h);
                (pw as usize, ph as usize)
            })
            .collect()
    }

    fn min_plane_height(&self, w: u32, h: u32) -> usize {
        self.plane_dims(w, h).iter().map(|d| d.1).min().unwrap_or(0)
    }

    fn frame_bytes(&self, w: u32, h: u32) -> usize {
        self.plane_dims(w, h).iter().map(|d| d.0 * d.1).sum()
    }

    fn dims_ok(&self, w: u32, h: u32) -> bool {
        self.fourcc.validate_dims(w, h).is_ok()
    }
}

// ───────────────────────── content ─────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Content {
    /// Every plane one constant value (non-128).
    Flat,
    /// Every plane constant 128 — zero residual under the +128 seed.
    FlatMid,
    /// Uniform random bytes: every symbol at the same code length.
    Noise,
    /// Smooth diagonal ramp.
    Ramp,
    /// Constant with rare random outliers: short alphabet, deep codes.
    Sparse,
    /// 0 / 255 checkerboard.
    Checker,
    /// Ramp plus small noise — natural-image-like residual spread.
    Soft,
    /// Geometric symbol distribution: forces long code lengths.
    Skew,
}

const ALL_CONTENT: [Content; 8] = [
    Content::Flat,
    Content::FlatMid,
    Content::Noise,
    Content::Ramp,
    Content::Sparse,
    Content::Checker,
    Content::Soft,
    Content::Skew,
];

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn fill_plane(
    content: Content,
    plane: usize,
    w: usize,
    h: usize,
    frame: usize,
    seed: u64,
) -> Vec<u8> {
    let mut rng = Rng::new(seed ^ ((plane as u64) << 32) ^ ((frame as u64) << 40) ^ 0x51ed);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let v = match content {
                Content::Flat => [77u8, 200, 31, 254][plane & 3].wrapping_add(frame as u8),
                Content::FlatMid => 128,
                Content::Noise => (rng.next() >> 24) as u8,
                Content::Ramp => (x * 3 + y * 5 + plane * 40 + frame * 2) as u8,
                Content::Sparse => {
                    let r = rng.next();
                    if r.is_multiple_of(97) {
                        (r >> 32) as u8
                    } else {
                        100
                    }
                }
                Content::Checker => {
                    if (x + y + frame) & 1 == 1 {
                        255
                    } else {
                        0
                    }
                }
                Content::Soft => {
                    let r = (rng.next() >> 40) as usize % 5;
                    (x * 2 + y + plane * 17 + r + frame) as u8
                }
                Content::Skew => {
                    let r = rng.next() >> 16;
                    ((r | (1 << 40)).trailing_zeros() as u8).wrapping_mul(37)
                }
            };
            out.push(v);
        }
    }
    out
}

/// One picture as tight per-plane buffers in wire / raw-file order.
fn make_picture(
    l: &Layout,
    w: u32,
    h: u32,
    content: Content,
    frame: usize,
    seed: u64,
) -> Vec<Vec<u8>> {
    l.plane_dims(w, h)
        .iter()
        .enumerate()
        .map(|(p, &(pw, ph))| fill_plane(content, p, pw, ph, frame, seed))
        .collect()
}

// ───────────────────────── process helpers ─────────────────────────

struct Run {
    ok: bool,
    stderr: String,
}

fn run_tool(args: &[String]) -> Run {
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

fn s(v: &str) -> String {
    v.to_string()
}

fn p(v: &Path) -> String {
    v.to_string_lossy().into_owned()
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
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

fn clear_dir(dir: &Path) {
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
fn write_avi(path: &Path, l: &Layout, w: u32, h: u32, extradata: &[u8; 16], frames: &[Vec<u8>]) {
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
    strh.extend_from_slice(l.fourcc.as_bytes());
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
    le16(&mut strf, l.bit_count);
    strf.extend_from_slice(l.fourcc.as_bytes());
    le32(&mut strf, (w * h * u32::from(l.bit_count)).div_ceil(8));
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

// ───────────────────────── cells ─────────────────────────

#[derive(Clone, Debug)]
struct Cell {
    layout: Layout,
    w: u32,
    h: u32,
    pred: Predictor,
    /// Requested slice count (`0` = leave the tool's default, foreign
    /// direction only).
    slices: usize,
    content: Content,
    frames: usize,
}

impl Cell {
    fn label(&self) -> String {
        format!(
            "{} {}x{} pred={:?} slices={} {:?} frames={}",
            self.layout.pix_fmt, self.w, self.h, self.pred, self.slices, self.content, self.frames
        )
    }

    fn seed(&self) -> u64 {
        (u64::from(self.w) << 20) ^ u64::from(self.h) ^ ((self.slices as u64) << 44)
    }

    fn pictures(&self) -> Vec<Vec<Vec<u8>>> {
        (0..self.frames)
            .map(|f| make_picture(&self.layout, self.w, self.h, self.content, f, self.seed()))
            .collect()
    }
}

fn pred_name(pr: Predictor) -> &'static str {
    match pr {
        Predictor::None => "none",
        Predictor::Left => "left",
        Predictor::Gradient => "gradient",
        Predictor::Median => "median",
    }
}

fn sizes_for(l: &Layout) -> Vec<(u32, u32)> {
    let all: [(u32, u32); 14] = [
        (1, 1),
        (1, 9),
        (9, 1),
        (2, 2),
        (2, 6),
        (6, 2),
        (3, 5),
        (15, 15),
        (16, 16),
        (17, 33),
        (18, 33),
        (64, 48),
        (130, 98),
        (322, 4),
    ];
    all.iter()
        .copied()
        .filter(|&(w, h)| l.dims_ok(w, h))
        .collect()
}

fn slice_choices(max: usize) -> Vec<usize> {
    let mut v: Vec<usize> = [1usize, 2, 3, 5, 7, 16, max]
        .iter()
        .copied()
        .filter(|&n| n >= 1 && n <= max && n <= 256)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Shared grid: layouts × sizes × content × slice choices, cycling the
/// given predictors over the full product so every (layout, predictor,
/// content) triple and every (size, predictor) pair appears.
fn grid(preds: &[Predictor]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for l in &LAYOUTS {
        for (w, h) in sizes_for(l) {
            let max = l.min_plane_height(w, h).min(256);
            for content in ALL_CONTENT {
                for slices in slice_choices(max) {
                    for &pred in preds {
                        cells.push(Cell {
                            layout: *l,
                            w,
                            h,
                            pred,
                            slices,
                            content,
                            frames: 1,
                        });
                    }
                }
            }
        }
    }
    cells
}

/// Every slice count the format allows on a tall narrow picture
/// (most of them do not divide the height).
fn slice_sweep(preds: &[Predictor]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for (li, l) in LAYOUTS.iter().enumerate() {
        let (w, h) = (16u32, 262u32);
        let max = l.min_plane_height(w, h).min(256);
        for slices in 1..=max {
            cells.push(Cell {
                layout: *l,
                w,
                h,
                pred: preds[(slices + li) % preds.len()],
                slices,
                content: if slices % 2 == 0 {
                    Content::Soft
                } else {
                    Content::Sparse
                },
                frames: 1,
            });
        }
    }
    cells
}

fn large_and_long(preds: &[Predictor]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for (li, l) in LAYOUTS.iter().enumerate() {
        for (k, &(w, h, slices)) in [(1920u32, 1080u32, 0usize), (1920, 1080, 8), (1280, 720, 13)]
            .iter()
            .enumerate()
        {
            cells.push(Cell {
                layout: *l,
                w,
                h,
                pred: preds[(li + k) % preds.len()],
                slices,
                content: [Content::Soft, Content::Skew, Content::Noise][k],
                frames: 1,
            });
        }
        // Long streams: every frame differs.
        for (k, &content) in [Content::Soft, Content::Flat].iter().enumerate() {
            cells.push(Cell {
                layout: *l,
                w: 48,
                h: 32,
                pred: preds[(li + k) % preds.len()],
                slices: 3,
                content,
                frames: 150,
            });
        }
    }
    cells.push(Cell {
        layout: LAYOUTS[0],
        w: 4096,
        h: 2160,
        pred: preds[preds.len() - 1],
        slices: 0,
        content: Content::Soft,
        frames: 1,
    });
    cells
}

// ───────────────────────── runner ─────────────────────────

#[derive(Default)]
struct Tally {
    pass: usize,
    refused: usize,
    /// Cells where the black-box decoder rejects the black-box
    /// encoder's own stream (no oracle).
    self_rejected: Vec<String>,
    /// Subset of `self_rejected` this crate nevertheless decodes to
    /// the original picture.
    self_rejected_recovered: usize,
    failures: Vec<String>,
    ours_bytes: u64,
    theirs_bytes: u64,
}

fn run_cells<F>(tag: &str, cells: &[Cell], f: F) -> Tally
where
    F: Fn(&Cell, &Path) -> Outcome + Sync,
{
    let scratch = Scratch::new(tag);
    let next = AtomicUsize::new(0);
    let tally = Mutex::new(Tally::default());
    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let dir = scratch.dir.join(format!("w{worker}"));
            let (next, tally, f) = (&next, &tally, &f);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(cell) = cells.get(i) else { break };
                    clear_dir(&dir);
                    let outcome = f(cell, &dir);
                    let mut t = tally.lock().unwrap();
                    match outcome {
                        Outcome::Pass { ours, theirs } => {
                            t.pass += 1;
                            t.ours_bytes += ours;
                            t.theirs_bytes += theirs;
                        }
                        Outcome::Refused => t.refused += 1,
                        Outcome::SelfRejected { recovered } => {
                            t.self_rejected.push(cell.label());
                            t.self_rejected_recovered += usize::from(recovered);
                        }
                        Outcome::Fail(why) => t.failures.push(format!("{}: {why}", cell.label())),
                    }
                }
            });
        }
    });
    tally.into_inner().unwrap()
}

enum Outcome {
    Pass {
        ours: u64,
        theirs: u64,
    },
    /// The black-box encoder declined the option combination.
    Refused,
    /// The black-box decoder rejected the black-box encoder's own
    /// stream: there is no reference output to compare against.
    SelfRejected {
        /// This crate decoded every frame to the original picture.
        recovered: bool,
    },
    Fail(String),
}

fn write_raw(path: &Path, pictures: &[Vec<Vec<u8>>]) {
    let mut raw = Vec::new();
    for pic in pictures {
        for plane in pic {
            raw.extend_from_slice(plane);
        }
    }
    fs::write(path, raw).expect("write raw input");
}

/// Black-box encode `cell` → per-frame `(extradata, body)` plus the
/// black-box decoder's raw output for the same stream.
enum Foreign {
    /// Per-frame packets (stream cookie + frame body) and the black-box
    /// decoder's raw output.
    Stream(Vec<Vec<u8>>, Vec<u8>),
    Refused,
    /// Packets the black-box decoder itself rejects.
    SelfRejected(Vec<Vec<u8>>),
}

fn foreign_encode(cell: &Cell, dir: &Path) -> Result<Foreign, String> {
    let pictures = cell.pictures();
    let input = dir.join("in.raw");
    write_raw(&input, &pictures);
    let avi = dir.join("t.avi");
    let mut args = vec![
        s("-loglevel"),
        s("error"),
        s("-f"),
        s("rawvideo"),
        s("-pix_fmt"),
        s(cell.layout.pix_fmt),
        s("-s"),
        format!("{}x{}", cell.w, cell.h),
        s("-r"),
        s("25"),
        s("-i"),
        p(&input),
        s("-c:v"),
        s("utvideo"),
        s("-pred"),
        s(pred_name(cell.pred)),
    ];
    if cell.slices != 0 {
        args.push(s("-slices"));
        args.push(cell.slices.to_string());
    }
    args.push(p(&avi));
    let enc = run_tool(&args);
    if !enc.ok {
        return Ok(Foreign::Refused);
    }
    let split = run_tool(&[
        s("-loglevel"),
        s("error"),
        s("-i"),
        p(&avi),
        s("-map"),
        s("0:v"),
        s("-c"),
        s("copy"),
        s("-bsf:v"),
        s("dump_extra=freq=all"),
        s("-f"),
        s("image2"),
        p(&dir.join("f_%06d.bin")),
    ]);
    if !split.ok {
        return Err(format!("black-box packet split failed: {}", split.stderr));
    }
    let mut packets = Vec::with_capacity(cell.frames);
    for i in 1..=cell.frames {
        let f = dir.join(format!("f_{i:06}.bin"));
        packets.push(fs::read(&f).map_err(|e| format!("missing packet {i}: {e}"))?);
    }
    let reference = dir.join("ref.raw");
    let dec = run_tool(&[
        s("-loglevel"),
        s("error"),
        s("-i"),
        p(&avi),
        s("-f"),
        s("rawvideo"),
        p(&reference),
    ]);
    if !dec.ok || !dec.stderr.trim().is_empty() {
        return Ok(Foreign::SelfRejected(packets));
    }
    let reference = fs::read(&reference).map_err(|e| format!("missing reference: {e}"))?;
    Ok(Foreign::Stream(packets, reference))
}

fn flatten(planes: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut v = Vec::new();
    for pl in planes {
        v.extend_from_slice(&pl);
    }
    v
}

fn foreign_cell(cell: &Cell, dir: &Path) -> Outcome {
    let (packets, reference) = match foreign_encode(cell, dir) {
        Ok(Foreign::Stream(pk, re)) => (pk, re),
        Ok(Foreign::Refused) => return Outcome::Refused,
        Ok(Foreign::SelfRejected(packets)) => {
            // No oracle. This crate must still answer without
            // panicking; record whether it recovers the picture.
            let pictures = cell.pictures();
            let mut recovered = packets.len() == pictures.len();
            for (pkt, pic) in packets.iter().zip(&pictures) {
                let got = (pkt.len() >= 16)
                    .then(|| pkt.split_at(16))
                    .and_then(|(cookie, body)| {
                        let ed = Extradata::parse(cookie).ok()?;
                        let cfg = StreamConfig::new(cell.layout.fourcc, cell.w, cell.h, ed).ok()?;
                        decode_frame(&cfg, body).ok()
                    })
                    .map(|f| flatten(f.planes.into_iter().map(|pl| pl.samples)));
                recovered &= got.as_deref() == Some(flatten(pic.iter().cloned()).as_slice());
            }
            return Outcome::SelfRejected { recovered };
        }
        Err(e) => return Outcome::Fail(e),
    };
    let pictures = cell.pictures();
    let fb = cell.layout.frame_bytes(cell.w, cell.h);
    if reference.len() != fb * cell.frames {
        return Outcome::Fail(format!(
            "black-box decode length {} != {}",
            reference.len(),
            fb * cell.frames
        ));
    }
    let mut theirs = 0u64;
    for (i, pkt) in packets.iter().enumerate() {
        if pkt.len() < 16 {
            return Outcome::Fail(format!("frame {i}: packet shorter than the stream cookie"));
        }
        let (cookie, body) = pkt.split_at(16);
        theirs += body.len() as u64;
        let ed = match Extradata::parse(cookie) {
            Ok(e) => e,
            Err(e) => return Outcome::Fail(format!("frame {i}: extradata refused: {e}")),
        };
        if cell.slices != 0 && ed.num_slices() != cell.slices {
            return Outcome::Fail(format!(
                "frame {i}: stream cookie says {} slices",
                ed.num_slices()
            ));
        }
        let cfg = match StreamConfig::new(cell.layout.fourcc, cell.w, cell.h, ed) {
            Ok(c) => c,
            Err(e) => return Outcome::Fail(format!("frame {i}: config refused: {e}")),
        };
        let want = &reference[i * fb..(i + 1) * fb];
        let original = flatten(pictures[i].iter().cloned());
        if want != original.as_slice() {
            return Outcome::Fail(format!("frame {i}: black-box round trip is not lossless"));
        }
        let serial = match decode_frame(&cfg, body) {
            Ok(f) => f,
            Err(e) => return Outcome::Fail(format!("frame {i}: decode refused: {e}")),
        };
        if serial.predictor != cell.pred {
            return Outcome::Fail(format!("frame {i}: predictor {:?}", serial.predictor));
        }
        let got = flatten(serial.planes.iter().map(|pl| pl.samples.clone()));
        if got != want {
            let at = got.iter().zip(want).position(|(a, b)| a != b);
            return Outcome::Fail(format!("frame {i}: pixel mismatch at byte {at:?}"));
        }
        // The other decode surfaces must agree; the strict one also
        // proves the foreign stream zero-pads its slices.
        match decode_frame_strict(&cfg, body) {
            Ok(f) if f == serial => {}
            Ok(_) => return Outcome::Fail(format!("frame {i}: strict decode differs")),
            Err(e) => return Outcome::Fail(format!("frame {i}: strict decode refused: {e}")),
        }
        match decode_frame_parallel(&cfg, body, 3) {
            Ok(f) if f == serial => {}
            Ok(_) => return Outcome::Fail(format!("frame {i}: parallel decode differs")),
            Err(e) => return Outcome::Fail(format!("frame {i}: parallel decode refused: {e}")),
        }
    }
    Outcome::Pass { ours: 0, theirs }
}

fn own_encode(cell: &Cell) -> Result<Vec<Vec<u8>>, String> {
    cell.pictures()
        .into_iter()
        .map(|pic| {
            let frame = EncodedFrame::new(
                cell.layout.fourcc,
                cell.w,
                cell.h,
                cell.pred,
                cell.slices,
                pic.into_iter().map(PlaneInput::new).collect(),
            );
            encode_frame(&frame).map_err(|e| format!("encode refused: {e}"))
        })
        .collect()
}

fn own_cell(cell: &Cell, dir: &Path) -> Outcome {
    let bodies = match own_encode(cell) {
        Ok(b) => b,
        Err(e) => return Outcome::Fail(e),
    };
    let ed = Extradata::canonical_extradata_for(cell.layout.fourcc, cell.slices).unwrap();
    let avi = dir.join("o.avi");
    write_avi(&avi, &cell.layout, cell.w, cell.h, &ed.to_bytes(), &bodies);
    let out = dir.join("o.raw");
    let dec = run_tool(&[
        s("-loglevel"),
        s("warning"),
        s("-probesize"),
        s("400M"),
        s("-i"),
        p(&avi),
        s("-f"),
        s("rawvideo"),
        p(&out),
    ]);
    if !dec.ok {
        return Outcome::Fail(format!("black-box decoder failed: {}", dec.stderr.trim()));
    }
    if !dec.stderr.trim().is_empty() {
        return Outcome::Fail(format!("black-box decoder warned: {}", dec.stderr.trim()));
    }
    let got = match fs::read(&out) {
        Ok(g) => g,
        Err(e) => return Outcome::Fail(format!("no decoder output: {e}")),
    };
    let want = flatten(cell.pictures().into_iter().flatten());
    if got != want {
        let at = got.iter().zip(&want).position(|(a, b)| a != b);
        return Outcome::Fail(format!(
            "black-box decode differs (len {} vs {}, first at {at:?})",
            got.len(),
            want.len()
        ));
    }
    let ours: u64 = bodies.iter().map(|b| b.len() as u64).sum();
    // Size reference: the black-box encoder on the same pictures, where
    // it offers the predictor.
    let mut theirs = 0u64;
    if cell.pred != Predictor::Gradient
        && let Ok(Foreign::Stream(packets, _)) = foreign_encode(cell, dir)
    {
        theirs = packets.iter().map(|pk| pk.len() as u64 - 16).sum();
    }
    Outcome::Pass {
        ours: if theirs == 0 { 0 } else { ours },
        theirs,
    }
}

fn report(name: &str, total: usize, t: &Tally) {
    eprintln!(
        "{name}: {total} cells — {} pass, {} declined by the black-box encoder, \
         {} where the black-box decoder rejects the black-box encoder's own stream \
         ({} of those decode to the original picture here), {} fail",
        t.pass,
        t.refused,
        t.self_rejected.len(),
        t.self_rejected_recovered,
        t.failures.len()
    );
    for f in t.failures.iter().take(60) {
        eprintln!("  FAIL {f}");
    }
}

// ───────────────────────── the two directions ─────────────────────────

const FOREIGN_PREDS: [Predictor; 3] = [Predictor::None, Predictor::Left, Predictor::Median];
const OWN_PREDS: [Predictor; 4] = [
    Predictor::None,
    Predictor::Left,
    Predictor::Gradient,
    Predictor::Median,
];

#[test]
fn foreign_streams_decode_bit_exact() {
    if !tool_available() {
        eprintln!("skipping: black-box ffmpeg with Ut Video support not found");
        return;
    }
    let _guard = MATRIX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cells = grid(&FOREIGN_PREDS);
    cells.extend(slice_sweep(&FOREIGN_PREDS));
    cells.extend(large_and_long(&FOREIGN_PREDS));
    let t = run_cells("foreign", &cells, foreign_cell);
    report("foreign -> ours", cells.len(), &t);
    assert!(t.failures.is_empty(), "{} cells failed", t.failures.len());
    assert_eq!(t.refused, 0, "the black-box encoder declined cells");
    if let Ok(path) = std::env::var("UTVIDEO_MATRIX_DUMP") {
        let _ = fs::write(path, t.self_rejected.join("\n"));
    }
    // Losing the oracle on a fringe of cells is expected (see the crate
    // README); losing it widely means the harness is broken.
    assert!(t.self_rejected.len() * 5 < cells.len(), "oracle lost");
    assert!(t.pass > 1000);
}

#[test]
fn own_streams_are_accepted_by_black_box_decoder() {
    if !tool_available() {
        eprintln!("skipping: black-box ffmpeg with Ut Video support not found");
        return;
    }
    let _guard = MATRIX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cells = grid(&OWN_PREDS);
    cells.extend(slice_sweep(&OWN_PREDS));
    cells.extend(large_and_long(&OWN_PREDS).into_iter().map(|mut c| {
        if c.slices == 0 {
            c.slices = 1;
        }
        c
    }));
    let t = run_cells("own", &cells, own_cell);
    report("ours -> foreign", cells.len(), &t);
    if t.theirs_bytes != 0 {
        eprintln!(
            "size on shared cells: ours {} bytes, black box {} bytes ({:+.2} %)",
            t.ours_bytes,
            t.theirs_bytes,
            (t.ours_bytes as f64 / t.theirs_bytes as f64 - 1.0) * 100.0
        );
    }
    assert!(t.failures.is_empty(), "{} cells failed", t.failures.len());
    assert!(t.pass > 1000);
}
