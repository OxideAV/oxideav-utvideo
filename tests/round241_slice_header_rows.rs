//! Round 241 — typed slice-header `row_start` / `row_end` /
//! `pixel_count` accessor on [`oxideav_utvideo::SliceLayout`].
//!
//! These tests pin the partitioning rule from
//! `docs/video/utvideo/spec/02-frame-format.md` §5.2:
//!
//! ```text
//! row_start[s] = floor((plane_height * s)        / num_slices)
//! row_end[s]   = floor((plane_height * (s + 1))  / num_slices)
//! slice_pixel_count[s] = (row_end - row_start) * plane_width
//! ```
//!
//! The fields are populated decode-free by
//! [`oxideav_utvideo::peek_frame`]; we cross-check against:
//!
//! - the spec's worked example
//!   `R2-uly2-testsrc-16x17-s3` → `slice_rows = (5, 6, 6)`,
//! - the partition invariants
//!   `row_end[s] == row_start[s+1]`,
//!   `Σ slice.pixel_count == plane_width * plane_height`,
//! - the empty-slice edge case `num_slices > plane_height`
//!   (`row_count` collapses to zero per `spec/02` §5.1).

use oxideav_utvideo::{
    encode_frame, peek_frame, EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig,
};

fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
    StreamConfig::new(fc, w, h, extradata).unwrap()
}

fn xorshift_frame(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
    let plane_count = fc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for idx in 0..plane_count {
        let (pw, ph) = fc.plane_dim(idx, w, h);
        let mut state = 0x1234_5678u32 ^ (idx as u32).wrapping_mul(0x9E37_79B9);
        let mut samples = Vec::with_capacity((pw * ph) as usize);
        for _ in 0..(pw * ph) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            samples.push((state & 0xff) as u8);
        }
        planes.push(PlaneInput { samples });
    }
    let frame = EncodedFrame {
        fourcc: fc,
        width: w,
        height: h,
        predictor: pred,
        num_slices: slices,
        planes,
    };
    encode_frame(&frame).unwrap()
}

/// Spec §5.2 worked example: ULY2 16×17, S=3. Plane heights are
/// (17, 17, 17) — ULY2 doesn't subsample vertically — so every
/// plane's per-slice row count is `(5, 6, 6)`.
#[test]
fn r2_uly2_testsrc_16x17_s3_rows_match_worked_example() {
    let fc = Fourcc::Uly2;
    let (w, h) = (16u32, 17u32);
    let s = 3usize;
    let cfg = cfg_for(fc, w, h, s);
    let bytes = xorshift_frame(fc, w, h, s, Predictor::Left);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let rows: Vec<u32> = p.slices.iter().map(|sl| sl.row_count()).collect();
        assert_eq!(rows, vec![5, 6, 6], "plane {}", p.plane_idx);
        // pixel_count per slice = row_count * plane_width.
        for sl in &p.slices {
            assert_eq!(sl.pixel_count, sl.row_count() * p.width);
        }
        // First slice anchors at row 0; last slice ends at plane_height.
        assert_eq!(p.slices.first().unwrap().row_start, 0);
        assert_eq!(p.slices.last().unwrap().row_end, p.height);
    }
}

/// `row_end[s] == row_start[s + 1]` for every adjacent slice pair —
/// the partition is gap-free and overlap-free per `spec/02` §5.2.
#[test]
fn slice_rows_partition_is_gapless() {
    for &fc in &[
        Fourcc::Ulrg,
        Fourcc::Ulra,
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
    ] {
        for &(w, h, s) in &[(32u32, 32u32, 1usize), (32, 32, 4), (64, 48, 8)] {
            let cfg = cfg_for(fc, w, h, s);
            let bytes = xorshift_frame(fc, w, h, s, Predictor::Gradient);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                assert_eq!(p.slices.first().unwrap().row_start, 0);
                assert_eq!(p.slices.last().unwrap().row_end, p.height);
                for win in p.slices.windows(2) {
                    assert_eq!(
                        win[0].row_end, win[1].row_start,
                        "gap or overlap in plane {} (fc={:?}, w={}, h={}, s={})",
                        p.plane_idx, fc, w, h, s
                    );
                }
            }
        }
    }
}

/// `Σ slice.pixel_count == plane_width * plane_height` for every
/// plane. This is the typed counterpart of the existing
/// `total_slice_data_bytes()` cross-check.
#[test]
fn total_pixels_matches_plane_area() {
    for &fc in &[Fourcc::Uly0, Fourcc::Uly2, Fourcc::Uly4, Fourcc::Ulra] {
        for &(w, h, s) in &[(16u32, 16u32, 1usize), (32, 32, 4), (64, 48, 6)] {
            let cfg = cfg_for(fc, w, h, s);
            let bytes = xorshift_frame(fc, w, h, s, Predictor::None);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            for p in &layout.planes {
                let area = u64::from(p.width) * u64::from(p.height);
                assert_eq!(
                    p.total_pixels(),
                    area,
                    "plane {} (fc={:?}, w={}, h={}, s={})",
                    p.plane_idx,
                    fc,
                    w,
                    h,
                    s
                );
            }
        }
    }
}

