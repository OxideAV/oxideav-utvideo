#![no_main]

//! Fuzz the decode-free frame-layout inspector
//! (`peek_frame` + `peek_frame_info`).
//!
//! Round 21 added a public inspector module that walks the same
//! per-frame byte layout the full decoder walks (`spec/02` §§1, 2, 4,
//! 5 + the trailing `frame_info` dword at `spec/02` §6), but does so
//! without building a `HuffmanTable` or allocating a residual buffer.
//! That walk has its own attack surface: it parses an attacker-
//! controlled chunk payload against a [`StreamConfig`] whose
//! width/height/slice-count drive the per-plane bounds, runs
//! monotonicity + word-alignment + total-length-identity checks
//! against attacker bytes, and is **publicly exposed** as a separate
//! entrypoint from [`decode_frame`]. This target asserts the inspector
//! shares the decoder's panic-free contract on every input.
//!
//! Five properties are checked on every fuzz iteration:
//!
//! 1. **Panic-free inspector**: `peek_frame_info` and `peek_frame`
//!    must always return a `Result` (no panic / abort / OOM) on any
//!    bytes — regardless of how malformed the chunk payload or how
//!    degenerate the synthesised [`StreamConfig`] is.
//!
//! 2. **Containment**: when `peek_frame` succeeds, every reported
//!    byte offset (`descriptor_start`, `end_offsets_start`,
//!    `slice_data_start`, per-slice `start`/`end`) must land inside
//!    `[0, chunk_payload.len())`. A caller indexing the returned
//!    ranges into the original buffer would never read out of bounds.
//!
//! 3. **Inspector/decoder agreement**: when `decode_frame` succeeds
//!    on the same `(cfg, payload)`, `peek_frame` must also succeed,
//!    and the predictor + trailing `frame_info` dword must match
//!    between the two parsers. This cross-validates that the two
//!    byte walks agree on the wire format. The reverse implication
//!    (peek succeeds ⇒ decode succeeds) is not asserted: the
//!    inspector skips Huffman validation, so a corrupt Huffman
//!    descriptor / slice bit-stream can fail decode while peek
//!    legitimately succeeds.
//!
//! 4. **Typed-accessor invariants**: the inspector grew a family of
//!    typed, decode-free accessors over rounds 244 / 250 / 255 / 261 /
//!    275 / 291 (`active_symbol_count`, `max_code_length`,
//!    `min_code_length`, `min_code_length_symbol_count`,
//!    `code_length_histogram`, `kraft_numerator`, `unused_symbol_count`,
//!    `is_kraft_complete`, `total_pixels`, `total_size`,
//!    `slice_data_total`) — all added *after* the round-228 fuzz target,
//!    so none of their documented cross-accessor invariants were pinned
//!    on attacker-shaped bytes. This property asserts every invariant the
//!    accessor doc-comments promise (all `spec/05` §§2.1, 2.2, 6.1 +
//!    `spec/02` §5) on each plane of every successful `peek_frame`:
//!    the descriptor-byte conservation law `active + unused + single ==
//!    256`; the strictly-ascending, no-zero-tier code-length histogram
//!    whose scalar projections recover the four round-244..261 counters;
//!    the single-symbol path forcing all length counters to 0 and an
//!    empty histogram; the Kraft-numerator / `is_kraft_complete`
//!    consistency (`kraft_numerator == 2^max` iff complete); and the
//!    per-plane geometry identities `total_pixels == width*height`,
//!    `slice_data_total` word-aligned, `total_size == 256 +
//!    4*num_slices + slice_data_total`. The frame roll-up identities
//!    (`total_size == Σ plane_total + 4`, `all_planes_kraft_complete ==
//!    ∀ plane is_kraft_complete`) are pinned too.
//!
//! 5. **Decode ⇒ Kraft-complete**: when `decode_frame` succeeds, every
//!    plane's descriptor MUST be Kraft-complete — `HuffmanTable::build`
//!    rejects any incomplete / over-subscribed descriptor
//!    (`Error::KraftViolation`, `spec/05` §2.2 step 3) and the
//!    single-symbol path is recognised as complete by definition
//!    (`spec/05` §6.1). So a successful decode implies
//!    `peek_frame(...).all_planes_kraft_complete()` — the decode-free
//!    predicate and the real decoder must never disagree on which
//!    frames are codebook-decodable.
//!
//! The header layout matches `decode_utvideo.rs` so a corpus entry
//! good enough for one target is a useful starting point for the
//! other:
//!
//! ```text
//!   byte 0      : FourCC selector (mod 5)
//!   byte 1      : width  seed  (mapped into 2..=64, snapped even)
//!   byte 2      : height seed  (mapped into 2..=64, snapped even)
//!   byte 3      : slice-count seed (1..=16)
//!   bytes 4..   : chunk payload fed verbatim to peek_frame + decode_frame
//! ```
//!
//! Dimensions are capped at 64×64 for the same reason the decoder
//! target caps them: the inspector legitimately allocates a
//! `Vec<PlaneLayout>` whose per-plane `Vec<SliceLayout>` is sized
//! by the declared geometry, and fuzzing oversized declarations
//! would burn iterations on documented capability rather than on
//! parser defects.

