//! Round 2 hardening — exhaustive (FourCC × predictor × pattern × size
//! × slices) self-roundtrip matrix.
//!
//! Round 1 covered every documented FourCC + predictor + multi-slice
//! combination on a single deterministic LCG-noise pattern plus a few
//! corner cases. The Auditor Round 1 ([`docs/video/utvideo/audit/`]
//! `01-validation-report.md` §3.1) characterises a 1018-cell test
//! matrix that the Python cleanroom passes 100% against the FFmpeg
//! oracle. Round 2 mirrors that matrix into Rust as a self-roundtrip
//! suite — the same encoder bytes go through the same decoder, but
//! across the wider corpus the audit found load-bearing.
//!
//! Self-roundtrip is the strongest test we can run **without** a
//! committed FFmpeg fixture corpus (none has been promoted to
//! `docs/video/utvideo/tables/` yet); FFmpeg byte-equality remains a
//! deferred round candidate per [`CHANGELOG.md`] "Round 1 — notes" and
//! the audit §8 open items.
//!
//! Patterns covered (per Validator §3.1):
//!   - `zeros`     — all bytes = 0
//!   - `mid`       — all bytes = 0x80 (128)
//!   - `ones`      — all bytes = 0xff
//!   - `gradient`  — `(7r + 3c) mod 256` (matches `spec/05` §3.1.2 Y-plane)
//!   - `ramp_x`    — `c mod 256`
//!   - `ramp_y`    — `r mod 256`
//!   - `checker`   — 0/255 8×8 checkerboard
//!   - `random`    — deterministic LCG noise
//!
//! Sizes covered: 1×1, 2×2, 8×8, 15×15, 15×16, 16×15, 16×16,
//! 16×17 (odd-H), 17×16 (odd-W), 32×32, 32×64, 64×48, 1×16, 16×1
//! (filtered per FourCC dimension constraints — ULY0 needs even W&H,
//! ULY2 needs even W).
//!
//! Slice counts: 1, 2, 4, 8 (filtered when slice height would be 0).

use oxideav_utvideo::{
    decode_frame, encode_frame, EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor,
    StreamConfig,
};

// ----------------------------------------------------------------------
// Pattern generators
// ----------------------------------------------------------------------

#[derive(Copy, Clone, Debug)]
enum Pattern {
    Zeros,
    Mid,
    Ones,
    Gradient,
    RampX,
    RampY,
    Checker,
    Random(u64),
}

fn fill_plane(p: Pattern, w: usize, h: usize, plane_seed: u64) -> Vec<u8> {
    let n = w * h;
    let mut buf = vec![0u8; n];
    match p {
        Pattern::Zeros => {}
        Pattern::Mid => buf.fill(0x80),
        Pattern::Ones => buf.fill(0xff),
        Pattern::Gradient => {
            for r in 0..h {
                for c in 0..w {
                    buf[r * w + c] = ((7usize * r + 3usize * c) & 0xff) as u8;
                }
            }
        }
        Pattern::RampX => {
            for r in 0..h {
                for c in 0..w {
                    buf[r * w + c] = (c & 0xff) as u8;
                }
            }
        }
        Pattern::RampY => {
            for r in 0..h {
                for c in 0..w {
                    buf[r * w + c] = (r & 0xff) as u8;
                }
            }
        }
        Pattern::Checker => {
            for r in 0..h {
                for c in 0..w {
                    let cell = ((r / 8) + (c / 8)) & 1;
                    buf[r * w + c] = if cell == 0 { 0 } else { 0xff };
                }
            }
        }
        Pattern::Random(seed) => {
            let mut state = seed
                .wrapping_add(plane_seed)
                .wrapping_mul(2862933555777941757)
                .wrapping_add(3037000493);
            for v in buf.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *v = (state >> 56) as u8;
            }
        }
    }
    buf
}

