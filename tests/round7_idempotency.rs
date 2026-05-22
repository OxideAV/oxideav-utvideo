//! Round 7 — encoder byte-stability (idempotency) + slice-count
//! boundary sweep at non-divisible heights.
//!
//! Every prior round asserts the *pixel* round-trip invariant
//! `decode ∘ encode == identity` (round-2 matrix, round-6 content
//! corpus, the in-crate `roundtrip_tests`). None of them assert the
//! complementary *byte* invariants the encoder must also hold:
//!
//! 1. **Deterministic encode.** `encode(x)` is a pure function of its
//!    input — calling it twice on the same frame, and across the
//!    serial / parallel / auto-dispatch entry points, yields
//!    byte-identical chunk payloads. This pins the Huffman tie-break
//!    (length DESC, sym DESC per `spec/05` §2.2) and the package-merge
//!    length build as deterministic, and proves the round-5
//!    slice-parallel encode is a bit-exact equivalent of the serial
//!    path — not merely "decodes to the same pixels".
//!
//! 2. **Encode is a fixed point under decode→re-encode.** Because the
//!    decoder reconstructs the exact source pixels (round-trip
//!    equality, established by every prior round), and the encoder is
//!    deterministic (invariant 1), the composed operation
//!    `encode ∘ decode ∘ encode` must reproduce the *first* encode's
//!    bytes exactly:
//!
//!        bytes1 = encode(frame)
//!        decoded = decode(bytes1)
//!        bytes2 = encode(frame_from(decoded))
//!        assert bytes1 == bytes2          // byte-stable fixed point
//!
//!    This is strictly stronger than pixel round-trip: a non-canonical
//!    Huffman build, an unstable sort, or a slice-partition that
//!    depended on un-zeroed scratch state would all pass pixel
//!    round-trip but break byte-stability. The codec is meant to be a
//!    *lossless transcode fixed point* — re-compressing an already
//!    decoded Ut Video frame must not drift.
//!
//! 3. **Slice-count boundary sweep at non-divisible heights.** The
//!    per-slice row split is `r_start = ph*s / N`, `r_end =
//!    ph*(s+1) / N` (`predict::forward` / decoder `parse_payload`).
//!    When `N` does not divide `ph`, slices carry uneven row counts;
//!    when `N > ph`, some slices carry **zero** rows (empty residual
//!    stream → zero slice-data bytes per `spec/02` §5.1). The existing
//!    suites only sweep `N ∈ {1,2,3,4,7,8}` against tame heights. This
//!    test sweeps the full documented range `N ∈ 1..=256` at heights
//!    deliberately chosen so `ph % N != 0` for most `N` (and `N > ph`
//!    for the tail), exercising the uneven-split and zero-row-slice
//!    paths across all four predictors and all five FOURCCs.
//!
//! Clean-room: all behaviour is derived from `docs/video/utvideo/spec/`
//! (slice split formula `spec/02` §5/§7, per-slice +128 seed `spec/04`
//! §§3.1/4/5/7, single-symbol zero-byte slice `spec/02` §5.1). No
//! external library source consulted. The xorshift PRNG below is a
//! standard well-known generator used purely as a deterministic
//! content source, not lifted from any codec implementation.

use oxideav_utvideo::encoder::{encode_frame_parallel, encode_frame_serial};
use oxideav_utvideo::{
    decode_frame, encode_frame, DecodedFrame, EncodedFrame, Extradata, Fourcc, PlaneInput,
    Predictor, StreamConfig,
};

// ----------------------------------------------------------------------
// Deterministic content source — xorshift64* (Vigna 2014), used as an
// opaque PRNG. Self-contained; no codec provenance.
// ----------------------------------------------------------------------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero fixed point.
        Xorshift64 { state: seed | 1 }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

fn all_fourccs() -> [Fourcc; 5] {
    [
        Fourcc::Uly0,
        Fourcc::Uly2,
        Fourcc::Uly4,
        Fourcc::Ulrg,
        Fourcc::Ulra,
    ]
}