/// `num_slices > plane_height` collapses some trailing slices to a
/// zero row count (`spec/02` §5.1). Use `ULY4` 16×3 with S=4 — plane
/// height 3 < 4 slices, so slice 0..2 each get 1 row and slice 3 gets
/// 0 rows; the last slice's `row_count` is 0 and its `pixel_count` is
/// 0 even though the slice extent exists.
#[test]
fn slice_count_above_plane_height_collapses_trailing_rows_to_zero() {
    // The in-crate encoder refuses `num_slices > plane height` since
    // round 382 (`Error::SliceCountExceedsPlaneHeight` — conformant
    // decoders reject zero-length slices in multi-symbol planes), so
    // the wire bytes are hand-crafted: 16×3 ULY4, 4 slices, each plane
    // a constant-zero surface under Left. Per plane: two-symbol
    // descriptor (`{0: 1, 128: 1}` → codes `128 → "0"`, `0 → "1"` per
    // spec/05 §2.2/§6.2), offsets `[0, 4, 8, 12]`, and three 4-byte
    // slice words (each populated slice = one 16-pixel row: first-pixel
    // residual 128 then fifteen zeros → bits `0` + `1`×15 → word
    // `0x7FFF_0000`). The decode-free inspector must stay lenient and
    // surface slice 0 as a zero-row, zero-length record.
    let fc = Fourcc::Uly4;
    let (w, h) = (16u32, 3u32);
    let s = 4usize;
    let cfg = cfg_for(fc, w, h, s);
    let mut plane = Vec::with_capacity(256 + 16 + 12);
    let mut desc = [255u8; 256];
    desc[0] = 1;
    desc[128] = 1;
    plane.extend_from_slice(&desc);
    for off in [0u32, 4, 8, 12] {
        plane.extend_from_slice(&off.to_le_bytes());
    }
    for _ in 0..3 {
        plane.extend_from_slice(&0x7FFF_0000u32.to_le_bytes());
    }
    let mut bytes = Vec::with_capacity(3 * plane.len() + 4);
    for _ in 0..3 {
        bytes.extend_from_slice(&plane);
    }
    bytes.extend_from_slice(&0x0000_0100u32.to_le_bytes()); // pred left
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        let rows: Vec<u32> = p.slices.iter().map(|sl| sl.row_count()).collect();
        // floor(3*0/4)=0, floor(3*1/4)=0, floor(3*2/4)=1,
        // floor(3*3/4)=2, floor(3*4/4)=3 → (0, 1, 1, 1).
        assert_eq!(rows, vec![0, 1, 1, 1], "plane {}", p.plane_idx);
        // Empty-row slice still has well-formed extent.
        let empty = &p.slices[0];
        assert_eq!(empty.row_start, 0);
        assert_eq!(empty.row_end, 0);
        assert_eq!(empty.pixel_count, 0);
        // Spec §5.1 invariant — empty bit-stream is legal.
        assert!(empty.is_empty(), "empty-row slice should have len() == 0");
    }
}

/// Plane chroma subsampling propagates into per-plane row partitions.
/// For ULY0 (4:2:0), the U/V planes have half the row count of Y,
/// so an even row count divides evenly; verify across S=2.
#[test]
fn yuv420_chroma_rows_track_subsampled_plane_height() {
    let fc = Fourcc::Uly0;
    let (w, h) = (16u32, 16u32);
    let s = 2usize;
    let cfg = cfg_for(fc, w, h, s);
    let bytes = xorshift_frame(fc, w, h, s, Predictor::Gradient);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    // Plane 0 (Y): 16 rows → 8 + 8.
    assert_eq!(layout.planes[0].height, 16);
    let y_rows: Vec<u32> = layout.planes[0]
        .slices
        .iter()
        .map(|sl| sl.row_count())
        .collect();
    assert_eq!(y_rows, vec![8, 8]);
    // Plane 1 (U) + plane 2 (V): each 8 rows → 4 + 4.
    for ch in &layout.planes[1..3] {
        assert_eq!(ch.height, 8);
        let r: Vec<u32> = ch.slices.iter().map(|sl| sl.row_count()).collect();
        assert_eq!(r, vec![4, 4]);
    }
}

/// `row_count` matches the `n_pixels / plane_width` argument shape
/// every per-slice Huffman call site uses (`spec/05` §6 pseudocode
/// `n_pixels = (r_end - r_start) * plane_width`).
#[test]
fn pixel_count_matches_n_pixels_huffman_argument() {
    let fc = Fourcc::Uly4;
    let (w, h) = (32u32, 24u32);
    let s = 4usize;
    let cfg = cfg_for(fc, w, h, s);
    let bytes = xorshift_frame(fc, w, h, s, Predictor::Median);
    let layout = peek_frame(&cfg, &bytes).unwrap();
    for p in &layout.planes {
        for sl in &p.slices {
            // n_pixels = (r_end - r_start) * plane_width.
            let expect = (sl.row_end - sl.row_start) * p.width;
            assert_eq!(sl.pixel_count, expect);
            // pixel_count is what HuffmanTable::decode_slice would
            // be asked to produce on the slice's byte range.
            assert_eq!(sl.pixel_count, sl.row_count() * p.width);
        }
    }
}
