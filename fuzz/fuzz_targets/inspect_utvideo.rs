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
//! Three properties are checked on every fuzz iteration:
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
    }

    // Property 3: inspector/decoder agreement on success.
    if let Ok(decoded) = decode_frame(&cfg, payload) {
        let layout =
            peek.expect("decode_frame succeeded; peek_frame must succeed on the same bytes");
        assert_eq!(layout.frame_info, decoded.frame_info);
        assert_eq!(layout.predictor, decoded.predictor);
    }
});
