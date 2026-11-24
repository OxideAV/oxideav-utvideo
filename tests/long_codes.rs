//! Code lengths beyond 32 bits.
//!
//! `spec/05` §7.2: the descriptor byte carries any code length in
//! `1..=254`, and "a decoder MUST therefore support code lengths up to
//! 254 to be wire-format-compliant". This crate used to refuse any
//! descriptor with a length above 32 as a Kraft violation.
//!
//! The frames here are hand-built straight from the spec text, with no
//! help from the crate's own code builder: the `spec/05` §2.2 code
//! assignment is carried out on arbitrary-width bit strings, and the
//! slice data is packed per §4 (32-bit little-endian words, MSB first,
//! zero padding). The most skewed complete code on `k + 1` symbols has
//! lengths `1, 2, …, k - 1, k, k`, which reaches 254 bits with 255
//! symbols.
//!
//! The last test hands the same frames to the black-box decoder (skipped
//! when the `ffmpeg` executable is absent) and reports the longest code
//! it reproduces; where it answers at all it must agree with this crate.

mod support;

use oxideav_utvideo::{
    Error, Extradata, Fourcc, StreamConfig, decode_frame, decode_frame_strict, peek_frame,
};

// ───────────────────── spec/05 §2.2 on wide bit strings ─────────────────────

/// A code value as a big-endian bit string of the current length.
#[derive(Clone)]
struct WideCode(Vec<bool>);

impl WideCode {
    fn zero(len: usize) -> Self {
        WideCode(vec![false; len])
    }

    /// `code = code + 1` (the width is never exceeded for a complete
    /// code until the very last assignment, whose carry-out is unused).
    fn increment(&mut self) {
        for bit in self.0.iter_mut().rev() {
            *bit = !*bit;
            if *bit {
                return;
            }
        }
    }

    /// `code = code >> n` together with the length step.
    fn shift_right(&mut self, n: usize) {
        let keep = self.0.len() - n;
        self.0.truncate(keep);
    }
}

/// `spec/05` §2.2, literally: sort by (length DESC, symbol DESC), start
/// at zero, `code = (code + 1) >> (prev_len - cur_len)` at a length
/// step, assign, `code += 1`.
fn spec_codes(code_length: &[u8; 256]) -> Vec<Option<Vec<bool>>> {
    let mut order: Vec<(u8, u8)> = (0..=255u8)
        .filter(|&s| !matches!(code_length[s as usize], 0 | 255))
        .map(|s| (code_length[s as usize], s))
        .collect();
    order.sort_by(|a, b| b.cmp(a));
    let mut codes = vec![None; 256];
    let Some(&(first_len, _)) = order.first() else {
        return codes;
    };
    let mut code = WideCode::zero(first_len as usize);
    let mut prev_len = first_len;
    let mut pending_increment = false;
    for (len, sym) in order {
        if len < prev_len {
            // `(code + 1) >> shift`: the `+ 1` is the post-increment of
            // the previous iteration plus this one.
            code.increment();
            code.increment();
            code.shift_right((prev_len - len) as usize);
            prev_len = len;
        } else if pending_increment {
            code.increment();
        }
        codes[sym as usize] = Some(code.0.clone());
        pending_increment = true;
    }
    codes
}

