//! Per-frame inverse predictors: NONE / LEFT / GRADIENT / MEDIAN.
//!
//! Per trace report §4.2, bits 8-9 of the per-frame `frame_info` LE32
//! select one of:
//!
//! | code | name     | row 0           | column 0 (later rows) | other       |
//! |------|----------|-----------------|------------------------|-------------|
//! | 0    | NONE     | raw             | raw                   | raw         |
//! | 1    | LEFT     | LEFT (per-row)  | LEFT (per-row)        | LEFT        |
//! | 2    | GRADIENT | LEFT            | TOP                   | a + c − b   |
//! | 3    | MEDIAN   | LEFT            | TOP                   | mid_pred    |
//!
//! `mid_pred(A, B, C)` = JPEG-LS / lossless-JPEG MED:
//!     `clip( a + b - c, min(a, b), max(a, b) )`.
//!
//! The first pixel of each LEFT row is seeded from `0x80` (8-bit) per
//! trace report §8. Predictors do **not** carry across slices — they
//! reset at the top of each slice (§8 LEFT note).

use oxideav_core::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Predictor {
    None,
    Left,
    Gradient,
    Median,
}

impl Predictor {
    pub fn from_frame_info(frame_info: u32) -> Result<Predictor> {
        match (frame_info >> 8) & 0x3 {
            0 => Ok(Predictor::None),
            1 => Ok(Predictor::Left),
            2 => Ok(Predictor::Gradient),
            3 => Ok(Predictor::Median),
            _ => Err(Error::invalid("Ut Video predictor selector out of range")),
        }
    }
}

const SEED_8BIT: u8 = 0x80;

/// Apply the inverse predictor in-place to a single slice of one
/// plane. `plane` is laid out as `width × slice_height` row-major;
/// `width` is the plane width (already chroma-subsampled by the
/// caller). For NONE this is a no-op.
pub fn apply_inverse_8bit(pred: Predictor, plane: &mut [u8], width: usize, slice_height: usize) {
    if width == 0 || slice_height == 0 {
        return;
    }
    debug_assert_eq!(plane.len(), width * slice_height);
    match pred {
        Predictor::None => {}
        Predictor::Left => apply_left(plane, width, slice_height),
        Predictor::Gradient => apply_gradient(plane, width, slice_height),
        Predictor::Median => apply_median(plane, width, slice_height),
    }
}

fn apply_left(plane: &mut [u8], _width: usize, _slice_height: usize) {
    // Empirical: LEFT is a single linear cumulative-sum scan over the
    // whole slice (NOT a per-row reseed). The trace report wording
    // ("first pixel of each row seeded from 0x80") is observed to be
    // imprecise — the seed applies only to the very first pixel of
    // each slice; every subsequent pixel uses its in-stream
    // predecessor, including the wrap-around between rows. Verified
    // against `ffmpeg -c:v utvideo -pred left` ULRG / ULY2 fixtures
    // (see `tests/ffmpeg_interop.rs`).
    let mut prev = SEED_8BIT;
    for px in plane.iter_mut() {
        let sum = prev.wrapping_add(*px);
        *px = sum;
        prev = sum;
    }
}

fn apply_gradient(plane: &mut [u8], width: usize, slice_height: usize) {
    // Row 0 uses LEFT.
    {
        let row0 = &mut plane[0..width];
        let mut prev = SEED_8BIT;
        for px in row0.iter_mut() {
            let sum = prev.wrapping_add(*px);
            *px = sum;
            prev = sum;
        }
    }
    // Subsequent rows: x=0 uses TOP; x>=1 uses a + c - b
    // where a = pixel[y, x-1], b = pixel[y-1, x-1], c = pixel[y-1, x].
    for y in 1..slice_height {
        let (top_part, this_part) = plane.split_at_mut(y * width);
        let prev_row = &top_part[(y - 1) * width..(y - 1) * width + width];
        let cur_row = &mut this_part[0..width];
        // Column 0 = TOP.
        let top0 = prev_row[0];
        let v0 = top0.wrapping_add(cur_row[0]);
        cur_row[0] = v0;
        for x in 1..width {
            let a = cur_row[x - 1];
            let b = prev_row[x - 1];
            let c = prev_row[x];
            let pred = a.wrapping_add(c).wrapping_sub(b);
            cur_row[x] = pred.wrapping_add(cur_row[x]);
        }
    }
}

