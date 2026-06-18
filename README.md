# oxideav-utvideo

Pure-Rust Ut Video classic-family lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.
Clean-room implementation against the spec under
`docs/video/utvideo/`.

## Status

Decode and encode are both functional at roughly **97%** coverage of
the classic 8-bit FourCC family. The crate is codec-only: AVI / VfW
container handling (including the FourCC + extradata that identifies a
Ut Video stream on the wire) lives in `oxideav-avi`. Callers hand in a
parsed `StreamConfig` + frame bytes; the codec returns per-plane
samples (decode) or chunk-payload bytes (encode).

### Supported

The five classic 8-bit FourCCs documented in the spec:
`Uly0` / `Uly2` / `Uly4` (YUV 4:2:0 / 4:2:2 / 4:4:4), `Ulrg` (RGB),
`Ulra` (RGBA). All four spatial predictors (None / Left / Gradient /
Median, spec §04, with the per-slice `+128` first-pixel seed), the
per-plane canonical-Huffman codebooks (spec §05), per-slice
partitioning (spec §02), and RGB inter-plane decorrelation (spec §04
§6) are implemented. Both decode and encode have a slice-parallel path
that auto-dispatches multi-slice frames above a pixel-count threshold.
The trait-path encoder picks a predictor per frame with a
content-adaptive entropy heuristic; the direct API takes an explicit
predictor.

### Not yet supported

- High-bit-depth FourCCs (`ULH0`, `ULH2`, 10-bit ULY4) — blocked on
  out-of-corpus docs.
- Ut Video Lite and interlaced variants — blocked on out-of-corpus
  docs.
- Raw / non-Huffman slice mode (`flags & 0x00000001 == 0`) — not
  observed in the corpus.

## Public API

- [`decode_frame`] — decode one `00dc` chunk payload into per-plane
  samples (`DecodedFrame`). [`decode_frame_strict`] is an opt-in
  conformance variant: byte-identical output for any well-formed
  stream, but it additionally verifies each slice's trailing
  word-boundary padding is zero (spec §05 §4.3 / §8) and rejects a
  non-zero padding bit with a located `Error::NonZeroPadding`.
- [`encode_frame`] — encode per-plane samples into one chunk payload.
  Explicit `*_serial` / `*_parallel` entry points are available for
  latency-sensitive or threadpool-controlled callers.
- [`Fourcc`] / [`Extradata`] / [`StreamConfig`] / [`Predictor`] — the
  identification surface. `Extradata::ffmpeg_for(fourcc, num_slices)`
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
every FourCC × predictor × slice-count combination, a malformed-payload
rejection suite, criterion benchmarks (`benches/`), and four
`cargo-fuzz` targets (`decode_utvideo`, `encode_utvideo_frame`,
`inspect_utvideo`, `huffman_codec`) that exercise the decode, encode,
inspector, and Huffman-codebook surfaces for panic-freedom and
cross-surface invariant agreement, each with a deterministic stable-CI
mirror. Fixture reference output is produced by black-box invocations
of a validator binary; no third-party codec library source is consulted
at any stage.

## License

MIT — see [LICENSE](./LICENSE).
