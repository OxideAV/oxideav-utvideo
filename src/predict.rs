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

/// Apply inverse prediction to **one** slice's row strip in isolation.
/// `strip` is exactly `rows * width` bytes, treated as row 0..rows of
/// an independent slice with the universal `+128` first-pixel seed
/// (per `spec/04` §§3.1, 4, 5, 7 — every slice's column 0 of row 0 is
/// `residual + 128`). All inter-row references inside the slice live
/// inside `strip`; nothing outside the strip is read or written.
///
/// This is the building block the round-4 slice-parallel decoder uses
/// — each slice owns a disjoint mutable strip and applies the
/// predictor in place.
pub fn apply_slice(pred: Predictor, strip: &mut [u8], width: usize, rows: usize, residuals: &[u8]) {
    debug_assert_eq!(strip.len(), width * rows);
    debug_assert_eq!(residuals.len(), width * rows);
    // The pre-existing per-mode helpers operate on a `(r_start, r_end)`
    // sub-range of a larger plane buffer. For a single-slice strip we
    // reuse them with `r_start = 0, r_end = rows` over the strip
    // itself — which is exactly what they would do for any independent
    // slice, by the spec's per-slice seed convention.
    match pred {
        Predictor::None => apply_none(strip, width, 0, rows, residuals),
        Predictor::Left => apply_left(strip, width, 0, rows, residuals),
        Predictor::Gradient => apply_gradient(strip, width, 0, rows, residuals),
        Predictor::Median => apply_median(strip, width, 0, rows, residuals),
    }
}

fn apply_none(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    if width == 0 || r_end == r_start {
        return;
    }
    debug_assert_eq!(residuals.len(), (r_end - r_start) * width);
    // Row-strided copy. `chunks_exact_mut(width)` over the slice-strip
    // gives the compiler a known row length so the inner copy is a
    // bounds-check-free `copy_from_slice` (memcpy intrinsic).
    let strip = &mut plane[r_start * width..r_end * width];
    let dst_rows = strip.chunks_exact_mut(width);
    let src_rows = residuals.chunks_exact(width);
    for (dst, src) in dst_rows.zip(src_rows) {
        dst.copy_from_slice(src);
    }
}

fn apply_left(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    if width == 0 || r_end == r_start {
        return;
    }
    debug_assert_eq!(residuals.len(), (r_end - r_start) * width);
    // Continuous-wrap Left across rows: `prev` is the running cumulative
    // running sum, seeded once at +128 per slice and carried row-to-row
    // (column 0 of row r reads the running prev = `sample[r-1, W-1]`).
    // Row-strided iteration so the inner add+store loop sees a fixed
    // `width` slice (bounds-check elided after the first access).
    let strip = &mut plane[r_start * width..r_end * width];
    let mut prev: u8 = 128;
    let dst_rows = strip.chunks_exact_mut(width);
    let src_rows = residuals.chunks_exact(width);
    for (dst, src) in dst_rows.zip(src_rows) {
        for (d, &r) in dst.iter_mut().zip(src.iter()) {
            let s = r.wrapping_add(prev);
            *d = s;
            prev = s;
        }
    }
}

