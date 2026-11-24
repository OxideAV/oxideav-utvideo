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

use crate::fourcc::{Predictor, slice_row_range};

/// Ut Video median predictor (`spec/04` §5.0):
/// `P = median(A, B, (A + B - C) mod 256)`.
///
/// The third argument to the median is the *modular* gradient
/// `G = (A + B - C) mod 256`, **not** the raw top-left `C`. In real
/// arithmetic `median(A, B, A+B-C)` collapses to the JPEG-LS T.87 MED
/// `clip_[min(A,B), max(A,B)](A+B-C)`, so the two agree whenever
/// `A + B - C` stays inside `0..=255`. They **diverge** the moment the
/// gradient overflows or underflows the byte range: the reference
/// codec reduces `A+B-C` modulo 256 first and then takes the median of
/// the three bytes, which the clip form cannot reproduce (a clip can
/// only ever return `A`, `B`, or the in-range gradient, never a wrapped
/// value that lands outside `[min(A,B), max(A,B)]`). `spec/04` §5.0
/// calls this out explicitly ("one of the JPEG-LS family of predictors
/// but **not** the JPEG-LS MED defined in ITU-T T.87 §A.2").
#[inline]
fn med(a: u8, b: u8, c: u8) -> u8 {
    let g = a.wrapping_add(b).wrapping_sub(c);
    median3(a, b, g)
}

/// Median of three bytes as `max(min(a, b), min(max(a, b), c))`: the
/// two operations on `(a, b)` alone are independent of `c`, which keeps
/// the serial dependency through the inverse predictor short.
#[inline]
fn median3(a: u8, b: u8, c: u8) -> u8 {
    a.min(b).max(a.max(b).min(c))
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
        apply_slice(
            pred,
            &mut plane[r_start * width..r_end * width],
            width,
            rows,
            residuals,
        );
    }
}

/// Apply inverse prediction to **one** slice's row strip in isolation.
/// `strip` is exactly `rows * width` bytes, treated as row 0..rows of
/// an independent slice with the universal `+128` first-pixel seed
/// (per `spec/04` §§3.1, 4, 5, 7 — every slice's column 0 of row 0 is
/// `residual + 128`). All inter-row references inside the slice live
/// inside `strip`; nothing outside the strip is read or written.
pub fn apply_slice(pred: Predictor, strip: &mut [u8], width: usize, rows: usize, residuals: &[u8]) {
    debug_assert_eq!(strip.len(), width * rows);
    debug_assert_eq!(residuals.len(), width * rows);
    strip.copy_from_slice(residuals);
    apply_slice_in_place(pred, strip, width, rows);
}

/// Inverse prediction of one slice **in place**: on entry `strip` holds
/// the slice's `rows * width` residuals in scan order, on return the
/// reconstructed samples. This is what the decoder runs — the entropy
/// stage writes residuals straight into the output plane, so no
/// separate residual buffer exists.
pub fn apply_slice_in_place(pred: Predictor, strip: &mut [u8], width: usize, rows: usize) {
    debug_assert_eq!(strip.len(), width * rows);
    if width == 0 || rows == 0 {
        return;
    }
    match pred {
        Predictor::None => {}
        Predictor::Left => undo_left(strip, 128),
        Predictor::Gradient => {
            let (first, mut rest) = strip.split_at_mut(width);
            undo_left(first, 128);
            let mut above: &[u8] = first;
            while !rest.is_empty() {
                let (row, tail) = rest.split_at_mut(width);
                // Column 0 predicts from the sample above (`spec/04`
                // §7.1); the interior adds `left + top - top_left`.
                let mut left = row[0].wrapping_add(above[0]);
                row[0] = left;
                for ((d, &top), &top_left) in row[1..].iter_mut().zip(&above[1..]).zip(above) {
                    // `top - top_left + residual` does not depend on the
                    // running value: one add on the serial chain.
                    left = left.wrapping_add(d.wrapping_add(top).wrapping_sub(top_left));
                    *d = left;
                }
                above = row;
                rest = tail;
            }
        }
        Predictor::Median => {
            let (first, mut rest) = strip.split_at_mut(width);
            undo_left(first, 128);
            let mut above: &[u8] = first;
            // Last sample of the row two above, once there is one.
            let mut two_above_last: Option<u8> = None;
            while !rest.is_empty() {
                let (row, tail) = rest.split_at_mut(width);
                // Column 0 (`spec/04` §5.0): the sample above on the
                // slice's second row, else the median taken with the
                // previous row's last sample as "left" and the last
                // sample of the row before that as "top-left".
                let above_last = above[width - 1];
                let p0 = match two_above_last {
                    None => above[0],
                    Some(c0) => med(above_last, above[0], c0),
                };
                let mut left = row[0].wrapping_add(p0);
                row[0] = left;
                for ((d, &top), &top_left) in row[1..].iter_mut().zip(&above[1..]).zip(above) {
                    left = d.wrapping_add(med(left, top, top_left));
                    *d = left;
                }
                two_above_last = Some(above_last);
                above = row;
                rest = tail;
            }
        }
    }
}

