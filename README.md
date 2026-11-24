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
black-box decoder does accept this crate's gradient streams), slice
counts above the smallest plane height, and anything outside the five
FourCCs below.

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
keeps the win measured — on a 16-core host at 1280×720 ULY4 with 8
slices: decode ≈5.8× at 4 workers (≈6.8× at max), encode ≈3.5× at 4
workers (Amdahl-bounded by the single-threaded per-plane Huffman
build).

**Behaviour change:** before round 420, `decode_frame` /
`encode_frame` silently fanned out across host threads for large
multi-slice frames. If you relied on that, pass a budget explicitly or
grant one through `set_execution_context`.

### Not yet supported

- High-bit-depth FourCCs (`ULH0`, `ULH2`, 10-bit ULY4) — blocked on
  out-of-corpus docs.
- Ut Video Lite and interlaced variants — blocked on out-of-corpus
  docs.
- Raw / non-Huffman slice mode (`flags & 0x00000001 == 0`) — not
  observed in the corpus.

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
  registry under codec id `"utvideo"`.

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
