#![no_main]

//! Decode arbitrary fuzz-supplied bytes through `decode_frame`. The
//! decoder must always return a `Result` and never panic / abort / OOM,
//! regardless of how malformed the chunk payload or the synthesised
//! [`StreamConfig`] is.
//!
//! The decoder's input is a parsed [`StreamConfig`] (FourCC + frame
//! dimensions + 16-byte extradata that fixes the slice count) plus the
//! attacker-controlled chunk-payload bytes. To exercise the parser
//! rather than trivially bail on a dimension mismatch, the harness
//! derives a *small* `StreamConfig` from a fixed-length header prefix of
//! the fuzz input and hands the remainder to `decode_frame`:
//!
//! ```text
//!   byte 0      : FourCC selector (mod 5)
//!   byte 1      : width  seed  (mapped into 1..=64, snapped even)
//!   byte 2      : height seed  (mapped into 1..=64, snapped even)
//!   byte 3      : low nibble → slice-count seed (1..=16);
//!                 high nibble → parallel worker budget (1..=8)
//!   bytes 4..   : chunk payload fed verbatim to decode_frame
//! ```
//!
//! Dimensions are capped at 64×64 so the fuzzer cannot ask the decoder
//! to allocate an attacker-sized plane buffer purely from the declared
//! geometry (that is a documented capability of the format, not a
//! decoder bug) — keeping the budget on genuine parser defects: index
//! math on the descriptor / offset table, slice-range arithmetic, and
//! the Huffman bit reader. Beyond panic-freedom, the harness asserts the
//! cross-surface contract: the serial default, forced-serial, and
//! forced-parallel (budget-clamped) decodes agree on success-vs-failure
//! and on the exact decoded frame, and the strict padding scanner stays
//! panic-free.

use libfuzzer_sys::fuzz_target;
use oxideav_utvideo::decoder::{decode_frame_parallel, decode_frame_serial};
use oxideav_utvideo::{decode_frame, decode_frame_strict, Extradata, Fourcc, StreamConfig};

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

    // Map each dimension seed into 2..=64 snapped even, satisfying the
    // chroma-subsampling parity constraints (ULY0 needs even W and H,
    // ULY2 needs even W) for every FourCC. `StreamConfig::new` validates
    // dims, so an unsatisfiable geometry would early-return `Err` and
    // waste the input; snapping keeps every header reaching the decoder.
    let width = ((header[1] as u32 % 64) + 2) & !1; // even, 2..=64
    let height = ((header[2] as u32 % 64) + 2) & !1; // even, 2..=64

    // Slice count 1..=16 from the low nibble. The extradata flags top
    // byte encodes `num_slices - 1`; the Huffman bit (0x1) must be set
    // or the parse rejects the stream before the decoder runs. The
    // high nibble picks the parallel path's worker budget (1..=8) so
    // the budget clamp `workers.min(num_slices).max(1)` is fuzzed
    // across under-, exact-, and over-subscribed fan-outs.
    let num_slices = (header[3] as u32 % 16) + 1;
    let workers = ((header[3] >> 4) as usize % 8) + 1;
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

    // Drive all four decode entry points. None may panic, and the three
    // lenient surfaces (serial default / forced-serial / forced-parallel
    // under the fuzzed worker budget) must agree on success-vs-failure
    // and — on success — on the exact decoded frame. Every slice's
    // predictor state restarts at the per-slice +128 seed (`spec/04`
    // §§3.1, 4, 5, 7) and every slice's Huffman bit-stream is
    // self-contained (`spec/02` §5), so the fan-out is bit-exact
    // equivalent to the serial walk.
    let d = decode_frame(&cfg, payload);
    let s = decode_frame_serial(&cfg, payload);
    let p = decode_frame_parallel(&cfg, payload, workers);
    // The strict padding scanner (`spec/05` §4.3) must also stay
    // panic-free on arbitrary bytes; its accept/reject verdict is
    // unconstrained here.
    let _ = decode_frame_strict(&cfg, payload);

    assert_eq!(d.is_ok(), s.is_ok(), "default vs serial ok-mismatch");
    assert_eq!(d.is_ok(), p.is_ok(), "default vs parallel ok-mismatch");
    if let (Ok(a), Ok(b)) = (&d, &s) {
        assert_eq!(a, b, "serial diverged from default");
    }
    if let (Ok(a), Ok(b)) = (&d, &p) {
        assert_eq!(a, b, "parallel diverged from default");
    }
});