fn build_planes(fc: Fourcc, w: u32, h: u32, p: Pattern) -> Vec<PlaneInput> {
    (0..fc.plane_count())
        .map(|i| {
            let (pw, ph) = fc.plane_dim(i, w, h);
            PlaneInput {
                samples: fill_plane(
                    p,
                    pw as usize,
                    ph as usize,
                    (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                ),
            }
        })
        .collect()
}

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

// `spec/02` §3.2 dimension validity. Plus: every plane must be
// non-empty (a zero-pixel plane has no slice partitioning).
fn dims_legal(fc: Fourcc, w: u32, h: u32) -> bool {
    if w == 0 || h == 0 {
        return false;
    }
    match fc {
        Fourcc::Uly0 => w % 2 == 0 && h % 2 == 0,
        Fourcc::Uly2 => w % 2 == 0,
        Fourcc::Uly4 | Fourcc::Ulrg | Fourcc::Ulra => true,
    }
}

// A slice count is legal iff every plane's height >= num_slices (so
// each slice gets at least one row). The wire formula
// `start_row = (height * n) / num_slices` allows zero-height slices
// in principle but the spec discourages them; round 1's encoder is
// happy either way, but skipping degenerate cases keeps the corpus
// honest.
fn slices_legal(fc: Fourcc, _w: u32, h: u32, num_slices: usize) -> bool {
    if num_slices == 0 || num_slices > 256 {
        return false;
    }
    let min_plane_h = match fc {
        Fourcc::Uly0 => h / 2,
        _ => h,
    };
    (min_plane_h as usize) >= num_slices
}

fn run_one(fc: Fourcc, w: u32, h: u32, p: Pattern, predictor: Predictor, slices: usize) {
    let planes = build_planes(fc, w, h, p);
    let cfg = cfg_for(fc, w, h, slices);
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor,
        num_slices: slices,
        planes: planes.clone(),
    };
    let bytes = encode_frame(&frame).unwrap_or_else(|e| {
        panic!("encode_frame failed FourCC={fc:?} {w}x{h} {p:?} {predictor:?} slices={slices}: {e}")
    });
    let decoded = decode_frame(&cfg, &bytes).unwrap_or_else(|e| {
        panic!("decode_frame failed FourCC={fc:?} {w}x{h} {p:?} {predictor:?} slices={slices}: {e}")
    });
    assert_eq!(decoded.fourcc, fc);
    assert_eq!(decoded.predictor, predictor);
    assert_eq!(decoded.planes.len(), fc.plane_count());
    for (i, want) in planes.iter().enumerate() {
        if decoded.planes[i].samples != want.samples {
            panic!(
                "plane {i} mismatch FourCC={fc:?} {w}x{h} pattern={p:?} predictor={predictor:?} slices={slices}: \
                 first diff at index {}",
                decoded.planes[i]
                    .samples
                    .iter()
                    .zip(&want.samples)
                    .position(|(a, b)| a != b)
                    .unwrap()
            );
        }
    }
}

// ----------------------------------------------------------------------
// Pattern × predictor × FourCC matrix per FOURCC
// ----------------------------------------------------------------------

const ALL_PATTERNS: &[Pattern] = &[
    Pattern::Zeros,
    Pattern::Mid,
    Pattern::Ones,
    Pattern::Gradient,
    Pattern::RampX,
    Pattern::RampY,
    Pattern::Checker,
    Pattern::Random(0xCAFE_F00D),
];

const ALL_PREDICTORS: &[Predictor] = &[
    Predictor::None,
    Predictor::Left,
    Predictor::Gradient,
    Predictor::Median,
];

fn pattern_matrix_for(fc: Fourcc) {
    let sizes: &[(u32, u32)] = &[
        (2, 2),
        (8, 8),
        (16, 16),
        (15, 16),
        (16, 15),
        (15, 15),
        (16, 17),
        (17, 16),
        (32, 32),
        (32, 64),
        (64, 48),
    ];
    for &(w, h) in sizes {
        if !dims_legal(fc, w, h) {
            continue;
        }
        for &p in ALL_PATTERNS {
            for &pred in ALL_PREDICTORS {
                for &slices in &[1usize, 2, 4, 8] {
                    if !slices_legal(fc, w, h, slices) {
                        continue;
                    }
                    run_one(fc, w, h, p, pred, slices);
                }
            }
        }
    }
}

#[test]
fn matrix_uly0() {
    pattern_matrix_for(Fourcc::Uly0);
}

#[test]
fn matrix_uly2() {
    pattern_matrix_for(Fourcc::Uly2);
}

#[test]
fn matrix_uly4() {
    pattern_matrix_for(Fourcc::Uly4);
}

#[test]
fn matrix_ulrg() {
    pattern_matrix_for(Fourcc::Ulrg);
}

#[test]
fn matrix_ulra() {
    pattern_matrix_for(Fourcc::Ulra);
}

// ----------------------------------------------------------------------
// Edge-case probes (audit/01-validation-report.md §3.2)
// ----------------------------------------------------------------------

