//! Committed pins for the 4:2:0 slice partition.
//!
//! A `ULY0` slice always holds whole chroma rows: luma slice boundaries
//! sit on even luma rows (`Fourcc::slice_row_step`), so luma slice `n`
//! covers rows `2 * floor(Hc * n / S) .. 2 * floor(Hc * (n + 1) / S)`
//! with `Hc` the chroma height. For a slice count that does not divide
//! the chroma height this differs from applying `spec/02` §5.2 to the
//! luma height directly — which is what this crate did before, and why
//! it refused or mis-decoded such streams from the black-box encoder
//! and wrote streams the black-box decoder mis-decoded.
//!
//! The three fixtures under `tests/fixtures/slice_partition_420/` are
//! frame bodies written by the black-box encoder (fixture provenance:
//! `ffmpeg -c:v utvideo -pred <p> -slices <n>` on a raw `yuv420p`
//! picture), their 16-byte stream cookie, and the picture itself, which
//! the black-box decoder reproduces exactly.

use oxideav_utvideo::fourcc::slice_row_range;
use oxideav_utvideo::{
    EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig, decode_frame,
    decode_frame_strict, encode_frame, peek_frame,
};

struct Pin {
    name: &'static str,
    w: u32,
    h: u32,
    slices: usize,
    predictor: Predictor,
}

const PINS: [Pin; 3] = [
    Pin {
        name: "uly0_4x12_left_s5",
        w: 4,
        h: 12,
        slices: 5,
        predictor: Predictor::Left,
    },
    Pin {
        name: "uly0_16x16_median_s3",
        w: 16,
        h: 16,
        slices: 3,
        predictor: Predictor::Median,
    },
    Pin {
        name: "uly0_6x20_none_s7",
        w: 6,
        h: 20,
        slices: 7,
        predictor: Predictor::None,
    },
];

fn load(name: &str, ext: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/slice_partition_420/{name}.{ext}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn planes_of(pin: &Pin, pixels: &[u8]) -> Vec<Vec<u8>> {
    let mut at = 0;
    (0..3)
        .map(|i| {
            let (pw, ph) = Fourcc::Uly0.plane_dim(i, pin.w, pin.h);
            let n = (pw * ph) as usize;
            let v = pixels[at..at + n].to_vec();
            at += n;
            v
        })
        .collect()
}

#[test]
fn luma_slices_start_on_even_rows() {
    // 16 luma rows, 8 chroma rows, 3 slices: chroma 0..2..5..8, luma
    // 0..4..10..16 (not floor(16n/3) = 0..5..10..16).
    let got: Vec<_> = (0..3)
        .map(|n| Fourcc::Uly0.slice_rows(0, 16, 3, n))
        .collect();
    assert_eq!(got, [(0, 4), (4, 10), (10, 16)]);
    let chroma: Vec<_> = (0..3)
        .map(|n| Fourcc::Uly0.slice_rows(1, 16, 3, n))
        .collect();
    assert_eq!(chroma, [(0, 2), (2, 5), (5, 8)]);
    // Every other layout partitions its own plane height.
    for fc in [Fourcc::Uly2, Fourcc::Uly4, Fourcc::Ulrg, Fourcc::Ulra] {
        for idx in 0..fc.plane_count() {
            assert_eq!(fc.slice_row_step(idx), 1);
            let got: Vec<_> = (0..3).map(|n| fc.slice_rows(idx, 16, 3, n)).collect();
            assert_eq!(got, [(0, 5), (5, 10), (10, 16)], "{fc:?} plane {idx}");
        }
    }
}

#[test]
fn partition_tiles_every_plane() {
    for step in [1usize, 2] {
        for h in (step..=40).step_by(step) {
            for s in 1..=(h / step).min(40) {
                let mut at = 0;
                for n in 0..s {
                    let (a, b) = slice_row_range(h, step, s, n);
                    assert_eq!(a, at, "h={h} step={step} s={s} n={n}");
                    assert!(b > a, "empty slice h={h} step={step} s={s} n={n}");
                    assert_eq!(a % step, 0);
                    at = b;
                }
                assert_eq!(at, h);
            }
        }
    }
    // Total on out-of-range input.
    assert_eq!(slice_row_range(10, 2, 0, 0), (10, 10));
    assert_eq!(slice_row_range(10, 2, 3, 7), (10, 10));
    assert_eq!(slice_row_range(10, 0, 2, 1), (5, 10));
}

#[test]
fn foreign_420_streams_decode_exactly() {
    for pin in &PINS {
        let extradata = Extradata::parse(&load(pin.name, "extradata")).unwrap();
        assert_eq!(extradata.num_slices(), pin.slices, "{}", pin.name);
        let cfg = StreamConfig::new(Fourcc::Uly0, pin.w, pin.h, extradata).unwrap();
        let chunk = load(pin.name, "chunk");
        let want = planes_of(pin, &load(pin.name, "pixels"));

        let frame = decode_frame(&cfg, &chunk).unwrap_or_else(|e| panic!("{}: {e}", pin.name));
        assert_eq!(frame.predictor, pin.predictor, "{}", pin.name);
        for (i, plane) in frame.planes.iter().enumerate() {
            assert_eq!(plane.samples, want[i], "{} plane {i}", pin.name);
        }
        assert_eq!(decode_frame_strict(&cfg, &chunk).unwrap(), frame);

        // The decode-free walk reports the same row ranges.
        let layout = peek_frame(&cfg, &chunk).unwrap();
        for (i, pl) in layout.planes.iter().enumerate() {
            for (n, sl) in pl.slices.iter().enumerate() {
                let (a, b) = Fourcc::Uly0.slice_rows(i, pin.h, pin.slices, n);
                assert_eq!((sl.row_start as usize, sl.row_end as usize), (a, b));
            }
        }
    }
}

#[test]
fn own_420_streams_use_the_same_partition() {
    // Re-encoding the pinned picture with the pinned options yields
    // per-slice byte extents of the same shape as the foreign stream:
    // identical slice-end tables mean identical row partitions (both
    // encoders pad each slice to a 32-bit word).
    for pin in &PINS {
        let want = planes_of(pin, &load(pin.name, "pixels"));
        let frame = EncodedFrame::new(
            Fourcc::Uly0,
            pin.w,
            pin.h,
            pin.predictor,
            pin.slices,
            want.iter().cloned().map(PlaneInput::new).collect(),
        );
        let ours = encode_frame(&frame).unwrap();
        let cfg = StreamConfig::new(
            Fourcc::Uly0,
            pin.w,
            pin.h,
            Extradata::canonical_extradata_for(Fourcc::Uly0, pin.slices).unwrap(),
        )
        .unwrap();
        let back = decode_frame(&cfg, &ours).unwrap();
        for (i, plane) in back.planes.iter().enumerate() {
            assert_eq!(plane.samples, want[i], "{} plane {i}", pin.name);
        }
        let ours_layout = peek_frame(&cfg, &ours).unwrap();
        let theirs_layout = peek_frame(&cfg, &load(pin.name, "chunk")).unwrap();
        for (a, b) in ours_layout.planes.iter().zip(&theirs_layout.planes) {
            for (x, y) in a.slices.iter().zip(&b.slices) {
                assert_eq!((x.row_start, x.row_end), (y.row_start, y.row_end));
                assert_eq!(x.pixel_count, y.pixel_count);
            }
        }
    }
}
