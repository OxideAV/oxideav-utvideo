//! Pixel-level inverse-prediction (decode) and forward-prediction
//! (encode) for the four Ut Video predictors per `spec/04`.
//!
//! Slice scan order is row-major top-down within a slice's display
//! row range; the per-slice first-pixel seed is 128 for **all four**
//! modes (`spec/04` §§3.1, 4, 5, 7); the slice's column-0 edge for
//! `r > r_start` differs per mode:
//!
//! - none: identity, no neighbour reference.
//! - left: continuous-wrap (predictor = `sample[r-1, W-1]`).
//! - gradient: top (predictor = `sample[r-1, 0]`).
//! - median: continuous-wrap MED with `A=sample[r-1,W-1]`,
//!   `B=sample[r-1,0]`, `C=sample[r-2,W-1]` (degenerates to `B` on
//!   row 1 of a slice when no `r-2` is available inside the slice).

use crate::fourcc::Predictor;

/// JPEG-LS MED helper. `P = clip_to_[min(A,B), max(A,B)] (A+B-C)`.
#[inline]
fn med(a: u8, b: u8, c: u8) -> u8 {
    let max = a.max(b);
    let min = a.min(b);
    if c >= max {
        min
    } else if c <= min {
        max
    } else {
        a.wrapping_add(b).wrapping_sub(c)
    }
}

/// Inverse-predict one slice's residuals into a `slice_rows x W` buffer.
/// `residual` is in scan order (row-major top-down within slice). For
/// the `Left` / `Gradient` / `Median` modes the column-0 edge inside
/// the slice consults the previous wire row from the **caller's full
/// plane**: pass the previous row's last sample (`prev_last_col`) and
/// the row two above's last sample (`prev_prev_last_col`) when the
/// slice itself does not yet contain them.
///
/// In practice — for round 1 — the implementation **always** scans the
/// plane top-down across slices, so by the time a slice is decoded
/// the previous slice's last row is already in the per-plane output
/// buffer. The slice-decoder convenience wrapper (`apply`) takes
/// care of the slice → plane composition.
pub fn apply(
    pred: Predictor,
    plane: &mut [u8],
    width: usize,
    plane_height: usize,
    num_slices: usize,
    slice_residuals: &[Vec<u8>],
) {
    debug_assert_eq!(slice_residuals.len(), num_slices);
    debug_assert_eq!(plane.len(), width * plane_height);
    for (s_idx, residuals) in slice_residuals.iter().enumerate() {
        let r_start = (plane_height * s_idx) / num_slices;
        let r_end = (plane_height * (s_idx + 1)) / num_slices;
        let rows = r_end - r_start;
        debug_assert_eq!(residuals.len(), rows * width);
        match pred {
            Predictor::None => apply_none(plane, width, r_start, r_end, residuals),
            Predictor::Left => apply_left(plane, width, r_start, r_end, residuals),
            Predictor::Gradient => apply_gradient(plane, width, r_start, r_end, residuals),
            Predictor::Median => apply_median(plane, width, r_start, r_end, residuals),
        }
    }
}

fn apply_none(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    let mut i = 0usize;
    for r in r_start..r_end {
        for c in 0..width {
            plane[r * width + c] = residuals[i];
            i += 1;
        }
    }
}

fn apply_left(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    let mut prev: u8 = 128;
    let mut i = 0usize;
    for r in r_start..r_end {
        for c in 0..width {
            let s = residuals[i].wrapping_add(prev);
            plane[r * width + c] = s;
            prev = s;
            i += 1;
        }
    }
}

fn apply_gradient(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    let mut i = 0usize;
    for r in r_start..r_end {
        for c in 0..width {
            let p: u8 = if r == r_start && c == 0 {
                128
            } else if r == r_start {
                plane[r * width + c - 1]
            } else if c == 0 {
                plane[(r - 1) * width]
            } else {
                let a = plane[r * width + c - 1];
                let b = plane[(r - 1) * width + c];
                let c2 = plane[(r - 1) * width + c - 1];
                a.wrapping_add(b).wrapping_sub(c2)
            };
            plane[r * width + c] = residuals[i].wrapping_add(p);
            i += 1;
        }
    }
}