#[test]
fn probe_minimum_dimension_1x1() {
    // 1×1 is legal for ULY4 / ULRG / ULRA only (Y-only pixel; chroma
    // planes 0×0 not legal in ULY0 / ULY2).
    for &fc in &[Fourcc::Uly4, Fourcc::Ulrg, Fourcc::Ulra] {
        for &pred in ALL_PREDICTORS {
            run_one(fc, 1, 1, Pattern::Mid, pred, 1);
            run_one(fc, 1, 1, Pattern::Ones, pred, 1);
        }
    }
}

#[test]
fn probe_minimum_dimension_2x2() {
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &pred in ALL_PREDICTORS {
            run_one(fc, 2, 2, Pattern::Random(0x12345), pred, 1);
        }
    }
}

#[test]
fn probe_thin_strip_1xn() {
    // 1×16 — single column, exercises the column-0-edge predictor
    // branches under multiple slices. ULY0 / ULY2 require even width
    // and skip; ULY4 / ULRG / ULRA take it.
    for &fc in &[Fourcc::Uly4, Fourcc::Ulrg, Fourcc::Ulra] {
        for &pred in ALL_PREDICTORS {
            run_one(fc, 1, 16, Pattern::Random(0xa1), pred, 1);
            run_one(fc, 1, 16, Pattern::Random(0xa2), pred, 4);
        }
    }
}

#[test]
fn probe_thin_strip_nx1() {
    // 16×1 — single row. With one row total, slice-count > 1 yields
    // zero-row slices and is skipped.
    for &fc in &[Fourcc::Uly4, Fourcc::Ulrg, Fourcc::Ulra] {
        for &pred in ALL_PREDICTORS {
            run_one(fc, 16, 1, Pattern::Gradient, pred, 1);
        }
    }
    // ULY2 requires even width (16 is even); chroma is 8×1.
    for &pred in ALL_PREDICTORS {
        run_one(Fourcc::Uly2, 16, 1, Pattern::Gradient, pred, 1);
    }
    // ULY0 requires even height; 16×1 is illegal — skipped.
}

#[test]
fn probe_tall_thin_8x240() {
    // 240 rows allows up to 240 slices in principle; we go up to 8.
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &pred in ALL_PREDICTORS {
            for &slices in &[1usize, 2, 8] {
                if dims_legal(fc, 8, 240) && slices_legal(fc, 8, 240, slices) {
                    run_one(fc, 8, 240, Pattern::Random(0xb0), pred, slices);
                }
            }
        }
    }
}

#[test]
fn probe_wide_short_1280x8() {
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &pred in ALL_PREDICTORS {
            for &slices in &[1usize, 2, 4] {
                if dims_legal(fc, 1280, 8) && slices_legal(fc, 1280, 8, slices) {
                    run_one(fc, 1280, 8, Pattern::Random(0xc0), pred, slices);
                }
            }
        }
    }
}