fn all_predictors() -> [Predictor; 4] {
    [
        Predictor::None,
        Predictor::Left,
        Predictor::Gradient,
        Predictor::Median,
    ]
}

fn stream_config(fc: Fourcc, width: u32, height: u32, num_slices: usize) -> StreamConfig {
    let extradata = Extradata::ffmpeg_for(fc, num_slices).unwrap();
    StreamConfig::new(fc, width, height, extradata).unwrap()
}

/// Build a frame with deterministic xorshift-random content. `entropy`
/// shifts how compressible the content is: `mask` is AND-ed onto each
/// byte so a low mask yields a small alphabet (skewed histogram → deep
/// Huffman trees with many zero-length symbols), a high mask yields
/// near-uniform noise.
fn random_frame(
    fc: Fourcc,
    width: u32,
    height: u32,
    num_slices: usize,
    pred: Predictor,
    seed: u64,
    mask: u8,
) -> EncodedFrame {
    let mut planes = Vec::with_capacity(fc.plane_count());
    for i in 0..fc.plane_count() {
        let (pw, ph) = fc.plane_dim(i, width, height);
        let n = (pw as usize) * (ph as usize);
        let mut rng = Xorshift64::new(seed.wrapping_add((i as u64 + 1) * 0x9E37_79B9));
        let samples: Vec<u8> = (0..n).map(|_| rng.next_byte() & mask).collect();
        planes.push(PlaneInput { samples });
    }
    EncodedFrame {
        fourcc: fc,
        width,
        height,
        predictor: pred,
        num_slices,
        planes,
    }
}

/// Re-wrap a decoded frame back into encoder input. The decoder emits
/// planes in on-wire order with RGB inverse-decorrelation already
/// applied, which is exactly the layout the encoder expects (it
/// re-applies forward decorrelation internally) — so the planes map
/// straight across by index.
fn frame_from_decoded(decoded: &DecodedFrame, num_slices: usize) -> EncodedFrame {
    EncodedFrame {
        fourcc: decoded.fourcc,
        width: decoded.width,
        height: decoded.height,
        predictor: decoded.predictor,
        num_slices,
        planes: decoded
            .planes
            .iter()
            .map(|p| PlaneInput {
                samples: p.samples.clone(),
            })
            .collect(),
    }
}

/// Assert the decoded planes equal the source frame's planes byte-exact.
fn assert_pixel_roundtrip(src: &EncodedFrame, decoded: &DecodedFrame, ctx: &str) {
    assert_eq!(decoded.fourcc, src.fourcc, "{ctx}: fourcc");
    assert_eq!(decoded.predictor, src.predictor, "{ctx}: predictor");
    assert_eq!(decoded.planes.len(), src.planes.len(), "{ctx}: plane count");
    for (i, dp) in decoded.planes.iter().enumerate() {
        assert_eq!(
            dp.samples, src.planes[i].samples,
            "{ctx}: plane {i} pixel round-trip mismatch"
        );
    }
}

// ----------------------------------------------------------------------
// Invariant 1 — deterministic encode across entry points
// ----------------------------------------------------------------------