fn apply_median(plane: &mut [u8], width: usize, slice_height: usize) {
    // Row 0: LEFT seeded from 0x80 (per trace report §8 row-0 fallback).
    {
        let row0 = &mut plane[0..width];
        let mut prev = SEED_8BIT;
        for px in row0.iter_mut() {
            let sum = prev.wrapping_add(*px);
            *px = sum;
            prev = sum;
        }
    }
    if slice_height < 2 {
        return;
    }
    // Empirical: rows 1+ run a uniform `median(a, b, a+b-c mod 256)`
    // predictor over a *linear-scan* view of the slice — i.e. the
    // "left" neighbour at column 0 wraps to the last pixel of the
    // previous row. The "above-left" wraps similarly: for pixel
    // (0, y>=2) it's the last pixel of row `y-2`; for the very first
    // pixel of row 1 (`y == 1, x == 0`), the linear-scan `pixel[-1]`
    // is undefined and the encoder collapses it to "left" — making
    // the gradient equal to "above" and the predictor equal to TOP.
    //
    // Concretely, define a virtual pixel buffer `decoded` whose index
    // `i = y*W + x` runs left-to-right, top-to-bottom; then `a =
    // decoded[i-1]`, `b = decoded[i-W]`, `c = decoded[i-W-1]` for
    // `i > W`, and `c = a` for `i == W`. The MED-style 3-way median
    // (NOT clip) of `(a, b, a+b-c)` is the predictor.
    //
    // Verified bit-exactly against `ffmpeg -c:v utvideo -pred median`
    // ULRG and ULY2 fixtures.
    for y in 1..slice_height {
        for x in 0..width {
            let i = y * width + x;
            // `a` (left, with wrap to end of previous row at col 0).
            let a = plane[i - 1];
            // `b` (top).
            let b = plane[i - width];
            // `c` (above-left): wraps to last-pixel-of-row-(y-2) at
            // col 0; the very first row-1, col-0 pixel collapses to
            // `a` so the gradient becomes top.
            let c = if x == 0 && y == 1 {
                a
            } else {
                plane[i - width - 1]
            };
            let predict = mid_pred(a, b, c);
            plane[i] = predict.wrapping_add(plane[i]);
        }
    }
}

/// Ut Video's MEDIAN predictor: `median(a, b, a + b - c)` — the
/// "predict median" form per the multimedia wiki. The third value
/// (`a + b - c`) is computed **modulo 256** for 8-bit samples, so the
/// straight 3-way median (NOT `clip(a+b-c, min(a,b), max(a,b))`) is
/// the right primitive. The two forms agree as long as `a + b - c`
/// stays in `0..=255`; they diverge when it overflows or
/// underflows, and both ULRG-median and ULY2-median fixtures exercise
/// the wrap-around case.
///
/// Verified bit-exactly against `ffmpeg -c:v utvideo -pred median`
/// for ULRG and ULY2 (see `tests/ffmpeg_interop.rs`).
#[inline]
fn mid_pred(a: u8, b: u8, c: u8) -> u8 {
    let gradient = a.wrapping_add(b).wrapping_sub(c);
    median3(a, b, gradient)
}

/// 3-way median of u8s: `(a + b + c) - min - max`.
#[inline]
fn median3(a: u8, b: u8, c: u8) -> u8 {
    let lo = a.min(b).min(c);
    let hi = a.max(b).max(c);
    let sum = a as u16 + b as u16 + c as u16;
    (sum - lo as u16 - hi as u16) as u8
}