fn apply_gradient(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    if width == 0 || r_end == r_start {
        return;
    }
    // Round 196: hoist the row-0 + column-0 branches out of the inner
    // loop so the dense interior runs branch-free. The first slice row
    // is a pure Left-predictor scan (seeded with 128); subsequent rows
    // start at column-0 with `above[0]` then run the
    // `a + b - c2` Gradient interior on `[1..width]`.
    let resid_len = (r_end - r_start) * width;
    debug_assert_eq!(residuals.len(), resid_len);

    // --- row 0 of the slice: Left-predictor (column 0 = +128 seed) ---
    {
        let row_off = r_start * width;
        let (row, rest) = plane[row_off..].split_at_mut(width);
        let _ = rest;
        let resid_row = &residuals[..width];
        let mut left: u8 = 128;
        for c in 0..width {
            let s = resid_row[c].wrapping_add(left);
            row[c] = s;
            left = s;
        }
    }

    // --- rows 1..rows: Gradient interior ---
    for r in (r_start + 1)..r_end {
        let resid_off = (r - r_start) * width;
        let resid_row = &residuals[resid_off..resid_off + width];

        // We need both the row above (read-only) and the current row
        // (write). `split_at_mut` at `r * width` yields exactly that
        // partition; the above row is the LAST `width` bytes of `head`,
        // the current row is the FIRST `width` bytes of `tail`.
        let (head, tail) = plane.split_at_mut(r * width);
        let above = &head[head.len() - width..];
        let row = &mut tail[..width];

        // Column 0: predictor = above[0].
        let p0 = above[0];
        row[0] = resid_row[0].wrapping_add(p0);

        // Columns 1..width: predictor = row[c-1] + above[c] - above[c-1].
        // Carry the live `left = row[c-1]` value to avoid re-reading the
        // store we just wrote (the dependency stays in a register).
        let mut left = row[0];
        for c in 1..width {
            let b = above[c];
            let c2 = above[c - 1];
            let p = left.wrapping_add(b).wrapping_sub(c2);
            let s = resid_row[c].wrapping_add(p);
            row[c] = s;
            left = s;
        }
    }
}

