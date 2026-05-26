//! Stable-Rust mirror of the `encode_utvideo_frame` cargo-fuzz target.
//!
//! `cargo-fuzz` requires a nightly toolchain (libFuzzer's sanitizer-
//! coverage flags are `-Z`-gated), so the regular CI matrix never builds
//! the `fuzz/` binary crate. This stable-Rust test gives the same
//! logical coverage so a corrupted seed file, a regressed encoder, or
//! an encoder/decoder skew trips one of the existing CI lanes instead
//! of waiting for the daily fuzz run to notice. It also doubles as
//! documentation: the in-line adversarial buffers spell out exactly
//! which encoder-input shapes the harness is built to survive.
//!
//! The harness is byte-identical to the libFuzzer target up to (a)
//! `bytes` being read from disk / vector literals instead of
//! `fuzz_target!(|data: &[u8]|)`, and (b) the panic on
//! "encoder accepted but decoder rejected" being asserted via
//! `assert_eq!` / `panic!` instead of libFuzzer's abort. The fuzz
//! target sets a 32×32 max plane size to keep iteration cost low; this
//! mirror runs at the same cap so seed-file inputs and in-line inputs
//! exercise the same code paths.

use std::fs;
use std::path::PathBuf;

use oxideav_utvideo::{
    decode_frame, encode_frame, EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor,
    StreamConfig,
};

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz")
        .join("corpus")
        .join("encode_utvideo_frame")
}

/// Mirror of the fuzz_target body — see
/// `fuzz/fuzz_targets/encode_utvideo_frame.rs` for the spec on the
/// header layout, dim parity, and round-trip contract.
fn drive(data: &[u8]) {
    if data.len() < 5 {
        return;
    }
    let header = &data[..5];
    let mut payload = &data[5..];

    let fourcc = match header[0] % 5 {
        0 => Fourcc::Uly0,
        1 => Fourcc::Uly2,
        2 => Fourcc::Uly4,
        3 => Fourcc::Ulrg,
        _ => Fourcc::Ulra,
    };

    let width = ((header[1] as u32 % 32) + 2) & !1;
    let height = ((header[2] as u32 % 32) + 2) & !1;

    let predictor = match header[3] % 4 {
        0 => Predictor::None,
        1 => Predictor::Left,
        2 => Predictor::Gradient,
        _ => Predictor::Median,
    };

    let num_slices = ((header[4] as u32 % 16) + 1).min(height) as usize;

    let plane_count = fourcc.plane_count();
    let mut planes = Vec::with_capacity(plane_count);
    for i in 0..plane_count {
        let (pw, ph) = fourcc.plane_dim(i, width, height);
        let expected = (pw as usize) * (ph as usize);
        let mut samples = vec![0u8; expected];
        let take = expected.min(payload.len());
        samples[..take].copy_from_slice(&payload[..take]);
        payload = &payload[take..];
        planes.push(PlaneInput { samples });
    }

    let frame = EncodedFrame {
        fourcc,
        width,
        height,
        predictor,
        num_slices,
        planes,
    };
    let inputs: Vec<Vec<u8>> = frame.planes.iter().map(|p| p.samples.clone()).collect();

    let encoded = match encode_frame(&frame) {
        Ok(b) => b,
        Err(_) => return,
    };

    let flags = 0x0000_0001 | (((num_slices as u32) - 1) << 24);
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

    let decoded = match decode_frame(&cfg, &encoded) {
        Ok(f) => f,
        Err(e) => panic!(
            "encoder produced bytes the in-crate decoder rejects: {e:?} \
             fourcc={fourcc:?} {width}x{height} pred={predictor:?} slices={num_slices}"
        ),
    };

    assert_eq!(decoded.planes.len(), inputs.len(), "plane-count mismatch");
    for (i, (dec, inp)) in decoded.planes.iter().zip(inputs.iter()).enumerate() {
        assert_eq!(
            &dec.samples, inp,
            "plane {i} roundtrip mismatch: fourcc={fourcc:?} {width}x{height} \
             pred={predictor:?} slices={num_slices}"
        );
    }
}