/// Inverse of the G-centred RGB transform (trace report §8.1):
///
/// ```text
/// R = R' + G - 0x80   (mod 256)
/// B = B' + G - 0x80   (mod 256)
/// G = G                (unchanged)
/// ```
///
/// Each input plane is `width × height`. R/B are mutated in place; G is
/// only read.
pub fn restore_g_centred_rgb(g: &[u8], b_plane: &mut [u8], r_plane: &mut [u8]) {
    debug_assert_eq!(g.len(), b_plane.len());
    debug_assert_eq!(g.len(), r_plane.len());
    for ((gp, bp), rp) in g.iter().zip(b_plane.iter_mut()).zip(r_plane.iter_mut()) {
        *bp = bp.wrapping_add(*gp).wrapping_sub(SEED_8BIT);
        *rp = rp.wrapping_add(*gp).wrapping_sub(SEED_8BIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_seed_matches_spec() {
        // Single row, raw symbol 0x10 → decoded sum should be 0x80 + 0x10.
        let mut p = vec![0x10u8];
        apply_inverse_8bit(Predictor::Left, &mut p, 1, 1);
        assert_eq!(p[0], 0x90);
    }

    #[test]
    fn left_chains() {
        let mut p = vec![0u8; 4];
        // Residuals [10, 20, 30, 40] → cumulative seeded at 0x80.
        p.copy_from_slice(&[10, 20, 30, 40]);
        apply_inverse_8bit(Predictor::Left, &mut p, 4, 1);
        assert_eq!(p, vec![0x80 + 10, 0x80 + 30, 0x80 + 60, 0x80 + 100]);
    }

    #[test]
    fn predictor_selector_bits_8_9() {
        assert_eq!(Predictor::from_frame_info(0x000).unwrap(), Predictor::None);
        assert_eq!(Predictor::from_frame_info(0x100).unwrap(), Predictor::Left);
        assert_eq!(
            Predictor::from_frame_info(0x200).unwrap(),
            Predictor::Gradient
        );
        assert_eq!(
            Predictor::from_frame_info(0x300).unwrap(),
            Predictor::Median
        );
    }

    /// Forward GRADIENT predictor matching the inverse rules in
    /// [`apply_gradient`]: row 0 of the slice uses single-row LEFT
    /// (seed `0x80`); column 0 of subsequent rows uses TOP; otherwise
    /// `residual = pixel - (A + B - C) mod 256`.
    fn forward_gradient(plane: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut out = vec![0u8; plane.len()];
        // Row 0: forward LEFT seeded at 0x80.
        let mut prev = SEED_8BIT;
        for x in 0..width {
            out[x] = plane[x].wrapping_sub(prev);
            prev = plane[x];
        }
        // Subsequent rows.
        for y in 1..height {
            // Column 0: forward TOP.
            let i = y * width;
            out[i] = plane[i].wrapping_sub(plane[i - width]);
            for x in 1..width {
                let i = y * width + x;
                let a = plane[i - 1];
                let b = plane[i - width];
                let c = plane[i - width - 1];
                let pred = a.wrapping_add(b).wrapping_sub(c);
                out[i] = plane[i].wrapping_sub(pred);
            }
        }
        out
    }

    /// Forward MEDIAN matching [`apply_median`]: row 0 uses LEFT;
    /// row 1 column 0 uses TOP; row 1 column ≥ 1 and rows ≥ 2 use
    /// `mid_pred(A, B, (A + B - C) mod 256)`.
    fn forward_median(plane: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut out = vec![0u8; plane.len()];
        // Row 0: forward LEFT seeded at 0x80.
        let mut prev = SEED_8BIT;
        for x in 0..width {
            out[x] = plane[x].wrapping_sub(prev);
            prev = plane[x];
        }
        if height < 2 {
            return out;
        }
        // The inverse path treats the slice as a flattened linear scan
        // for the `a` (left) and `c` (above-left) wrap-arounds — see
        // `apply_median`. Mirror that exactly for the forward path.
        for y in 1..height {
            for x in 0..width {
                let i = y * width + x;
                let a = plane[i - 1];
                let b = plane[i - width];
                let c = if x == 0 && y == 1 {
                    a
                } else {
                    plane[i - width - 1]
                };
                let pred = mid_pred(a, b, c);
                out[i] = plane[i].wrapping_sub(pred);
            }
        }
        out
    }

    #[test]
    fn gradient_round_trip_handcrafted() {
        // 6×4 plane chosen to exercise both row-0 LEFT and the
        // gradient-rule pixels in subsequent rows, plus a column-0
        // wrap into the previous row.
        let w = 6;
        let h = 4;
        let plane: Vec<u8> = (0..(w * h)).map(|i| (i as u8).wrapping_mul(7)).collect();
        let residuals = forward_gradient(&plane, w, h);
        let mut out = residuals.clone();
        apply_inverse_8bit(Predictor::Gradient, &mut out, w, h);
        assert_eq!(out, plane, "gradient inverse must undo its forward");
    }

    #[test]
    fn gradient_row0_uses_left_seed() {
        // Row 0, single row, residuals [10, 20] should decode to
        // [0x80+10, 0x80+30] (cumulative LEFT seeded at 0x80).
        let mut p = vec![10u8, 20];
        apply_inverse_8bit(Predictor::Gradient, &mut p, 2, 1);
        assert_eq!(p, vec![0x80 + 10, 0x80 + 30]);
    }

    #[test]
    fn gradient_col0_row1_uses_top() {
        // 2×2, residuals laid out row-major:
        //   row 0: [0, 0]              → after row-0 LEFT (seed 0x80) → [0x80, 0x80]
        //   row 1: [0x10, 0]           → col 0 uses TOP → 0x80 + 0x10 = 0x90
        //                                col 1 uses A+B-C = 0x90 + 0x80 - 0x80 = 0x90
        //                                pixel = 0 + 0x90 = 0x90
        let mut p = vec![0u8, 0, 0x10, 0];
        apply_inverse_8bit(Predictor::Gradient, &mut p, 2, 2);
        assert_eq!(p, vec![0x80, 0x80, 0x90, 0x90]);
    }

    #[test]
    fn median_round_trip_handcrafted() {
        // 5×4 plane mixing high and low values to make the 3-way
        // median step nontrivial and exercise the modular wrap.
        let w = 5;
        let h = 4;
        let plane: Vec<u8> = (0..(w * h))
            .map(|i| (i as u8).wrapping_mul(13).wrapping_add(0x42))
            .collect();
        let residuals = forward_median(&plane, w, h);
        let mut out = residuals.clone();
        apply_inverse_8bit(Predictor::Median, &mut out, w, h);
        assert_eq!(out, plane, "median inverse must undo its forward");
    }

    #[test]
    fn median_diverges_from_jpeg_ls_on_overflow() {
        // Trace doc §8.1 divergence example: A=100, B=200, C=10.
        // A + B − C = 290 wraps to 34 (mod 256); 3-way median of
        // (100, 200, 34) is 100. Ut Video predicts 100; JPEG-LS
        // would predict 200. We verify that our `mid_pred` matches
        // the Ut Video answer.
        assert_eq!(mid_pred(100, 200, 10), 100);
    }

    #[test]
    fn rgb_g_centred_round_trip() {
        // Encode side: B' = B - G - 0x80, R' = R - G - 0x80.
        let g = vec![100u8, 50, 200];
        let b = vec![120u8, 80, 50];
        let r = vec![60u8, 70, 220];
        let bp_enc: Vec<u8> = b
            .iter()
            .zip(&g)
            .map(|(b, g)| b.wrapping_sub(*g).wrapping_sub(0x80))
            .collect();
        let rp_enc: Vec<u8> = r
            .iter()
            .zip(&g)
            .map(|(r, g)| r.wrapping_sub(*g).wrapping_sub(0x80))
            .collect();
        let mut bp = bp_enc.clone();
        let mut rp = rp_enc.clone();
        restore_g_centred_rgb(&g, &mut bp, &mut rp);
        assert_eq!(bp, b);
        assert_eq!(rp, r);
    }
}