fn apply_median(plane: &mut [u8], width: usize, r_start: usize, r_end: usize, residuals: &[u8]) {
    if width == 0 || r_end == r_start {
        return;
    }
    // Round 196: hoist the row-0 + column-0 branches out of the inner
    // loop so the dense interior runs branch-free. The first slice row
    // is a pure Left-predictor scan (column-0 = +128 seed). Subsequent
    // rows pick the column-0 predictor (row 1 → above[0]; row > 1 →
    // MED(above[W-1], above[0], two-rows-above[W-1])) then run the
    // dense MED interior across `[1..width]`.
    let resid_len = (r_end - r_start) * width;
    debug_assert_eq!(residuals.len(), resid_len);

    // --- row 0 of the slice: Left-predictor (column 0 = +128 seed) ---
    {
        let row_off = r_start * width;
        let (row, rest) = plane[row_off..].split_at_mut(width);
        let _ = rest;
        let resid_row = &residuals[..width];
        let mut left: u8 = 128;
        for c in 0..width {
            let s = resid_row[c].wrapping_add(left);
            row[c] = s;
            left = s;
        }
    }

    // --- rows 1..rows: Median interior ---
    for r in (r_start + 1)..r_end {
        let resid_off = (r - r_start) * width;
        let resid_row = &residuals[resid_off..resid_off + width];

        // We need the row above (read-only) AND, for r > r_start + 1,
        // the row two above to construct the column-0 MED. Split the
        // plane buffer at `r * width`; the row above lives at the tail
        // of `head`; the row two above lives at `head[(r-1-1)*width..]`
        // when present.
        let (head, tail) = plane.split_at_mut(r * width);
        let above = &head[head.len() - width..];
        let two_above_last: Option<u8> = if r >= r_start + 2 {
            Some(head[head.len() - 2 * width + (width - 1)])
        } else {
            None
        };
        let row = &mut tail[..width];

        // Column 0: above[W-1] vs above[0] (vs two-above[W-1]) via MED.
        let a0 = above[width - 1];
        let b0 = above[0];
        let p0 = match two_above_last {
            None => b0,
            Some(c0) => med(a0, b0, c0),
        };
        row[0] = resid_row[0].wrapping_add(p0);

        // Columns 1..width: MED(row[c-1], above[c], above[c-1]).
        let mut left = row[0];
        for c in 1..width {
            let b = above[c];
            let c2 = above[c - 1];
            let p = med(left, b, c2);
            let s = resid_row[c].wrapping_add(p);
            row[c] = s;
            left = s;
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
    let mut residuals = Vec::with_capacity(rows * width);
    match pred {
        Predictor::None => {
            // Row-strided copy: extend the residual buffer with each row
            // as a contiguous slice (memcpy intrinsic, no per-pixel
            // bounds check).
            if width > 0 && r_end > r_start {
                let strip = &plane[r_start * width..r_end * width];
                for src in strip.chunks_exact(width) {
                    residuals.extend_from_slice(src);
                }
            }
        }
        Predictor::Left => {
            // Continuous-wrap Left: `prev` is the running cumulative
            // baseline, +128-seeded per slice and carried across rows.
            if width > 0 && r_end > r_start {
                let total = (r_end - r_start) * width;
                residuals.resize(total, 0);
                let strip = &plane[r_start * width..r_end * width];
                let mut prev: u8 = 128;
                let src_rows = strip.chunks_exact(width);
                let dst_rows = residuals.chunks_exact_mut(width);
                for (src, dst) in src_rows.zip(dst_rows) {
                    for (&s, d) in src.iter().zip(dst.iter_mut()) {
                        *d = s.wrapping_sub(prev);
                        prev = s;
                    }
                }
            }
        }
        Predictor::Gradient => {
            // Round 196: hoist branches out of the inner loop (mirror
            // of `apply_gradient`). Row 0 = Left-predictor (column 0
            // seed = +128); subsequent rows pick column 0 = above[0]
            // then run the dense `(a + b - c2)` predictor across
            // `[1..width]`.
            if width > 0 && r_end > r_start {
                let total = (r_end - r_start) * width;
                residuals.resize(total, 0);
                // Row 0
                {
                    let row = &plane[r_start * width..r_start * width + width];
                    let dst = &mut residuals[..width];
                    let mut prev: u8 = 128;
                    for c in 0..width {
                        let s = row[c];
                        dst[c] = s.wrapping_sub(prev);
                        prev = s;
                    }
                }
                // Rows 1..
                for r in (r_start + 1)..r_end {
                    let above = &plane[(r - 1) * width..(r - 1) * width + width];
                    let row = &plane[r * width..r * width + width];
                    let dst = &mut residuals[(r - r_start) * width..(r - r_start) * width + width];
                    // Column 0: predictor = above[0]
                    dst[0] = row[0].wrapping_sub(above[0]);
                    // Columns 1..: predictor = row[c-1] + above[c] - above[c-1]
                    for c in 1..width {
                        let p = row[c - 1].wrapping_add(above[c]).wrapping_sub(above[c - 1]);
                        dst[c] = row[c].wrapping_sub(p);
                    }
                }
            }
        }
        Predictor::Median => {
            // Round 196: hoist branches (mirror of `apply_median`).
            if width > 0 && r_end > r_start {
                let total = (r_end - r_start) * width;
                residuals.resize(total, 0);
                // Row 0: Left-predictor (column 0 seed = +128).
                {
                    let row = &plane[r_start * width..r_start * width + width];
                    let dst = &mut residuals[..width];
                    let mut prev: u8 = 128;
                    for c in 0..width {
                        let s = row[c];
                        dst[c] = s.wrapping_sub(prev);
                        prev = s;
                    }
                }
                // Rows 1..
                for r in (r_start + 1)..r_end {
                    let above = &plane[(r - 1) * width..(r - 1) * width + width];
                    let row = &plane[r * width..r * width + width];
                    let dst = &mut residuals[(r - r_start) * width..(r - r_start) * width + width];
                    // Column 0: row 1 → above[0]; row > 1 → MED(above[W-1], above[0], two-above[W-1]).
                    let a0 = above[width - 1];
                    let b0 = above[0];
                    let p0 = if r == r_start + 1 {
                        b0
                    } else {
                        let two_above = &plane[(r - 2) * width..(r - 2) * width + width];
                        med(a0, b0, two_above[width - 1])
                    };
                    dst[0] = row[0].wrapping_sub(p0);
                    // Columns 1..: predictor = MED(row[c-1], above[c], above[c-1]).
                    for c in 1..width {
                        let p = med(row[c - 1], above[c], above[c - 1]);
                        dst[c] = row[c].wrapping_sub(p);
                    }
                }
            }
        }
    }
    residuals
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