#[test]
fn corpus_files_drive_encoder_without_panicking() {
    let dir = corpus_dir();
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| {
        panic!("read fuzz seed-corpus dir {}: {}", dir.display(), e);
    });
    let mut count = 0;
    for ent in entries {
        let path = ent.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).expect("read seed");
        drive(&bytes);
        count += 1;
    }
    assert!(
        count >= 1,
        "expected at least one seed in {}",
        dir.display()
    );
}

#[test]
fn empty_bytes_dont_panic() {
    drive(&[]);
}

#[test]
fn header_only_no_payload_doesnt_panic() {
    // Header present but no plane bytes follow — the harness zero-fills
    // the per-plane buffers, so the encoder still runs against a valid
    // (all-zero) input.
    drive(&[0, 6, 6, 1, 1]);
}

#[test]
fn single_byte_short_input_doesnt_panic() {
    // One byte is below the 5-byte header minimum — the target returns
    // early without touching the encoder. This pins that early-exit so
    // a future header-format change can't silently start indexing into
    // a too-short buffer.
    drive(&[0u8]);
}

#[test]
fn all_ones_doesnt_panic() {
    let bytes = vec![0xffu8; 256];
    drive(&bytes);
}

#[test]
fn deterministic_random_pattern() {
    let mut bytes = Vec::with_capacity(4096);
    let mut x: u32 = 0xdead_beef;
    for _ in 0..4096 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        bytes.push((x >> 16) as u8);
    }
    drive(&bytes);
}

#[test]
fn every_fourcc_with_predictor_left_roundtrips() {
    // One driver call per FourCC at predictor=Left, 8×8, single slice,
    // solid grey samples. Pins that the FourCC enumeration is closed
    // (any new variant would trip the `% 5` mod in the harness — fix
    // here too if Fourcc grows).
    for fcc_sel in 0u8..5 {
        let header = [fcc_sel, 6, 6, /* Left */ 1, 1];
        // Big enough payload for any FourCC's plane sum at 8×8:
        // ULRA needs 4 × 64 = 256 bytes; everything smaller fits.
        let mut bytes = header.to_vec();
        bytes.extend(std::iter::repeat(0x80u8).take(256));
        drive(&bytes);
    }
}

#[test]
fn every_predictor_with_uly2_roundtrips() {
    // One driver call per predictor at FourCC=ULY2, 8×8, 2 slices.
    // Pins that the predictor enumeration is closed.
    for pred_sel in 0u8..4 {
        let mut bytes = vec![/* ULY2 */ 1, 6, 6, pred_sel, 2];
        // ULY2 8×8 = 64 + 32 + 32 = 128 plane bytes.
        bytes.extend(std::iter::repeat(0x80u8).take(128));
        drive(&bytes);
    }
}

#[test]
fn slice_count_capped_at_height() {
    // slice_seed = 15 → 16, but height = 4 → capped at 4. The fuzz
    // target's `.min(height)` step is what prevents a "more slices than
    // rows" caller bug from short-circuiting the harness on every input.
    let mut bytes = vec![/* ULY4 */ 2, 2, 2, /* Median */ 3, 15];
    bytes.extend(std::iter::repeat(0x40u8).take(48)); // ULY4 4×4 ×3 planes.
    drive(&bytes);
}

#[test]
fn max_dim_32x32_uly4_gradient_runs() {
    // 32×32 ULY4 with 8 slices and the Gradient predictor — the
    // upper-bound input the fuzz target accepts; pins that the
    // 4 KiB-per-plane budget round-trips cleanly.
    let header = [/* ULY4 */ 2, 30, 30, /* Gradient */ 2, 8];
    let mut bytes = header.to_vec();
    // 32×32 ×3 planes = 3 072 bytes.
    bytes.extend((0..3072).map(|i| (i * 7) as u8));
    drive(&bytes);
}

#[test]
fn ulra_alpha_plane_roundtrips() {
    // ULRA: 4 planes (G, B, R, A). The alpha plane has no decorrelation
    // transform per spec/04; pins that the harness wires 4 planes
    // through without dropping the trailing A.
    let header = [/* ULRA */ 4, 2, 2, /* Left */ 1, 1];
    let mut bytes = header.to_vec();
    // 4×4 ×4 planes = 64 bytes.
    bytes.extend((0..64).map(|i| (0x40 + i) as u8));
    drive(&bytes);
}