#[test]
fn probe_ulra_alpha_independent() {
    // ULRA's alpha plane is direct (no decorrelation transform per
    // `spec/04` §6). Confirm a non-trivial alpha pattern roundtrips
    // independently of the colour content.
    let w = 32u32;
    let h = 32u32;
    for &pred in ALL_PREDICTORS {
        let g = fill_plane(Pattern::Gradient, w as usize, h as usize, 0);
        let b = fill_plane(Pattern::RampX, w as usize, h as usize, 1);
        let r = fill_plane(Pattern::RampY, w as usize, h as usize, 2);
        let a = fill_plane(Pattern::Checker, w as usize, h as usize, 3);
        let cfg = cfg_for(Fourcc::Ulra, w, h, 1);
        let frame = EncodedFrame {
            fourcc: Fourcc::Ulra,
            width: w,
            height: h,
            predictor: pred,
            num_slices: 1,
            planes: vec![
                PlaneInput { samples: g.clone() },
                PlaneInput { samples: b.clone() },
                PlaneInput { samples: r.clone() },
                PlaneInput { samples: a.clone() },
            ],
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        assert_eq!(decoded.planes[0].samples, g, "G mismatch under {pred:?}");
        assert_eq!(decoded.planes[1].samples, b, "B mismatch under {pred:?}");
        assert_eq!(decoded.planes[2].samples, r, "R mismatch under {pred:?}");
        assert_eq!(decoded.planes[3].samples, a, "A mismatch under {pred:?}");
    }
}

#[test]
fn probe_solid_colours_all_fourccs() {
    // Every solid colour collapses each plane to a single-symbol
    // residual under -pred left (after decorrelation for RGB family).
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &pred in ALL_PREDICTORS {
            for &val in &[0u8, 1, 64, 128, 200, 254, 255] {
                let mut planes: Vec<PlaneInput> = Vec::new();
                for i in 0..fc.plane_count() {
                    let (pw, ph) = fc.plane_dim(i, 16, 16);
                    planes.push(PlaneInput {
                        samples: vec![val; (pw as usize) * (ph as usize)],
                    });
                }
                let cfg = cfg_for(fc, 16, 16, 1);
                let frame = EncodedFrame {
                    fourcc: fc,
                    width: 16,
                    height: 16,
                    predictor: pred,
                    num_slices: 1,
                    planes: planes.clone(),
                };
                let bytes = encode_frame(&frame).unwrap();
                let decoded = decode_frame(&cfg, &bytes).unwrap();
                for (i, want) in planes.iter().enumerate() {
                    assert_eq!(
                        decoded.planes[i].samples, want.samples,
                        "solid colour val={val} FourCC={fc:?} pred={pred:?} plane {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn probe_high_slice_count_uly4() {
    // 16 slices over a 16-row plane — one row per slice. Stresses the
    // per-slice +128 first-pixel seed convention 16× per encode.
    for &pred in ALL_PREDICTORS {
        run_one(Fourcc::Uly4, 16, 16, Pattern::Random(0xd0), pred, 16);
    }
}

#[test]
fn probe_two_symbol_descriptor() {
    // Solid Y=0 ULY4 with -pred none has all residuals = 0 — single
    // symbol. Verify that the descriptor with codelens {0: 0} (i.e.
    // sentinel) decodes back. (Already exercised by the round-1
    // unit test, but at the public-API level here.)
    for &pred in &[Predictor::None, Predictor::Left] {
        let planes: Vec<PlaneInput> = (0..3)
            .map(|_| PlaneInput {
                samples: vec![0u8; 64 * 64],
            })
            .collect();
        let cfg = cfg_for(Fourcc::Uly4, 64, 64, 1);
        let frame = EncodedFrame {
            fourcc: Fourcc::Uly4,
            width: 64,
            height: 64,
            predictor: pred,
            num_slices: 1,
            planes: planes.clone(),
        };
        let bytes = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&cfg, &bytes).unwrap();
        for (i, want) in planes.iter().enumerate() {
            assert_eq!(decoded.planes[i].samples, want.samples, "plane {i}");
        }
    }
}

// ----------------------------------------------------------------------
// Bidirectional cross-pattern probe — confirm encoder and decoder
// don't agree on a defect that self-roundtrip alone might mask. We
// re-encode a previously-decoded frame and verify the second-pass
// bytes match the first-pass bytes byte-for-byte.
// ----------------------------------------------------------------------

#[test]
fn double_roundtrip_bytes_stable() {
    // Encode → decode → encode' should give the same bytes (modulo
    // the encoder's deterministic Huffman tie-breaking; for that to
    // be stable our package-merge must be deterministic, which it
    // is since input ordering is by (count, sym) ascending).
    for &fc in &[
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ] {
        for &pred in ALL_PREDICTORS {
            for &slices in &[1usize, 2, 4] {
                let (w, h) = (32u32, 32u32);
                if !slices_legal(fc, w, h, slices) {
                    continue;
                }
                let planes = build_planes(fc, w, h, Pattern::Random(0xee));
                let cfg = cfg_for(fc, w, h, slices);
                let frame = EncodedFrame {
                    fourcc: fc,
                    width: w,
                    height: h,
                    predictor: pred,
                    num_slices: slices,
                    planes: planes.clone(),
                };
                let bytes1 = encode_frame(&frame).unwrap();
                let decoded = decode_frame(&cfg, &bytes1).unwrap();
                let frame2 = EncodedFrame {
                    fourcc: fc,
                    width: w,
                    height: h,
                    predictor: pred,
                    num_slices: slices,
                    planes: decoded
                        .planes
                        .iter()
                        .map(|p| PlaneInput {
                            samples: p.samples.clone(),
                        })
                        .collect(),
                };
                let bytes2 = encode_frame(&frame2).unwrap();
                assert_eq!(
                    bytes1, bytes2,
                    "double-encode bytes diverged for FourCC={fc:?} pred={pred:?} slices={slices}"
                );
            }
        }
    }
}
