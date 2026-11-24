# oxideav-utvideo

[![CI](https://github.com/OxideAV/oxideav-utvideo/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-utvideo/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-utvideo.svg)](https://crates.io/crates/oxideav-utvideo) [![docs.rs](https://docs.rs/oxideav-utvideo/badge.svg)](https://docs.rs/oxideav-utvideo) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust Ut Video classic-family lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.
Clean-room implementation against the spec under
`docs/video/utvideo/`.

## Status

Decoder and encoder for the five 8-bit FourCCs the spec documents
(`ULY0` / `ULY2` / `ULY4` / `ULRG` / `ULRA`), measured against a **live
black-box matrix** (`tests/blackbox_matrix.rs`; generated at test time,
skipped when the `ffmpeg` executable is absent):

| Direction | Cells | Result |
| --------- | ----- | ------ |
| black-box encoder → this decoder | 7 469 | **6 858 / 6 858** verifiable cells bit-exact (vs the black-box decoder *and* the source picture); 611 cells have no oracle — the black-box decoder rejects the black-box encoder's own stream |
| this encoder → black-box decoder | 9 565 | **9 565 / 9 565** accepted with an empty warning log and bit-exact |

The sweep covers every pixel layout the black-box encoder takes
(`yuv420p` / `yuv422p` / `yuv444p` / `gbrp` / `gbrap`), every predictor
it offers (none / left / median — plus gradient in the encode
direction), every slice count the format allows on a 16×262 picture
(1 … 131 for 4:2:0, 1 … 256 otherwise, most of which do not divide the
height), 1×1 to 4096×2160 pictures including odd and one-pixel-wide
ones, flat / mid-grey / noise / ramp / sparse / checker / soft / skewed
content (single-symbol planes, uniform 8-bit tables, deep code tables)
and 150-frame streams. On the cells both encoders can produce, this
encoder's output is within 0.01 % of the black-box encoder's size.

The 611 oracle-less cells are all multi-slice, never 4:2:0, and mostly
tiny or incompressible pictures: the black-box encoder writes a frame
its own decoder refuses ("Incorrect slice size" / "Insufficient data
for a plane"). This crate rejects or mis-decodes those frames too
(none decodes to the source picture), and its own encoder covers the
same option cells in the other direction.

**Never verified against an outside stream:** gradient prediction in
the decode direction (the black-box encoder does not offer it; the
black-box decoder does accept this crate's gradient streams), code
lengths above 16 bits (hand-built frames only; cross-checked against
the black-box decoder up to 32), slice counts above the smallest plane
height, and anything outside the five FourCCs below.

The crate is codec-only: AVI / VfW container handling (including the
FourCC + extradata that identifies a Ut Video stream on the wire) lives
in `oxideav-avi`. Callers hand in a parsed `StreamConfig` + frame
bytes; the codec returns per-plane samples (decode) or chunk-payload
bytes (encode).

### Supported

The five classic 8-bit FourCCs documented in the spec:
`Uly0` / `Uly2` / `Uly4` (YUV 4:2:0 / 4:2:2 / 4:4:4), `Ulrg` (RGB),
`Ulra` (RGBA). All four spatial predictors (None / Left / Gradient /
Median, spec §04, with the per-slice `+128` first-pixel seed), the
per-plane canonical-Huffman codebooks (spec §05), per-slice
partitioning (spec §02 — on the vertically-subsampled row grid, so a
4:2:0 luma slice always starts on an even row and holds whole chroma
rows), and RGB inter-plane decorrelation (spec §04
§6) are implemented. Both decode and encode have a slice-parallel path
gated on a caller-granted worker budget (see **Threading** below).
The trait-path encoder picks a predictor per frame with a
content-adaptive entropy heuristic; the direct API takes an explicit
predictor.

## Performance

Single thread, 1920×1080, one slice, Apple-silicon laptop shared with
other jobs. `cargo bench --bench families` times every FourCC ×
predictor (15 runs each, fastest run reported); "before" is the
previous release of this crate under the same harness, the two builds
run alternately and the better of two passes taken for each. MB/s is
raw sample bytes per second. Output is byte-identical before and after
(encoder: 1 200 frames compared; decoder: the black-box matrix).

| FourCC | predictor | decode before → after | MB/s | encode before → after | MB/s |
| ------ | --------- | --------------------- | ---- | --------------------- | ---- |
| ULY0 | none | 3.7 → **2.5 ms** (1.48×) | 1255 | 6.6 → **3.5 ms** (1.90×) | 895 |
| ULY0 | left | 8.5 → **4.6 ms** (1.85×) | 677 | 13.6 → **3.1 ms** (4.33×) | 990 |
| ULY0 | gradient | 9.7 → **7.0 ms** (1.40×) | 447 | 13.3 → **3.1 ms** (4.37×) | 1017 |
| ULY0 | median | 12.9 → **8.9 ms** (1.44×) | 348 | 14.1 → **3.1 ms** (4.63×) | 1017 |
| ULY2 | none | 5.3 → **3.2 ms** (1.68×) | 1306 | 10.3 → **4.3 ms** (2.39×) | 963 |
| ULY2 | left | 13.1 → **6.9 ms** (1.91×) | 603 | 20.2 → **4.1 ms** (4.91×) | 1007 |
| ULY2 | gradient | 14.3 → **9.4 ms** (1.52×) | 442 | 19.4 → **4.4 ms** (4.44×) | 950 |
| ULY2 | median | 19.9 → **13.0 ms** (1.53×) | 319 | 19.4 → **4.4 ms** (4.42×) | 943 |
| ULY4 | none | 7.9 → **5.2 ms** (1.53×) | 1199 | 15.2 → **7.0 ms** (2.16×) | 883 |
| ULY4 | left | 18.5 → **10.8 ms** (1.71×) | 575 | 27.1 → **6.7 ms** (4.04×) | 926 |
| ULY4 | gradient | 19.5 → **14.0 ms** (1.39×) | 445 | 26.8 → **6.8 ms** (3.94×) | 915 |
| ULY4 | median | 26.9 → **18.6 ms** (1.44×) | 334 | 26.5 → **6.5 ms** (4.05×) | 951 |
| ULRG | none | 13.0 → **7.7 ms** (1.68×) | 806 | 23.7 → **6.8 ms** (3.47×) | 908 |
| ULRG | left | 17.2 → **11.2 ms** (1.54×) | 557 | 26.1 → **6.9 ms** (3.78×) | 901 |
| ULRG | gradient | 21.5 → **15.5 ms** (1.39×) | 402 | 28.5 → **7.1 ms** (4.03×) | 879 |
| ULRG | median | 26.4 → **19.5 ms** (1.35×) | 319 | 28.4 → **6.6 ms** (4.28×) | 939 |
| ULRA | none | 17.6 → **9.1 ms** (1.94×) | 916 | 30.1 → **8.9 ms** (3.38×) | 931 |
| ULRA | left | 26.0 → **14.1 ms** (1.84×) | 588 | 40.4 → **8.7 ms** (4.63×) | 950 |
| ULRA | gradient | 32.8 → **19.5 ms** (1.68×) | 425 | 41.2 → **9.1 ms** (4.53×) | 913 |
| ULRA | median | 43.1 → **25.1 ms** (1.72×) | 330 | 40.7 → **8.7 ms** (4.70×) | 957 |
| ULY4 | none, noise | 8.8 → **4.8 ms** (1.84×) | 1303 | 15.7 → **6.5 ms** (2.43×) | 962 |
| ULY4 | none, skewed | 18.8 → **7.6 ms** (2.48×) | 820 | 37.9 → **6.2 ms** (6.16×) | 1010 |

"natural" is a smooth ramp plus four bits of noise; "noise" is uniform
random bytes (a flat 8-bit code); "skewed" is a geometric symbol
distribution whose code reaches the encoder's 16-bit cap.

Where the time went:

- **Decoder** — residuals are decoded straight into the output plane
  and predicted in place (no per-slice residual buffer, no copy, no
  copy of the G plane for RGB); when two codes fit in one 12-bit table
  index a companion table yields two symbols per probe with a
  branch-free refill; the gradient predictor keeps one add on its
  serial chain and the median is written so that only two operations
  depend on the running sample.
- **Encoder** — a per-slice histogram gives every slice's packed size
  before packing, so slices are packed directly into the final buffer
  by a branch-free 64-bit packer taking two codes per step; forward
  predictors read only source samples over zipped rows (they
  vectorise); source planes are no longer copied (only the two
  decorrelated RGB planes are).

## Threading

**Serial by default — parallelism is opt-in (round 420, execution
context contract).** The crate never queries host parallelism; every
internal `std::thread::scope` fan-out is bounded by a caller-granted
worker budget clamped to the slice count
(`workers.min(num_slices).max(1)`).

- **Registry / trait path:** the `Decoder` and `Encoder` impls
  override `set_execution_context` and store the
  `oxideav_core::ExecutionContext::threads` budget. With no call the
  trait path runs single-threaded — the contract default. Grant
  `threads > 1` to enable the slice-parallel path for multi-slice
  frames above the 64 Ki-pixel threshold.
- **Direct API:** `decode_frame` / `encode_frame` are serial;
  `decode_frame_with_workers` / `encode_frame_with_workers` take an
  explicit budget with the same threshold gate; the forced
  `*_parallel` entry points take the budget with no threshold gate.

The budget is a pure scheduling input: output is **byte-identical for
every budget** (pinned across the whole 19-fixture reference corpus at
budgets 1/2/8, on both the direct and trait surfaces, by
`tests/round420_execution_invariance.rs`). The `thread_scaling` bench
keeps the win measured — re-measured this round at 1280×720 ULY4, 8
slices, gradient predictor: decode 5.3 ms → 2.8 ms at 2 workers →
1.5 ms at 4 (3.5×) → 0.96 ms at 8 or more (5.5×); encode 2.5 ms →
1.5 ms at 2 → 1.0 ms at 4 (2.5×), with nothing further beyond 4 (the
per-plane code-length build is serial and each plane opens two short
thread scopes). An earlier revision of this file claimed 5.8× decode
at 4 workers; that figure was not reproducible.

**Behaviour change:** before round 420, `decode_frame` /
`encode_frame` silently fanned out across host threads for large
multi-slice frames. If you relied on that, pass a budget explicitly or
grant one through `set_execution_context`.

Code lengths over the whole wire range are decoded: the descriptor
byte allows 1 … 254 bits (spec §05 §7.2), codes up to 32 bits go
through a 12-bit table with a tier search behind it, longer ones
through a bit-serial walk of the length histogram. Hand-built frames
at every depth from 1 to 254 bits decode exactly
(`tests/long_codes.rs`); the black-box decoder reproduces the same
frames up to 32 bits and declines beyond. The encoder caps its own
codes at 16 bits.

### Not supported

Each of these is refused with a typed error; none is specified by the
clean-room documents this crate is built from, and the black-box
encoder emits none of them (probed: it maps every 10-bit / grey /
packed input to one of the five 8-bit layouts above, and never sets
the interlaced flag):

- Interlaced streams (extradata flag `0x00000800`).
- Raw / non-Huffman slices (extradata flag `0x00000001` clear).
- Any other FourCC of the wider family (10-bit, alternate-matrix and
  fast variants).

## Public API

- [`decode_frame`] — decode one `00dc` chunk payload into per-plane
  samples (`DecodedFrame`), single-threaded.
  [`decode_frame_with_workers`] adds a caller-granted worker budget
  for the slice-parallel path. [`decode_frame_strict`] is an opt-in
  conformance variant: byte-identical output for any well-formed
  stream, but it additionally verifies each slice's trailing
  word-boundary padding is zero (spec §05 §4.3 / §8) and rejects a
  non-zero padding bit with a located `Error::NonZeroPadding`.
- [`encode_frame`] — encode per-plane samples into one chunk payload,
  single-threaded. [`encode_frame_with_workers`] adds the worker
  budget. Explicit `*_serial` / `*_parallel(workers)` entry points are
  available for latency-sensitive or threadpool-controlled callers.
- [`Fourcc`] / [`Extradata`] / [`StreamConfig`] / [`Predictor`] — the
  identification surface. `Extradata::canonical_extradata_for(fourcc, num_slices)`
  builds the canonical 16-byte extradata block for the named FourCC.
- [`inspect`] — a decode-free byte-walk (`peek_frame` /
  `peek_frame_info`) returning a typed `FrameLayout` of per-plane
  `PlaneLayout` / `SliceLayout` records: descriptor / slice-table /
  slice-data byte offsets, per-slice row partitioning, and per-plane
  Huffman-descriptor primitives (`active_symbol_count`,
  `min`/`max_code_length`, the code-length histogram, the
  Kraft-completeness predicate). It runs the same parse rules and
  surfaces the same `Error` variants as the full decoder but builds no
  Huffman table and allocates no residual buffer, so container indexers
  and diagnostic tools can read the per-frame layout without paying the
  per-pixel decode cost.
- [`Error`] / [`ErrorCategory`] — the failure surface, classified into
  `MalformedStream` / `ApiMisuse` / `Unsupported` / `StreamShape`
  buckets with `is_*` convenience predicates.
- [`register`] / [`register_codecs`] — wire into `oxideav-core`'s codec
  registry under codec id `"utvideo"`. The decoder reports its frame
  shape through `output_params()` as soon as it is configured
  (`Yuv420P` / `Yuv422P` / `Yuv444P` / `Gbrp8` / `Gbrap8`, planes in
  wire order); the encoder selects the FourCC from those same five
  pixel formats (or from `CodecParameters::tag`) and takes two options
  through `CodecParameters::options` ([`UtVideoEncoderOptions`]):

  | key      | values                                           | default |
  | -------- | ------------------------------------------------ | ------- |
  | `slices` | `1` … `256`, at most the smallest plane's height; `0` keeps the extradata's count (one slice without extradata) | `0` |
  | `pred`   | `auto` / `none` / `left` / `gradient` / `median` | `auto` (chosen per frame) |

## Cargo features

- **`registry`** (default): wire the crate into `oxideav-core`'s codec
  registry.

## Testing

The crate ships per-stage unit tests, a self-roundtrip matrix across
every FourCC × predictor × slice-count combination, a cross-surface
decode-agreement harness that asserts the five decode entry points
(serial default / budgeted / forced-serial / forced-parallel / strict)
reproduce byte-identical frames on every self-encoded stream and never
diverge or panic on arbitrary bytes, an execution-budget invariance
suite (`round420`) pinning byte-identical decode and encode output
across worker budgets 1/2/8 on the full reference corpus through both
the direct and registry trait surfaces, a
**reference-stream golden corpus** (`tests/fixtures/reference/`: 19
pre-extracted frame bodies + wire extradata + reference-decoder pixel
ground-truth, decoded byte-exact and cross-checked through the strict,
serial, parallel, and inspector surfaces, with the extradata builder
pinned to captured wire bytes), a malformed-payload
rejection suite, criterion benchmarks (`benches/`), and four
`cargo-fuzz` targets (`decode_utvideo`, `encode_utvideo_frame`,
`inspect_utvideo`, `huffman_codec`) that exercise the decode, encode,
inspector, and Huffman-codebook surfaces for panic-freedom and
cross-surface invariant agreement, each with a deterministic stable-CI
mirror. The decode-free inspector's Kraft accessors are hardened against
the full `spec/05` §7.2 wire range of code lengths (`1..=254`): a
malformed descriptor declaring a code length past the 128-bit shift
boundary is handled with saturating `kraft_numerator` arithmetic and a
node-merge `is_kraft_complete` predicate that never materialises `2^max`,
so the byte-walk stays panic-free on any descriptor a decoder would
reject. Fixture reference output is produced by black-box invocations
of a validator binary; no third-party codec library source is consulted
at any stage.

## License

MIT — see [LICENSE](./LICENSE).