/// Pack a bit string per `spec/05` §4: 32-bit LE words, MSB first,
/// zero-padded to a word boundary.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len().div_ceil(32) * 4);
    for word_bits in bits.chunks(32) {
        let mut word = 0u32;
        for (i, &b) in word_bits.iter().enumerate() {
            if b {
                word |= 1 << (31 - i);
            }
        }
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

// ───────────────────────── frame construction ─────────────────────────

/// Descriptor with lengths `1, 2, …, k - 1, k, k` on symbols
/// `stride * i mod 256` (so symbol order and length order disagree).
fn chain_descriptor(k: u8, stride: u8) -> ([u8; 256], Vec<u8>) {
    let mut d = [255u8; 256];
    let mut symbols = Vec::new();
    for i in 0..=k {
        let sym = stride.wrapping_mul(i).wrapping_add(11);
        assert_eq!(d[sym as usize], 255, "stride must be odd");
        d[sym as usize] = if i == k { k } else { i + 1 };
        symbols.push(sym);
    }
    (d, symbols)
}

struct Built {
    cfg: StreamConfig,
    payload: Vec<u8>,
    /// Expected decoded planes (prediction mode 0: sample = residual).
    planes: Vec<Vec<u8>>,
}

/// A `ULY4` frame, predictor "none", whose three planes all use the
/// chain code of depth `k`. Every symbol appears; the deep ones are
/// spread through the picture and the slices.
fn build_frame(k: u8, w: u32, h: u32, slices: usize) -> Built {
    let fourcc = Fourcc::Uly4;
    let mut payload = Vec::new();
    let mut planes = Vec::new();
    for plane in 0..3u8 {
        let (d, symbols) = chain_descriptor(k, [1u8, 37, 201][plane as usize]);
        let codes = spec_codes(&d);
        let n = (w * h) as usize;
        let mut state = 0x9e37_79b9u32 ^ (u32::from(plane) << 8) ^ u32::from(k);
        let samples: Vec<u8> = (0..n)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                // Mostly shallow symbols; every 5th pixel walks the
                // whole alphabet so each depth is exercised.
                let idx = if i % 5 == 0 {
                    (i / 5) % symbols.len()
                } else {
                    (state.trailing_zeros() as usize).min(symbols.len() - 1)
                };
                symbols[idx]
            })
            .collect();
        payload.extend_from_slice(&d);
        let mut ends = Vec::new();
        let mut data = Vec::new();
        for s in 0..slices {
            let (r0, r1) = fourcc.slice_rows(plane as usize, h, slices, s);
            let mut bits = Vec::new();
            for &sample in &samples[r0 * w as usize..r1 * w as usize] {
                bits.extend_from_slice(codes[sample as usize].as_ref().unwrap());
            }
            data.extend_from_slice(&pack_bits(&bits));
            ends.push(data.len() as u32);
        }
        for e in ends {
            payload.extend_from_slice(&e.to_le_bytes());
        }
        payload.extend_from_slice(&data);
        planes.push(samples);
    }
    payload.extend_from_slice(&0u32.to_le_bytes()); // frame info: predictor 0
    let extradata = Extradata::canonical_extradata_for(fourcc, slices).unwrap();
    Built {
        cfg: StreamConfig::new(fourcc, w, h, extradata).unwrap(),
        payload,
        planes,
    }
}

const DEPTHS: [u8; 12] = [2, 12, 13, 16, 31, 32, 33, 34, 64, 127, 128, 254];

// ───────────────────────── tests ─────────────────────────

#[test]
fn spec_code_builder_matches_the_documented_examples() {
    // `spec/05` §3.1.2: {3: 1, 128: 2, 218: 2} → 218 "00", 128 "01", 3 "1".
    let mut d = [255u8; 256];
    d[3] = 1;
    d[128] = 2;
    d[218] = 2;
    let codes = spec_codes(&d);
    assert_eq!(codes[218].as_deref(), Some(&[false, false][..]));
    assert_eq!(codes[128].as_deref(), Some(&[false, true][..]));
    assert_eq!(codes[3].as_deref(), Some(&[true][..]));
    // §6.3: uniform 8-bit code maps symbol s to !s.
    let codes = spec_codes(&[8u8; 256]);
    for s in 0..=255u8 {
        let want: Vec<bool> = (0..8).rev().map(|b| (!s >> b) & 1 == 1).collect();
        assert_eq!(codes[s as usize].as_deref(), Some(&want[..]));
    }
    // §2.4: the shortest code of a chain is all ones, the longest all
    // zeros.
    let (d, symbols) = chain_descriptor(200, 3);
    let codes = spec_codes(&d);
    assert_eq!(codes[symbols[0] as usize].as_deref(), Some(&[true][..]));
    let deepest = symbols[199].max(symbols[200]);
    assert!(codes[deepest as usize].as_ref().unwrap().iter().all(|b| !b));
}

