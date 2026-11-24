//! Serial decode + encode throughput for **every** FourCC × predictor
//! at 1920×1080, one slice, plus two content regimes the other benches
//! never touch: incompressible noise (a flat 8-bit code) and a
//! geometric residual distribution whose code reaches the encoder's
//! 16-bit cap (the decoder's beyond-the-lookup-table path).
//!
//! Plain timing loop rather than a statistics harness: each case runs
//! `RUNS` times and reports the **fastest** run next to the median. On a
//! machine shared with other jobs the minimum is the stable figure.
//!
//! ```text
//! cargo bench --bench families [-- <substring filter>]
//! ```

use std::hint::black_box;
use std::time::Instant;

use oxideav_utvideo::{
    EncodedFrame, Extradata, Fourcc, PlaneInput, Predictor, StreamConfig, decode_frame,
    encode_frame,
};

const W: u32 = 1920;
const H: u32 = 1080;

#[derive(Clone, Copy)]
enum Content {
    /// Smooth ramp plus 4 bits of noise.
    Natural,
    /// Uniform random bytes.
    Noise,
    /// Geometric symbol distribution (deep code table).
    Skewed,
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn plane(content: Content, w: usize, h: usize, idx: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ ((idx as u64) << 32) | 1;
    let mut out = Vec::with_capacity(w * h);
    for r in 0..h {
        for c in 0..w {
            let x = next(&mut state);
            out.push(match content {
                Content::Natural => (((r + c) >> 1) as u64 + ((x >> 40) & 0x0f)) as u8,
                Content::Noise => (x >> 40) as u8,
                Content::Skewed => {
                    (((x >> 20) | (1 << 30)).trailing_zeros() as u8).wrapping_mul(37)
                }
            });
        }
    }
    out
}

fn frame(fc: Fourcc, pred: Predictor, content: Content) -> EncodedFrame {
    let planes = (0..fc.plane_count())
        .map(|i| {
            let (pw, ph) = fc.plane_dim(i, W, H);
            PlaneInput::new(plane(content, pw as usize, ph as usize, i))
        })
        .collect();
    EncodedFrame::new(fc, W, H, pred, 1, planes)
}

fn config(fc: Fourcc) -> StreamConfig {
    StreamConfig::new(fc, W, H, Extradata::canonical_extradata_for(fc, 1).unwrap()).unwrap()
}

const FOURCCS: [(Fourcc, &str); 5] = [
    (Fourcc::Uly0, "ULY0"),
    (Fourcc::Uly2, "ULY2"),
    (Fourcc::Uly4, "ULY4"),
    (Fourcc::Ulrg, "ULRG"),
    (Fourcc::Ulra, "ULRA"),
];

const PREDICTORS: [(Predictor, &str); 4] = [
    (Predictor::None, "none"),
    (Predictor::Left, "left"),
    (Predictor::Gradient, "gradient"),
    (Predictor::Median, "median"),
];

fn cases() -> Vec<(String, Fourcc, EncodedFrame)> {
    let mut v = Vec::new();
    for (fc, fc_name) in FOURCCS {
        for (pred, pred_name) in PREDICTORS {
            v.push((
                format!("{fc_name}/{pred_name}/natural"),
                fc,
                frame(fc, pred, Content::Natural),
            ));
        }
    }
    for (content, name) in [(Content::Noise, "noise"), (Content::Skewed, "skewed")] {
        v.push((
            format!("ULY4/none/{name}"),
            Fourcc::Uly4,
            frame(Fourcc::Uly4, Predictor::None, content),
        ));
    }
    v
}

fn sample_bytes(fc: Fourcc) -> u64 {
    (0..fc.plane_count())
        .map(|i| {
            let (pw, ph) = fc.plane_dim(i, W, H);
            u64::from(pw) * u64::from(ph)
        })
        .sum()
}

const RUNS: usize = 15;

/// Fastest and median wall time of `RUNS` calls, in milliseconds.
fn time_ms<T>(mut f: impl FnMut() -> T) -> (f64, f64) {
    black_box(f()); // warm caches and the allocator
    let mut samples: Vec<f64> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            black_box(f());
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    (samples[0], samples[RUNS / 2])
}

fn report(name: &str, bytes: u64, (min, median): (f64, f64)) {
    println!(
        "{name:<34} min {min:>7.3} ms  median {median:>7.3} ms  {:>6.0} MB/s",
        bytes as f64 / 1e6 / (min / 1e3)
    );
}

fn main() {
    // `cargo bench` passes `--bench`; anything else is a name filter.
    let filter: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let wanted = |name: &str| filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str()));
    for (name, fc, frame) in &cases() {
        let payload = encode_frame(frame).unwrap();
        let cfg = config(*fc);
        let dec_name = format!("decode/{name}");
        if wanted(&dec_name) {
            report(
                &dec_name,
                sample_bytes(*fc),
                time_ms(|| decode_frame(&cfg, black_box(&payload)).unwrap()),
            );
        }
        let enc_name = format!("encode/{name}");
        if wanted(&enc_name) {
            report(
                &enc_name,
                sample_bytes(*fc),
                time_ms(|| encode_frame(black_box(frame)).unwrap()),
            );
        }
    }
}