#[test]
fn encode_is_deterministic_and_path_invariant() {
    // For every FOURCC × predictor, on a frame large enough that the
    // auto-dispatch path would *choose* parallel (> 64 Ki luma px and
    // > 1 slice), the three entry points must all emit byte-identical
    // payloads, and a repeat call must reproduce them exactly.
    //
    // 320×216 luma = 69 120 px > PARALLEL_PIXEL_THRESHOLD (65 536), so
    // `encode_frame` takes the parallel branch; `encode_frame_serial`
    // forces the serial branch. Byte-equality across them is the
    // round-5 parallel-correctness guarantee, re-pinned here as a byte
    // invariant rather than only a pixel one.
    let (w, h) = (320u32, 216u32);
    let slices = 8usize;
    let mut cells = 0usize;
    for fc in all_fourccs() {
        for pred in all_predictors() {
            // Mid-entropy content (mask 0x3f) so the Huffman build has
            // a non-trivial alphabet and the tie-break actually matters.
            let frame = random_frame(fc, w, h, slices, pred, 0xC0FFEE ^ fc as u64, 0x3f);

            let auto1 = encode_frame(&frame).expect("encode_frame auto");
            let auto2 = encode_frame(&frame).expect("encode_frame auto repeat");
            let ser = encode_frame_serial(&frame).expect("encode_frame_serial");
            let par = encode_frame_parallel(&frame).expect("encode_frame_parallel");

            assert_eq!(
                auto1, auto2,
                "{fc:?}/{pred:?}: encode_frame not deterministic across two calls"
            );
            assert_eq!(auto1, ser, "{fc:?}/{pred:?}: auto-dispatch != serial bytes");
            assert_eq!(
                auto1, par,
                "{fc:?}/{pred:?}: auto-dispatch != parallel bytes"
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 20);
}

// ----------------------------------------------------------------------
// Invariant 2 — encode ∘ decode ∘ encode fixed point (byte-stable)
// ----------------------------------------------------------------------

#[test]
fn encode_decode_reencode_is_byte_stable_fixed_point() {
    // The headline invariant: re-compressing an already-decoded frame
    // must reproduce the first encode's bytes exactly. Sweep all five
    // FOURCCs × four predictors × three entropy regimes × two slice
    // counts at a non-trivial size.
    let (w, h) = (96u32, 70u32); // 70 % 4 != 0, 70 % 8 != 0 — uneven splits.
    let masks = [0x07u8, 0x3f, 0xff]; // skewed → mid → near-uniform.
    let slice_counts = [1usize, 4];
    let mut cells = 0usize;

    for fc in all_fourccs() {
        for pred in all_predictors() {
            for &mask in &masks {
                for &slices in &slice_counts {
                    let frame =
                        random_frame(fc, w, h, slices, pred, 0xDEAD_BEEF ^ (mask as u64), mask);
                    let cfg = stream_config(fc, w, h, slices);

                    let bytes1 = encode_frame(&frame).expect("first encode");
                    let decoded = decode_frame(&cfg, &bytes1).expect("decode");
                    assert_pixel_roundtrip(
                        &frame,
                        &decoded,
                        &format!("{fc:?}/{pred:?}/mask={mask:#x}/s={slices}"),
                    );

                    let frame2 = frame_from_decoded(&decoded, slices);
                    let bytes2 = encode_frame(&frame2).expect("re-encode");

                    assert_eq!(
                        bytes1,
                        bytes2,
                        "{fc:?}/{pred:?}/mask={mask:#x}/s={slices}: \
                         encode∘decode∘encode is not byte-stable \
                         (len {} vs {})",
                        bytes1.len(),
                        bytes2.len()
                    );

                    // And the fixed point must itself still decode to
                    // the same pixels (a second full round-trip).
                    let decoded2 = decode_frame(&cfg, &bytes2).expect("decode 2");
                    assert_pixel_roundtrip(
                        &frame,
                        &decoded2,
                        &format!("{fc:?}/{pred:?}/mask={mask:#x}/s={slices} (2nd rt)"),
                    );

                    cells += 1;
                }
            }
        }
    }
    // 5 × 4 × 3 × 2 = 120 cells.
    assert_eq!(cells, 120);
}

// ----------------------------------------------------------------------
// Invariant 3 — slice-count boundary sweep at non-divisible heights
// ----------------------------------------------------------------------

/// Sweep `num_slices` across the *full* documented `1..=256` range for
/// one FOURCC at a height chosen so most slice counts do NOT evenly
/// divide it, and the tail (`N > ph`) forces zero-row slices. Each cell
/// round-trips and (for `N <= 64`, where re-encode is cheap enough)
/// also checks the byte-stable fixed point.
fn slice_sweep_for(fc: Fourcc, w: u32, h: u32, pred: Predictor) {
    for slices in 1usize..=256 {
        let frame = random_frame(fc, w, h, slices, pred, 0xA5A5_1234 ^ slices as u64, 0x1f);
        let cfg = stream_config(fc, w, h, slices);

        let bytes1 = encode_frame(&frame).expect("encode in sweep");
        let decoded = decode_frame(&cfg, &bytes1).expect("decode in sweep");
        assert_pixel_roundtrip(
            &frame,
            &decoded,
            &format!("{fc:?}/{pred:?}/s={slices} sweep round-trip"),
        );

        // The slice-end-offset table must list exactly `num_slices`
        // entries per plane (spec/02 §5). Byte-stability re-encode for
        // the lower half keeps this test's cost bounded while still
        // covering the uneven-split (N ∤ h) and zero-row (N > h) cases.
        if slices <= 64 {
            let frame2 = frame_from_decoded(&decoded, slices);
            let bytes2 = encode_frame(&frame2).expect("re-encode in sweep");
            assert_eq!(
                bytes1, bytes2,
                "{fc:?}/{pred:?}/s={slices}: sweep not byte-stable"
            );
        }
    }
}

#[test]
fn slice_count_sweep_1_to_256_uly0_non_divisible_height() {
    // ULY0 needs even W and H. h = 70 → for N ∈ {3,4,8,16,...} the
    // luma split is uneven; chroma height = 35 → N > 35 forces zero-row
    // chroma slices well before N hits 256. W = 64, H = 70.
    slice_sweep_for(Fourcc::Uly0, 64, 70, Predictor::Median);
}

#[test]
fn slice_count_sweep_1_to_256_uly2_zero_row_tail() {
    // ULY2 needs only even W. h = 50 → for N > 50 every luma slice past
    // the 50th carries zero rows (empty residual stream, zero
    // slice-data bytes per spec/02 §5.1). Stresses the zero-row path
    // hard across N ∈ 51..=256. W = 62, H = 50.
    slice_sweep_for(Fourcc::Uly2, 62, 50, Predictor::Gradient);
}

#[test]
fn slice_count_sweep_1_to_256_uly4_all_predictors_small() {
    // ULY4 4:4:4, odd-ish height 45 (any dims allowed). Run all four
    // predictors but at a small width to keep the 256-cell sweep fast.
    for pred in all_predictors() {
        slice_sweep_for(Fourcc::Uly4, 24, 45, pred);
    }
}

#[test]
fn slice_count_sweep_1_to_256_rgb_family() {
    // ULRG + ULRA exercise the RGB inverse-decorrelation path under the
    // full slice sweep. Height 39, width 30 (any dims allowed).
    slice_sweep_for(Fourcc::Ulrg, 30, 39, Predictor::Left);
    slice_sweep_for(Fourcc::Ulra, 30, 39, Predictor::None);
}

// ----------------------------------------------------------------------
// Edge: N == ph (every slice exactly one row) and N == ph+1 (one
// zero-row slice at the tail). These are the exact transition points
// of the `ph*(s+1)/N` integer-division split.
// ----------------------------------------------------------------------

#[test]
fn slice_count_at_and_past_one_row_per_slice() {
    let fc = Fourcc::Uly4;
    let (w, h) = (20u32, 30u32);
    for &slices in &[h as usize - 1, h as usize, h as usize + 1, h as usize + 7] {
        for pred in all_predictors() {
            let frame = random_frame(fc, w, h, slices, pred, 0x1357 ^ slices as u64, 0xff);
            let cfg = stream_config(fc, w, h, slices);
            let bytes = encode_frame(&frame).expect("encode edge");
            let decoded = decode_frame(&cfg, &bytes).expect("decode edge");
            assert_pixel_roundtrip(
                &frame,
                &decoded,
                &format!("{fc:?}/{pred:?}/s={slices} one-row edge"),
            );
            // Byte-stable fixed point at the transition too.
            let frame2 = frame_from_decoded(&decoded, slices);
            let bytes2 = encode_frame(&frame2).expect("re-encode edge");
            assert_eq!(
                bytes, bytes2,
                "{fc:?}/{pred:?}/s={slices}: one-row edge not byte-stable"
            );
        }
    }
}