#[test]
fn every_code_length_up_to_254_decodes() {
    // The interpreter run keeps the two depths either side of the
    // 32-bit boundary plus the maximum.
    let depths: &[u8] = if cfg!(miri) { &[32, 33, 254] } else { &DEPTHS };
    for &k in depths {
        for (w, h, slices) in [(23u32, 31u32, 1usize), (40, 29, 4), (1, 300, 7)] {
            let f = build_frame(k, w, h, slices);
            let frame = decode_frame(&f.cfg, &f.payload)
                .unwrap_or_else(|e| panic!("depth {k} {w}x{h} s{slices}: {e}"));
            for (i, plane) in frame.planes.iter().enumerate() {
                assert_eq!(plane.samples, f.planes[i], "depth {k} plane {i}");
            }
            // Padding is zero, so the strict surface agrees too.
            assert_eq!(decode_frame_strict(&f.cfg, &f.payload).unwrap(), frame);
            let layout = peek_frame(&f.cfg, &f.payload).unwrap();
            assert!(layout.planes.iter().all(|p| p.max_code_length == k));
        }
    }
}

#[test]
fn all_chain_depths_decode() {
    // Every depth, small picture: no gap between the 32-bit path and
    // the bit-serial one.
    for k in (1..=254u8).step_by(if cfg!(miri) { 23 } else { 1 }) {
        let f = build_frame(k, 64, 9, 2);
        let frame = decode_frame(&f.cfg, &f.payload).unwrap_or_else(|e| panic!("depth {k}: {e}"));
        for (i, plane) in frame.planes.iter().enumerate() {
            assert_eq!(plane.samples, f.planes[i], "depth {k} plane {i}");
        }
    }
}

#[test]
fn damaged_long_code_frames_fail_cleanly() {
    let f = build_frame(254, 40, 29, 4);
    // Truncate inside the last plane's slice data: the frame no longer
    // parses, and nothing panics for any cut point.
    for cut in (0..f.payload.len()).step_by(if cfg!(miri) { 997 } else { 97 }) {
        assert!(decode_frame(&f.cfg, &f.payload[..cut]).is_err());
    }
    // Flip bits inside slice data: decode either fails or yields
    // different samples, never panics. A slice that runs dry reports
    // truncation.
    let mut dry = 0;
    for at in (256 + 16..f.payload.len() - 4).step_by(if cfg!(miri) { 211 } else { 53 }) {
        let mut bad = f.payload.clone();
        bad[at] ^= 0x5a;
        if let Err(Error::SliceTruncated { .. }) = decode_frame(&f.cfg, &bad) {
            dry += 1;
        }
    }
    assert!(dry > 0, "expected at least one slice to run out of bits");
    // A length histogram that is not complete stays rejected at any
    // depth.
    let (mut d, symbols) = chain_descriptor(254, 3);
    d[symbols[254] as usize] = 255; // drop one of the two deepest codes
    let mut bad = f.payload.clone();
    bad[..256].copy_from_slice(&d);
    assert!(matches!(
        decode_frame(&f.cfg, &bad),
        Err(Error::KraftViolation)
    ));
}

#[test]
fn black_box_decoder_agrees_on_long_codes() {
    use support::{Scratch, p, run_tool, s, tool_available, write_avi};
    if cfg!(miri) || !tool_available() {
        eprintln!("skipping: black-box ffmpeg with Ut Video support not found");
        return;
    }
    let scratch = Scratch::new("long-codes");
    let mut accepted = Vec::new();
    let mut refused = Vec::new();
    for k in DEPTHS {
        let f = build_frame(k, 40, 29, 4);
        let avi = scratch.dir.join(format!("k{k}.avi"));
        let ed = f.cfg.extradata.to_bytes();
        write_avi(
            &avi,
            b"ULY4",
            24,
            40,
            29,
            &ed,
            std::slice::from_ref(&f.payload),
        );
        let out = scratch.dir.join(format!("k{k}.raw"));
        let run = run_tool(&[
            s("-loglevel"),
            s("warning"),
            s("-i"),
            p(&avi),
            s("-f"),
            s("rawvideo"),
            p(&out),
        ]);
        let got = std::fs::read(&out).unwrap_or_default();
        let want: Vec<u8> = f.planes.concat();
        if run.ok && run.stderr.trim().is_empty() {
            // The black box answered: it must agree with this crate.
            assert_eq!(got, want, "depth {k}: black-box decode differs");
            accepted.push(k);
        } else {
            refused.push(k);
        }
    }
    eprintln!("black-box decoder reproduces code depths {accepted:?}; declines {refused:?}");
    // Depths every decoder of this format must handle (`spec/05` §7.1:
    // 16 bits observed in the wild).
    for k in [2u8, 12, 13, 16] {
        assert!(accepted.contains(&k), "depth {k} declined by the black box");
    }
}