use libfuzzer_sys::fuzz_target;
use oxideav_utvideo::{decode_frame, peek_frame, peek_frame_info, Extradata, Fourcc, StreamConfig};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let (header, payload) = data.split_at(4);

    let fourcc = match header[0] % 5 {
        0 => Fourcc::Uly0,
        1 => Fourcc::Uly2,
        2 => Fourcc::Uly4,
        3 => Fourcc::Ulrg,
        _ => Fourcc::Ulra,
    };

    // Snap each dim to even, 2..=64, satisfying chroma-subsampling
    // parity constraints (ULY0 even W+H, ULY2 even W).
    let width = ((header[1] as u32 % 64) + 2) & !1;
    let height = ((header[2] as u32 % 64) + 2) & !1;

    let num_slices = (header[3] as u32 % 16) + 1;
    let flags = 0x0000_0001 | ((num_slices - 1) << 24);
    let extradata = Extradata {
        encoder_version: 0x0100_00f0,
        source_format_tag: *fourcc.as_bytes(),
        frame_info_size: 4,
        flags,
    };

    let cfg = match StreamConfig::new(fourcc, width, height, extradata) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Property 1: panic-free `peek_frame_info`.
    let peek_info = peek_frame_info(payload);

    // Property 1: panic-free `peek_frame`.
    let peek = peek_frame(&cfg, payload);

    // Property 2: containment on every reported offset.
    if let Ok(layout) = &peek {
        let n = payload.len();
        for plane in &layout.planes {
            assert!(plane.descriptor_start <= n);
            assert!(plane.end_offsets_start <= n);
            assert!(plane.slice_data_start <= n);
            assert!(plane.descriptor_start <= plane.end_offsets_start);
            assert!(plane.end_offsets_start <= plane.slice_data_start);
            for slice in &plane.slices {
                assert!(slice.start <= n);
                assert!(slice.end <= n);
                assert!(slice.start <= slice.end);
                assert!(slice.start >= plane.slice_data_start);
            }
        }
        // `peek_frame_info` succeeds iff `payload.len() >= 4`, and
        // `peek_frame` itself enforces the same lower bound — so when
        // `peek_frame` returns `Ok`, the trailing-dword peek must also
        // succeed and report the same `frame_info`.
        let (peek_info_dword, peek_info_pred) =
            peek_info.expect("peek_frame_info must succeed when peek_frame succeeds");
        assert_eq!(peek_info_dword, layout.frame_info);
        assert_eq!(peek_info_pred, layout.predictor);

        // Property 4: typed-accessor invariants on every plane.
        for plane in &layout.planes {
            let p = plane.plane_idx;

            // --- descriptor-byte conservation (`spec/05` §2.1) ---
            // Every one of the 256 descriptor bytes is exactly one of:
            // an active code length (`1..=254`), the `0` single-symbol
            // sentinel (`spec/05` §6.1, at most one such byte), or a
            // `255` unused sentinel. `unused_symbol_count` doc:
            // `active + unused + (single ? 1 : 0) == 256`.
            let single = u32::from(plane.is_single_symbol);
            assert_eq!(
                plane.active_symbol_count + plane.unused_symbol_count() + single,
                256,
                "plane {p}: active+unused+single != 256"
            );
            assert!(
                plane.active_symbol_count <= 256,
                "plane {p}: active_symbol_count {} > 256",
                plane.active_symbol_count
            );

            // --- code-length range (`spec/05` §2.1: active is 1..=254) ---
            assert!(
                plane.max_code_length <= 254,
                "plane {p}: max_code_length {} > 254 wire bound",
                plane.max_code_length
            );
            assert!(
                plane.min_code_length <= plane.max_code_length,
                "plane {p}: min_code_length {} > max_code_length {}",
                plane.min_code_length,
                plane.max_code_length
            );

            // --- single-symbol path forces all length counters to 0 ---
            // (`spec/05` §6.1: the lone `0` sentinel is NOT an active
            // code, so the active scan finds nothing.)
            if plane.is_single_symbol {
                assert_eq!(
                    plane.active_symbol_count, 0,
                    "plane {p}: single but active != 0"
                );
                assert_eq!(
                    plane.max_code_length, 0,
                    "plane {p}: single but max_len != 0"
                );
                assert_eq!(
                    plane.min_code_length, 0,
                    "plane {p}: single but min_len != 0"
                );
                assert_eq!(
                    plane.min_code_length_symbol_count, 0,
                    "plane {p}: single but min_len_count != 0"
                );
                assert!(
                    plane.code_length_histogram.is_empty(),
                    "plane {p}: single but histogram non-empty"
                );
            }

            // --- histogram <-> scalar-accessor projection (`code_length_histogram` doc) ---
            let hist = &plane.code_length_histogram;
            // Empty list exactly when no active symbol.
            assert_eq!(
                hist.is_empty(),
                plane.active_symbol_count == 0,
                "plane {p}: histogram-empty / active-zero disagreement"
            );
            // Strictly ascending by length, no zero-count tiers, each L in 1..=254.
            let mut prev_len: Option<u8> = None;
            let mut hist_total: u32 = 0;
            for &(len, count) in hist {
                assert!(
                    (1..=254).contains(&len),
                    "plane {p}: histogram tier length {len} out of active range 1..=254"
                );
                assert!(
                    count >= 1,
                    "plane {p}: histogram tier ({len}) has zero count"
                );
                if let Some(prev) = prev_len {
                    assert!(
                        len > prev,
                        "plane {p}: histogram not strictly ascending ({prev} then {len})"
                    );
                }
                prev_len = Some(len);
                hist_total = hist_total
                    .checked_add(count)
                    .expect("plane histogram count sum overflowed u32");
            }
            assert_eq!(
                hist_total, plane.active_symbol_count,
                "plane {p}: Σ histogram count != active_symbol_count"
            );
            if let (Some(first), Some(last)) = (hist.first(), hist.last()) {
                assert_eq!(
                    first.0, plane.min_code_length,
                    "plane {p}: first tier len != min"
                );
                assert_eq!(
                    first.1, plane.min_code_length_symbol_count,
                    "plane {p}: first tier count != min_code_length_symbol_count"
                );
                assert_eq!(
                    last.0, plane.max_code_length,
                    "plane {p}: last tier len != max"
                );
            }

            // --- min-tier multiplicity cross-checks (`min_code_length_symbol_count` doc) ---
            assert!(
                plane.min_code_length_symbol_count <= plane.active_symbol_count,
                "plane {p}: min_len_count {} > active {}",
                plane.min_code_length_symbol_count,
                plane.active_symbol_count
            );
            // Single-length descriptor: every active symbol shares the one tier.
            if plane.active_symbol_count > 0 && plane.min_code_length == plane.max_code_length {
                assert_eq!(
                    plane.min_code_length_symbol_count, plane.active_symbol_count,
                    "plane {p}: single-length descriptor but min_len_count != active"
                );
            }

            // --- Kraft numerator / completeness consistency (`spec/05` §2.2 step 3) ---
            let kn = plane.kraft_numerator();
            // kraft_numerator == 0 iff histogram empty.
            assert_eq!(
                kn == 0,
                hist.is_empty(),
                "plane {p}: kraft_numerator-zero / histogram-empty disagreement"
            );
            // is_kraft_complete agrees with the documented arithmetic.
            let expected_complete = if plane.is_single_symbol {
                true
            } else if hist.is_empty() {
                false
            } else {
                kn == 1u128 << plane.max_code_length
            };
            assert_eq!(
                plane.is_kraft_complete(),
                expected_complete,
                "plane {p}: is_kraft_complete disagrees with kraft_numerator arithmetic"
            );

            // --- per-plane geometry identities (`spec/02` §5) ---
            assert_eq!(
                plane.total_pixels(),
                u64::from(plane.width) * u64::from(plane.height),
                "plane {p}: total_pixels != width*height"
            );
            // slice_data_total is word-aligned for a well-formed plane
            // (`spec/05` §4.1); peek_frame enforces the wire alignment.
            assert_eq!(
                plane.slice_data_total() % 4,
                0,
                "plane {p}: slice_data_total not 4-byte aligned"
            );
            assert_eq!(
                plane.total_size(),
                256 + 4 * plane.slices.len() + plane.slice_data_total(),
                "plane {p}: total_size identity broken"
            );
        }

        // --- frame roll-up identities (`FrameLayout` docs) ---
        let plane_total_sum: usize = layout.planes.iter().map(|p| p.total_size()).sum();
        assert_eq!(
            layout.total_size(),
            plane_total_sum + 4,
            "frame total_size != Σ plane_total_size + 4"
        );
        let plane_slice_sum: usize = layout.planes.iter().map(|p| p.slice_data_total()).sum();
        assert_eq!(
            layout.total_slice_data_bytes(),
            plane_slice_sum,
            "frame total_slice_data_bytes != Σ plane slice_data_total"
        );
        assert_eq!(
            layout.all_planes_kraft_complete(),
            layout.planes.iter().all(|p| p.is_kraft_complete()),
            "all_planes_kraft_complete != ∀ plane is_kraft_complete"
        );
    }

    // Property 3: inspector/decoder agreement on success.
    if let Ok(decoded) = decode_frame(&cfg, payload) {
        let layout =
            peek.expect("decode_frame succeeded; peek_frame must succeed on the same bytes");
        assert_eq!(layout.frame_info, decoded.frame_info);
        assert_eq!(layout.predictor, decoded.predictor);

        // Property 5: a successful decode implies every plane's
        // descriptor is Kraft-complete. `HuffmanTable::build` rejects any
        // incomplete / over-subscribed descriptor (`Error::KraftViolation`,
        // `spec/05` §2.2 step 3); the single-symbol path is complete by
        // definition (`spec/05` §6.1). So the decode-free predicate must
        // never claim a successfully-decoded frame is NOT decodable.
        assert!(
            layout.all_planes_kraft_complete(),
            "decode_frame succeeded but all_planes_kraft_complete() is false"
        );
    }
});