fn apply_median(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    let mut i = 0usize;
    for r in r_start..r_end {
        for c in 0..width {
            let p: u8 = if r == r_start && c == 0 {
                128
            } else if r == r_start {
                plane[r * width + c - 1]
            } else if c == 0 {
                let a = plane[(r - 1) * width + (width - 1)];
                let b = plane[(r - 1) * width];
                if r == r_start + 1 {
                    b
                } else {
                    let c2 = plane[(r - 2) * width + (width - 1)];
                    med(a, b, c2)
                }
            } else {
                let a = plane[r * width + c - 1];
                let b = plane[(r - 1) * width + c];
                let c2 = plane[(r - 1) * width + c - 1];
                med(a, b, c2)
            };
            plane[r * width + c] = residuals[i].wrapping_add(p);
            i += 1;
        }
    }
}

/// Forward predictor: emit residuals from a plane in slice order.
/// Mirror of [`apply`]. Encode side.
pub fn forward(
    pred: Predictor,
    plane: &[u8],
    width: usize,
    plane_height: usize,
    num_slices: usize,
) -> Vec<Vec<u8>> {
    debug_assert_eq!(plane.len(), width * plane_height);
    let mut out = Vec::with_capacity(num_slices);
    for s_idx in 0..num_slices {
        let r_start = (plane_height * s_idx) / num_slices;
        let r_end = (plane_height * (s_idx + 1)) / num_slices;
        let rows = r_end - r_start;
        let mut residuals = Vec::with_capacity(rows * width);
        match pred {
            Predictor::None => {
                for r in r_start..r_end {
                    for c in 0..width {
                        residuals.push(plane[r * width + c]);
                    }
                }
            }
            Predictor::Left => {
                let mut prev: u8 = 128;
                for r in r_start..r_end {
                    for c in 0..width {
                        let s = plane[r * width + c];
                        residuals.push(s.wrapping_sub(prev));
                        prev = s;
                    }
                }
            }
            Predictor::Gradient => {
                for r in r_start..r_end {
                    for c in 0..width {
                        let p: u8 = if r == r_start && c == 0 {
                            128
                        } else if r == r_start {
                            plane[r * width + c - 1]
                        } else if c == 0 {
                            plane[(r - 1) * width]
                        } else {
                            let a = plane[r * width + c - 1];
                            let b = plane[(r - 1) * width + c];
                            let c2 = plane[(r - 1) * width + c - 1];
                            a.wrapping_add(b).wrapping_sub(c2)
                        };
                        residuals.push(plane[r * width + c].wrapping_sub(p));
                    }
                }
            }
            Predictor::Median => {
                for r in r_start..r_end {
                    for c in 0..width {
                        let p: u8 = if r == r_start && c == 0 {
                            128
                        } else if r == r_start {
                            plane[r * width + c - 1]
                        } else if c == 0 {
                            let a = plane[(r - 1) * width + (width - 1)];
                            let b = plane[(r - 1) * width];
                            if r == r_start + 1 {
                                b
                            } else {
                                let c2 = plane[(r - 2) * width + (width - 1)];
                                med(a, b, c2)
                            }
                        } else {
                            let a = plane[r * width + c - 1];
                            let b = plane[(r - 1) * width + c];
                            let c2 = plane[(r - 1) * width + c - 1];
                            med(a, b, c2)
                        };
                        residuals.push(plane[r * width + c].wrapping_sub(p));
                    }
                }
            }
        }
        out.push(residuals);
    }
    out
}