/// Running sum: `out[i] = seed + in[0] + … + in[i]` (mod 256), in place.
#[inline]
fn undo_left(run: &mut [u8], seed: u8) {
    let mut prev = seed;
    for d in run {
        prev = prev.wrapping_add(*d);
        *d = prev;
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
    forward_on_row_grid(pred, plane, width, plane_height, 1, num_slices)
}

/// [`forward`] with slice boundaries placed on multiples of `row_step`
/// rows (2 for the luma plane of a 4:2:0 stream, 1 otherwise — see
/// [`crate::fourcc::Fourcc::slice_row_step`]).
pub fn forward_on_row_grid(
    pred: Predictor,
    plane: &[u8],
    width: usize,
    plane_height: usize,
    row_step: usize,
    num_slices: usize,
) -> Vec<Vec<u8>> {
    debug_assert_eq!(plane.len(), width * plane_height);
    let mut out = Vec::with_capacity(num_slices);
    for s_idx in 0..num_slices {
        let (r_start, r_end) = slice_row_range(plane_height, row_step, num_slices, s_idx);
        out.push(forward_slice(pred, plane, width, r_start, r_end));
    }
    out
}

/// Forward predictor on **one** slice's row range of a plane. The
/// slice's first-pixel seed is always `128` (`spec/04` §§3.1, 4, 5, 7),
/// matching [`apply_slice`] on the decode side; the slice reads samples
/// only from rows `r_start..r_end` of `plane`, so the work is fully
/// independent across slices and is what the round-5 parallel encoder
/// dispatches to per-thread workers.
pub fn forward_slice(
    pred: Predictor,
    plane: &[u8],
    width: usize,
    r_start: usize,
    r_end: usize,
) -> Vec<u8> {
    let rows = r_end - r_start;
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let strip = &plane[r_start * width..r_end * width];
    if pred == Predictor::None {
        return strip.to_vec();
    }
    let mut residuals = vec![0u8; rows * width];
    // Every loop below reads only source samples (never its own
    // output), over equal-length zipped slices: no bounds checks, and
    // the subtractions vectorise.
    let (first_src, _) = strip.split_at(width);
    match pred {
        Predictor::None => {}
        Predictor::Left => {
            // Continuous within the slice: the predictor of every sample
            // but the first is the previous sample in scan order.
            diff_left(&mut residuals, strip, 128);
        }
        Predictor::Gradient => {
            diff_left(&mut residuals[..width], first_src, 128);
            for ((dst, row), above) in residuals
                .chunks_exact_mut(width)
                .zip(strip.chunks_exact(width))
                .skip(1)
                .zip(strip.chunks_exact(width))
            {
                dst[0] = row[0].wrapping_sub(above[0]);
                for ((((d, &cur), &left), &top), &top_left) in dst[1..]
                    .iter_mut()
                    .zip(&row[1..])
                    .zip(row)
                    .zip(&above[1..])
                    .zip(above)
                {
                    *d = cur.wrapping_sub(left.wrapping_add(top).wrapping_sub(top_left));
                }
            }
        }
        Predictor::Median => {
            diff_left(&mut residuals[..width], first_src, 128);
            let mut two_above_last: Option<u8> = None;
            for ((dst, row), above) in residuals
                .chunks_exact_mut(width)
                .zip(strip.chunks_exact(width))
                .skip(1)
                .zip(strip.chunks_exact(width))
            {
                let above_last = above[width - 1];
                let p0 = match two_above_last {
                    None => above[0],
                    Some(c0) => med(above_last, above[0], c0),
                };
                dst[0] = row[0].wrapping_sub(p0);
                for ((((d, &cur), &left), &top), &top_left) in dst[1..]
                    .iter_mut()
                    .zip(&row[1..])
                    .zip(row)
                    .zip(&above[1..])
                    .zip(above)
                {
                    *d = cur.wrapping_sub(med(left, top, top_left));
                }
                two_above_last = Some(above_last);
            }
        }
    }
    residuals
}

/// `dst[0] = src[0] - seed`, `dst[i] = src[i] - src[i - 1]` (mod 256).
#[inline]
fn diff_left(dst: &mut [u8], src: &[u8], seed: u8) {
    debug_assert_eq!(dst.len(), src.len());
    let Some((d0, rest)) = dst.split_first_mut() else {
        return;
    };
    *d0 = src[0].wrapping_sub(seed);
    for ((d, &cur), &prev) in rest.iter_mut().zip(&src[1..]).zip(src) {
        *d = cur.wrapping_sub(prev);
    }
}

/// Cross-entropy proxy over the residual symbol distribution of a
/// sampled row-strip — used by [`choose_predictor`] to pick the
/// predictor that the per-plane Huffman code will compress shortest.
///
/// Returns `Σ count[s] · log2(N / count[s])` (Shannon entropy of the
/// 256-bin histogram, scaled by total sample count). The Huffman
/// code-length lower bound IS the entropy; the package-merge code we
/// build in `encoder::build_lengths` typically lands within a fraction
/// of a bit per symbol of that bound, so a lower entropy reliably
/// predicts a smaller per-plane wire blob.
///
/// `samples` must be non-empty.
fn entropy_proxy(residuals: &[u8]) -> f64 {
    debug_assert!(!residuals.is_empty());
    let mut counts = [0u32; 256];
    for &r in residuals {
        counts[r as usize] += 1;
    }
    let n = residuals.len() as f64;
    let log2_n = n.log2();
    let mut bits = 0.0_f64;
    for &c in &counts {
        if c > 0 {
            let cf = c as f64;
            // `c * log2(N / c)` = `c * log2(N) - c * log2(c)`.
            bits += cf * log2_n - cf * cf.log2();
        }
    }
    bits
}

/// Pick the predictor (None / Left / Gradient / Median) that minimises
/// the Huffman-code entropy lower bound on a representative sample of
/// `plane`. Used by the [`oxideav_core::Encoder`] trait path to switch
/// from the round-17 hardcoded `Gradient` default to a content-adaptive
/// per-frame choice; the direct [`crate::encode_frame`] API still
/// accepts an explicit [`Predictor`] verbatim.
///
/// The heuristic samples up to [`HEURISTIC_SAMPLE_ROWS`] rows starting
/// from row 0 of the plane (the slice-0 +128 seed convention of all
/// four predictors per `spec/04` §§3.1, 4, 5, 7 means the row-0
/// residuals genuinely characterise the predictor's behaviour). Each
/// predictor's per-row residuals are computed via [`forward_slice`]
/// over the sampled-row range; the predictor with the lowest
/// [`entropy_proxy`] wins.
///
/// Ties (within `1e-6` bits) break in the order
/// `Gradient → Median → Left → None`: this matches the round-11
/// benchmark ordering where Gradient was the fastest dense kernel
/// AND most often the best compressor on natural content; it
/// degrades gracefully to Median on JPEG-LS-MED-friendly inputs and
/// to Left on row-correlated content.
///
/// `width` and `plane_height` describe the plane's true dimensions
/// (post-RGB-decorrelation for ULRG/ULRA's B and R planes); the
/// caller must pass the same `plane` slice it will hand to
/// [`forward`]. Returns [`Predictor::Gradient`] as a safe fallback
/// when the plane has zero rows or zero columns (degenerate inputs
/// where every predictor produces an empty residual stream).
pub fn choose_predictor(plane: &[u8], width: usize, plane_height: usize) -> Predictor {
    if width == 0 || plane_height == 0 {
        return Predictor::Gradient;
    }
    debug_assert_eq!(plane.len(), width * plane_height);

    // Sample the first `HEURISTIC_SAMPLE_ROWS` rows. The +128 first-pixel
    // seed dominates the column-0 statistics of every predictor, so the
    // top rows are representative of the whole slice; sampling more rows
    // would barely improve the choice and slow the heuristic on large
    // frames.
    let sample_rows = HEURISTIC_SAMPLE_ROWS.min(plane_height);

    // Candidate order pins the tie-break preference.
    let candidates = [
        Predictor::Gradient,
        Predictor::Median,
        Predictor::Left,
        Predictor::None,
    ];

    let mut best = Predictor::Gradient;
    let mut best_bits = f64::INFINITY;
    for &p in &candidates {
        let residuals = forward_slice(p, plane, width, 0, sample_rows);
        if residuals.is_empty() {
            // Defensive: forward_slice over a 1-pixel sample CAN happen
            // for width=plane_height=1; entropy of one symbol is zero so
            // every predictor "ties". Stick with the first-preferred.
            continue;
        }
        let bits = entropy_proxy(&residuals);
        // Tie-break: strict `<` keeps the first-encountered candidate
        // (Gradient → Median → Left → None) on equal entropies.
        if bits + 1e-6 < best_bits {
            best_bits = bits;
            best = p;
        }
    }
    best
}

/// Row-sample budget for [`choose_predictor`]. Set to 8 rows: the
/// universal per-slice +128 first-pixel seed (`spec/04` §§3.1, 4, 5, 7)
/// makes the leading rows representative of the predictor's
/// downstream behaviour, and 8 rows × 1920 columns = 15 KiB of work per
/// predictor candidate × 4 predictors = 60 KiB total — negligible
/// compared to a full-frame Huffman pass at ≥160 MiB/s (round-11
/// benchmark baseline).
pub const HEURISTIC_SAMPLE_ROWS: usize = 8;

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
    fn median_uses_modular_gradient_not_clip() {
        // `spec/04` §5.0: Ut Video median is `median(A, B, (A+B-C) mod 256)`,
        // which diverges from the JPEG-LS T.87 clip predictor exactly when
        // the gradient `A+B-C` leaves the byte range. Concrete wrap case
        // (from the `uly0_median_grad_s1` reference frame at row 3 col 31):
        // A=249, B=243, C=236 → A+B-C = 256 → mod 0 → median(249,243,0)=243.
        // The clip form would instead return max(A,B)=249 because
        // C=236 <= min(A,B)=243.
        assert_eq!(med(249, 243, 236), 243);
        assert_eq!(median3(249, 243, 0), 243);

        // Underflow wrap: A=2, B=1, C=200 → 2+1-200 = -197 ≡ 59 (mod 256).
        // median(2, 1, 59) = 2. The clip form returns max(A,B)=2 here too,
        // so pick a case where they part: A=10, B=20, C=250 → 10+20-250 =
        // -220 ≡ 36 → median(10,20,36)=20; clip → C=250>=max(20) → min=10.
        assert_eq!(med(10, 20, 250), 20);

        // No-wrap agreement with clip: A=100, B=120, C=110 → 110 in range
        // → median(100,120,110)=110 == clip(A+B-C)=110.
        assert_eq!(med(100, 120, 110), 110);
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