/// RGB inverse decorrelation per `spec/04` §6: the encoder stored
/// `B' = (B - G + 128) mod 256` and `R' = (R - G + 128) mod 256`;
/// this undoes the +128 offset and the green subtraction. Operates
/// in-place on B and R planes; the G plane is untouched.
pub fn inverse_decorrelate_rgb(g: &[u8], b: &mut [u8], r: &mut [u8]) {
    debug_assert_eq!(g.len(), b.len());
    debug_assert_eq!(g.len(), r.len());
    for ((bp, rp), gp) in b.iter_mut().zip(r.iter_mut()).zip(g.iter()) {
        *bp = (*bp).wrapping_add(*gp).wrapping_sub(128);
        *rp = (*rp).wrapping_add(*gp).wrapping_sub(128);
    }
}

/// Forward RGB decorrelation: G stays, `B = (B - G + 128) mod 256`,
/// `R = (R - G + 128) mod 256`. Encoder side.
pub fn forward_decorrelate_rgb(g: &[u8], b: &mut [u8], r: &mut [u8]) {
    debug_assert_eq!(g.len(), b.len());
    debug_assert_eq!(g.len(), r.len());
    for ((bp, rp), gp) in b.iter_mut().zip(r.iter_mut()).zip(g.iter()) {
        *bp = (*bp).wrapping_sub(*gp).wrapping_add(128);
        *rp = (*rp).wrapping_sub(*gp).wrapping_add(128);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(pred: Predictor, w: usize, h: usize, slices: usize) {
        // Synthesise a deterministic plane and verify forward / inverse round-trip.
        let mut plane: Vec<u8> = (0..w * h).map(|i| ((i * 17 + 3) & 0xff) as u8).collect();
        let residuals = forward(pred, &plane, w, h, slices);
        let mut decoded = vec![0u8; w * h];
        apply(pred, &mut decoded, w, h, slices, &residuals);
        if decoded != plane {
            for (i, (a, b)) in decoded.iter().zip(plane.iter()).enumerate() {
                if a != b {
                    panic!("plane mismatch at {} ({} vs {}) for {:?}", i, a, b, pred);
                }
            }
        }
        // Mutate to make sure assertions actually run.
        plane[0] = plane[0].wrapping_add(0);
        let _ = plane;
    }

    #[test]
    fn left_round_trip_single_slice() {
        round_trip(Predictor::Left, 16, 16, 1);
    }

    #[test]
    fn left_round_trip_multi_slice() {
        round_trip(Predictor::Left, 16, 16, 4);
    }

    #[test]
    fn none_round_trip() {
        round_trip(Predictor::None, 16, 16, 1);
        round_trip(Predictor::None, 16, 16, 8);
    }

    #[test]
    fn gradient_round_trip_various_slices() {
        for slices in [1, 2, 4, 8] {
            round_trip(Predictor::Gradient, 16, 16, slices);
        }
    }

    #[test]
    fn median_round_trip_various_slices() {
        for slices in [1, 2, 4, 8] {
            round_trip(Predictor::Median, 16, 16, slices);
        }
    }

    #[test]
    fn rgb_decorrelation_round_trip() {
        let g: Vec<u8> = (0..256).map(|x| x as u8).collect();
        let mut b: Vec<u8> = (0..256).map(|x| ((x * 7) & 0xff) as u8).collect();
        let mut r: Vec<u8> = (0..256).map(|x| ((x * 13) & 0xff) as u8).collect();
        let b0 = b.clone();
        let r0 = r.clone();
        forward_decorrelate_rgb(&g, &mut b, &mut r);
        inverse_decorrelate_rgb(&g, &mut b, &mut r);
        assert_eq!(b, b0);
        assert_eq!(r, r0);
    }

    #[test]
    fn left_first_pixel_seed_128_per_slice() {
        // Per spec/04 §4.1.1: with `-pred left` and slice-local
        // first-pixel seed = 128, encoding a constant-0 plane gives
        // residual stream [128, 0, 0, ...].
        let plane = vec![0u8; 16 * 16];
        let residuals = forward(Predictor::Left, &plane, 16, 16, 1);
        assert_eq!(residuals.len(), 1);
        assert_eq!(residuals[0][0], 128);
        for &r in &residuals[0][1..] {
            assert_eq!(r, 0);
        }
    }
}
