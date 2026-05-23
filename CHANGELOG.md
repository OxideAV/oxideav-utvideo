# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 9 — descriptor-mutation rejection + encoder API misuse +
  bit-pack/unpack invariants.** New
  `tests/round9_descriptor_and_api_robustness.rs` (23 tests) extends
  Round 8's negative-test surface along the dimensions Round 8
  deliberately left untouched. Round 8 fuzzed the slice-data span,
  pinning `SliceTruncated` / `HuffmanDecodeFailure`; it did **not** fuzz
  the 256-byte Huffman descriptor (a different guard family — the
  descriptor goes through `huffman::HuffmanTable::build`, not
  `decode_slice`, and trips `KraftViolation` /
  `MultipleSingleSymbolSentinels`). The other Round-8 omissions were
  the encoder's input-validation surface (`EncoderPlaneSizeMismatch`,
  `InvalidSliceCount`, `DimensionConstraint`) and the `BitWriter` /
  `BitReader` pair tested in isolation (Round 1..8 always tested them
  through a `HuffmanTable`).

  - **Plane-0 descriptor mutations** (5 tests). Starting from a valid
    encoded frame, mutate plane 0's 256-byte descriptor and confirm
    `decode_frame` surfaces the correct variant:
    - `MultipleSingleSymbolSentinels` when two distinct codelen-0
      entries are injected (`spec/05` §6.1 — only one single-symbol
      sentinel per plane).
    - `KraftViolation` on three histograms: incomplete (one codelen-1
      entry, Σ = 1/2), excess (three codelen-1, Σ = 3/2), and uniform
      codelen-1 (256 × 2^-1 = 128).
    - A full single-byte-flip sweep over the 256-byte descriptor
      asserts the no-panic / no-spurious-variant contract: every flip
      either decodes successfully (the residual stream happens to
      match an alternate but Kraft-valid descriptor) or fails with one
      of `KraftViolation`, `MultipleSingleSymbolSentinels`,
      `SliceTruncated`, `HuffmanDecodeFailure` — never any other
      variant, never a panic.

  - **Encoder API rejection** (5 tests). `encode_frame` surfaces:
    - `EncoderPlaneSizeMismatch` for wrong plane count (3 planes
      passed to ULRA which needs 4) and for wrong per-plane buffer
      length (ULY0 U-plane size 15 when 16 expected; the
      offending-plane index + expected + got fields are pinned).
    - `InvalidSliceCount` for `num_slices == 0` and `num_slices == 257`
      (the wire formula caps at 256).
    - `DimensionConstraint` for ULY0 with odd width (`spec/02` §3.2).

  - **`Extradata::ffmpeg_for` boundary** (3 tests). Round 6 tested the
    happy case; this adds the explicit rejection arms (0 slices and
    257 slices → `InvalidSliceCount`) and the upper-bound success case
    (256 slices → `flags` high byte = `0xff`, `num_slices() == 256`).

  - **`StreamConfig::new` cascade** (3 tests). Zero width and zero
    height surface `DimensionConstraint`; ULY2 with odd height is
    accepted (it chroma-subsamples by width only).

  - **`BitWriter` ⇄ `BitReader` round-trip invariants** (6 tests) in
    isolation, without going through `HuffmanTable`. Every code length
    `L ∈ 1..=32` round-trips exactly (200 codes per length × 32
    lengths = 6400 round-trip pairs); the bit-pack byte length is the
    exact multiple of 4 the spec mandates (`spec/05` §4.1); mixed-
    length code sequences cover every bit-offset transition within a
    32-bit word; `BitWriter::finish` on an empty writer returns an
    empty `Vec` (the encoder relies on this for the single-symbol
    zero-slice-data fast path, `spec/02` §5.1); a 33-bit write
    zero-pads the trailing partial word (`spec/05` §4.3);
    `BitReader::has_bits` at end-of-stream rejects `n > 0` and accepts
    `n == 0`; `BitReader::peek_bits` straddling a 32-bit-word boundary
    returns the expected MSB-first concatenation (4-bit codes 0xa, 0xb
    at positions [30..38] split across the first two words).

  Plus a `base_fixture_decodes_clean` positive control so an encoder
  regression surfaces here rather than masquerading as "everything in
  the suite mysteriously errors". All behaviour derived from
  `docs/video/utvideo/spec/02` + `docs/video/utvideo/spec/05`; the
  xorshift64*-flavoured PRNG is self-contained with no codec
  provenance. **141 tests** (+23), all green. Headline estimate
  unchanged at decode ~97% / encode ~96% — round 9 hardens the
  existing decode + encode surface (rejection paths +
  bit-pack/unpack invariants) rather than extending capability.

- **Round 8 — malformed-payload decode robustness (negative tests).**
  New `tests/round8_malformed_decode.rs` (11 tests) pins the decoder's
  defensive surface. Every prior round (1..7) exercises only the happy
  decode path (a frame the in-crate encoder produced, fed back through
  `decode_frame`); the `Err(...)` arms in `decoder::parse_payload` +
  `huffman::decode_slice` had a single smoke test
  (`round4_parallel_decode.rs` truncates 8 bytes and asserts
  `is_err()`) and **none asserted the specific `Error` variant**. For
  each malformed-payload condition the spec names, the suite starts
  from a valid encoder output and surgically mutates the wire bytes to
  trip exactly one decoder guard:

  - **`MissingFrameInfo`** — payload shorter than the trailing 4-byte
    frame-info dword (`spec/02` §6); swept over lengths 0..4.
  - **`ChunkTooShort`** at all three structural spans (`spec/02` §7):
    the 256-byte Huffman descriptor (4- and 104-byte payloads), the
    `num_slices × 4` offset table (4-slice frame, table truncated to 2
    offsets), and the slice-data span (inflate plane 0's slice-end
    offset past the present bytes). Plus a trailing-junk case (4 junk
    bytes inserted before the frame-info dword break the
    `offset == frame_info_off` exact-length invariant).
  - **`NonMonotonicSliceOffsets`** — 2-slice frame whose second
    slice-end offset is made strictly less than the first (`spec/02`
    §5).
  - **`SliceNotWordAligned`** — bump plane 0's slice-end offset by 1
    (the encoder always emits a multiple of 4, so `+1` is guaranteed
    non-aligned; `spec/05` §4.1).
  - **`SliceTruncated` / `HuffmanDecodeFailure`** — zero plane 0's
    entire slice-data span (bounded by reading the real slice-end
    offset, so the corruption never bleeds into the next plane's
    descriptor). The canonical table assigns the all-zero prefix to the
    longest code (`spec/05` §2.2), so an all-zero stream emits the
    max-length symbol every pixel and exhausts the bit budget before
    256 pixels are produced.

  Plus a **single-byte-flip sweep** over a real slice-data span
  asserting the **no-panic / no-spurious-variant contract**: a flipped
  bit either resyncs to a structurally complete frame (correct plane
  count + sample lengths) or is rejected as `SliceTruncated` /
  `HuffmanDecodeFailure` — never a panic, never an out-of-family error.
  A positive control (`base_fixtures_decode_clean`) re-decodes the
  unmutated base fixtures so a "passing" negative test can't hide a
  pre-broken base frame.

  All wire layout / error conditions derived from
  `docs/video/utvideo/spec/02` + `docs/video/utvideo/spec/05`; the
  xorshift64*-flavoured content source is a self-contained PRNG with no
  codec provenance. **118 tests** (+11), all green. Headline estimate
  unchanged at decode ~97% / encode ~96% — round 8 hardens the existing
  decode surface (rejection paths) rather than extending capability.

- **Round 7 — encoder byte-stability (idempotency) + full slice-count
  boundary sweep.** New `tests/round7_idempotency.rs` adds the two
  *byte*-level encoder invariants no prior round asserted (every
  earlier suite only checks the *pixel* round-trip `decode ∘ encode ==
  identity`):

  - **Deterministic, path-invariant encode.** `encode_frame` called
    twice on one frame, and `encode_frame` / `encode_frame_serial` /
    `encode_frame_parallel` on the same input, all emit byte-identical
    chunk payloads — pinning the Huffman tie-break (length DESC, sym
    DESC per `spec/05` §2.2) and the package-merge length build as
    deterministic, and re-stating the round-5 parallel-encode
    correctness guarantee as a byte equality rather than only a pixel
    one. 20 cells (5 FOURCCs × 4 predictors) at 320×216/8-slice so the
    auto-dispatch path actually selects the parallel branch.
  - **Byte-stable transcode fixed point.** `encode ∘ decode ∘ encode`
    reproduces the first encode's bytes exactly across 5 FOURCCs ×
    4 predictors × 3 entropy regimes × 2 slice counts (120 cells) at a
    non-divisible height (96×70). Strictly stronger than pixel
    round-trip: a non-canonical Huffman build or scratch-state-
    dependent slice partition would pass pixel round-trip but break
    byte-stability.

  Plus a **full `num_slices ∈ 1..=256` boundary sweep** at heights
  deliberately chosen so `ph % N != 0` for most `N` and `N > ph` for
  the tail (forcing uneven-row and zero-row slices, the latter
  carrying zero slice-data bytes per `spec/02` §5.1) — ULY0 64×70,
  ULY2 62×50, ULY4 24×45 (all four predictors), ULRG/ULRA 30×39 — each
  cell round-trips and (for `N <= 64`) re-checks the byte-stable fixed
  point. A focused edge test covers the exact `ph*(s+1)/N`
  integer-division transition at `N ∈ {ph-1, ph, ph+1, ph+7}`. All
  behaviour derived from `docs/video/utvideo/spec/`; the xorshift64*
  content source is a self-contained PRNG with no codec provenance.
  **107 tests** (+7), all green.

- **Round 6 — FFmpeg-pinned extradata builder + content-fixture corpus.**
  New [`Extradata::ffmpeg_for(fourcc, num_slices)`] builder produces the
  16-byte extradata block FFmpeg 7.1.2's `utvideo` encoder writes for
  every FOURCC at every slice count `1..=256`, byte-identical to
  `spec/01` §5 test-set `T1`. New [`Fourcc::ffmpeg_source_format_tag`]
  accessor exposes the per-FOURCC 4-byte tag (`"YV12"` / `"YUY2"` /
  `"YV24"` / `00 00 01 18` / `00 00 02 18`) without forcing the caller
  to construct an Extradata. Together these close
  [`audit/00-report.md`](../../docs/video/utvideo/audit/00-report.md)
  §5.2 implementer-resolvable open items 1 (encoder-version: mirror
  FFmpeg's `0x0100_00f0`) and 2 (RGB source-format tag: mirror
  FFmpeg's `00 00 01 18` / `00 00 02 18`).

  New content-fixture corpus (`tests/round6_content_fixtures.rs`)
  exercises eight content-style synthetic patterns (solid, horizontal
  gradient, diagonal gradient, vertical stripes 4-wide, horizontal
  stripes 4-tall, 8×8 binary checker, LCG noise, sparse impulses) ×
  four predictors × five FOURCCs at 128×96, plus a 16-cell 256×192
  8-slice smoke pass and a four-cell compressed-size headline
  measurement. Beyond the existing round-2 self-roundtrip equality,
  round-6 introduces **compressed-size bounds** as audit/01 §8 item 4
  ("wider slice-count and resolution corpus … compressed size within
  X% of FFmpeg") recommended:

  - **Universal upper bound** on every cell: `8 bits/sample ×
    total_samples + per-plane overhead`, with 10% slack. Catches an
    encoder regression that drops back to flat 8-bit-per-pixel.
  - **Solid pattern exact-bound**: `3 * (256 + 4 * num_slices) + 4`
    bytes (single-symbol Huffman per plane → zero slice-data bytes
    per `spec/02` §5.1). Locks down the single-symbol fast path.
  - **Very-compressible bound** (`VerticalStripes`+`Left`,
    `HorizontalStripes`+`Gradient`, `GradientX`+`Left`): ≤ 3
    bits/sample.
  - **Compression-quality ordering invariants**:
    `Solid << GradientDiag/Gradient` (highly predictable << ~7-symbol
    histogram) and `GradientDiag/Gradient * 2 < Noise/None` (well-
    predicted signal half-or-less the unpredicted-noise size).

  336-cell content matrix + 4-cell headline measurement is fully
  byte-exact self-roundtripped; this is regression sentinel coverage
  for any future predictor / Huffman / parallel-encode change.

  Total test count: **100** = 61 unit (+9 from round 5) + 16 round-2
  matrix + 6 round-3 LUT + 6 round-4 parallel-decode + 7 round-5
  parallel-encode + **4 round-6 content fixtures**.

  Wall: spec/00 + spec/01 + spec/02 + spec/04 + spec/05 (read-only) +
  audit/00-report.md (read-only, for §5.2 + §8.4 directions only).
  No reference-impl/python read; no external library source.

- **Round 5 — slice-parallel encode.** Mirror of the round-4
  decoder fan-out. `encode_frame` now auto-dispatches multi-slice
  frames whose luma pixel count crosses
  [`encoder::PARALLEL_PIXEL_THRESHOLD`] (64 Ki pixels, same threshold
  as the decoder) onto a `std::thread::scope` thread pool sized at
  `min(num_slices, available_parallelism())`. Within each plane the
  fan-out covers both stages that are slice-independent per the spec:
  - **Forward predict** (`predict::forward_slice`): every slice's
    first-pixel seed is `128` (`spec/04` §§3.1, 4, 5, 7), and the
    predictor reads only samples in its own row range. The slices
    fan out into disjoint mutable slots of a pre-sized
    `Vec<Vec<u8>>` (one slot per slice).
  - **Per-slice Huffman bit-pack**: the per-plane code-length
    descriptor is built once on the parent thread (it needs the
    cross-slice histogram), then each slice's `BitWriter`
    invocation runs on a worker. Every slice's Huffman bit-stream
    is a self-contained byte blob (`spec/02` §5), so the packs
    are fully independent.

  Per-plane work itself stays plane-serial: per-plane outputs are 1–4
  blobs and the per-plane Huffman build is a single histogram + a
  package-merge length build. The slice-level parallelism within a
  plane already saturates the pool with 8 slices on an 8-core host.

  Output of the parallel path is **byte-identical** to the serial
  path on every fixture in the round-5 matrix (288 ULY0 cells +
  6 RGB cells + 256-slice stress + perf smoke). Explicit
  `encode_frame_serial` / `encode_frame_parallel` entry points are
  kept alongside the auto-dispatching `encode_frame` for
  latency-sensitive single-frame callers or for callers driving a
  foreign thread-pool.

  Measured wall-clock on an 8-core host (release build, gradient
  predictor, 8-slice ULY4 luma + UV at 4:4:4):
  | Frame    | Serial   | Parallel | Speedup |
  | -------- | -------- | -------- | ------- |
  | 320×240  | 1.94 ms  | 1.72 ms  | 1.13×   |
  | 640×480  | 3.40 ms  | 1.75 ms  | 1.94×   |
  | 1280×720 | 9.29 ms  | 2.84 ms  | 3.28×   |

  Speedup gap vs. the round-4 decoder (5.63× at 1280×720) reflects
  the encoder's heavier serial-prelude: the per-plane Huffman build
  (256-bin histogram + package-merge length build) is single-threaded
  by construction (the parallel slices share one codebook), so the
  serial fraction stays Amdahl-bounded above the slice fan-out.

  New `predict::forward_slice` helper produces residuals for one
  slice's row range with the universal `+128` seed in isolation;
  preserves the existing `predict::forward` plane-level entry for
  the serial path.

  Test suite at `tests/round5_parallel_encode.rs` (7 tests): ULY0
  matrix (288 cases of W × H × slices × predictor; byte-equal serial
  vs. parallel), RGB family (ULRG + ULRA across 3 predictors), auto-
  dispatch threshold equivalence, 1-slice serial-equiv, 256-slice
  one-row stress, parallel-encode → serial+parallel-decode end-to-end
  roundtrip, and a perf smoke. 87 tests total = 52 unit + 16
  round-2 matrix + 6 round-3 LUT + 6 round-4 parallel-decode + 7
  round-5 parallel-encode.

- **Round 4 — slice-parallel decode.** `decode_frame` now
  auto-dispatches multi-slice frames over
  `PARALLEL_PIXEL_THRESHOLD` (64 Ki pixels, hand-picked from the
  perf-smoke matrix) onto a `std::thread::scope` pool sized at
  `min(num_slices, available_parallelism())`. The per-plane Huffman
  table is built once on the parent thread, then the per-slice
  decode + inverse-predict run on disjoint mutable row strips of the
  output buffer (`split_at_mut`), so no synchronisation is needed
  inside a plane. The first failing slice's error wins on the join.
  - Slices are fully independent per the spec: the +128 first-pixel
    seed restarts at every slice (`spec/04` §§3.1, 4, 5, 7) and
    every slice's Huffman bit-stream is self-contained (`spec/02`
    §5). The parallel path is therefore bit-exact equivalent to the
    serial path, verified across a 192-cell ULY0 W×H×slices×predictor
    matrix plus dedicated ULRG / ULRA / 256-slice / single-slice
    probes (`tests/round4_parallel_decode.rs`, 6 tests).
  - Explicit `decode_frame_serial` and `decode_frame_parallel` entry
    points kept alongside the auto-dispatching `decode_frame` for
    latency-sensitive single-frame callers or for callers that want
    to drive a foreign thread-pool.
  - Measured wall-clock on an 8-core host (release build, gradient
    predictor, 8-slice ULY4 luma+UV):
    | Frame    | Serial   | Parallel | Speedup |
    | -------- | -------- | -------- | ------- |
    | 320×240  | 1.44 ms  | 0.50 ms  | 2.87×   |
    | 640×480  | 3.62 ms  | 0.76 ms  | 4.76×   |
    | 1280×720 | 8.95 ms  | 1.59 ms  | 5.63×   |
  - New `predict::apply_slice` helper applies inverse prediction to
    one slice's row strip in isolation; preserves the existing
    `predict::apply` plane-level entry for the serial path.

- **Round 3 — LUT-accelerated Huffman decode.** The per-plane
  Huffman decoder now caches a flat `2^12 = 4096`-entry lookup
  table on `HuffmanTable::build` and resolves any code of length
  `<= 12` bits in one shift+load. Codes longer than 12 bits (which
  `spec/02` §4.2 documents as topping out at 16 bits empirically)
  fall back to the existing length-tier binary-search prefix scan.
  Pure-LUT planes (most frames in the behavioural corpus) skip the
  tier scan entirely.
  - `BitReader::peek_bits` rewritten to combine the current and
    next 32-bit LE words into a 64-bit register and shift to align,
    replacing the prior `O(n)` bit-by-bit byte read.
  - New integration suite at `tests/round3_lut_decode.rs` (6 tests)
    covering: high-entropy LCG noise across every FOURCC, mandelbrot
    iteration patterns (the `spec/02` §4.2 R2-mandelbrot deep-tree
    case), high-slice-count deep-codelen probe, pure-LUT-path
    stripe-after-median test, and 320×240 perf smoke.
  - Three new in-module Huffman tests pin LUT-slot population,
    `LUT_MISS` sentinel coverage for `> LUT_BITS` codes, and short-
    code resolution at the bit-stream tail.

  Decoder correctness preserved bit-for-bit against the existing
  round-1 + round-2 corpus (74 tests total, all green).

- **Round 2 — exhaustive pattern matrix.** New integration suite at
  `tests/round2_pattern_matrix.rs` that mirrors the Auditor Round 1
  test matrix (`docs/video/utvideo/audit/01-validation-report.md`
  §3.1) into a Rust self-roundtrip suite. Coverage:
  - 5 FourCC × 4 predictor × 8 patterns
    (`zeros`, `mid`, `ones`, `gradient`, `ramp_x`, `ramp_y`,
    `checker`, deterministic-LCG `random`) × 11 sizes
    (`2×2..64×48` plus odd-W and odd-H corners) × {1, 2, 4, 8} slices,
    filtered against `spec/02` §3.2 dimension constraints and the
    "every slice must contain at least one row" sanity guard.
  - Edge-case probes: 1×1 (ULY4 / ULRG / ULRA only), 2×2, thin-strip
    1×N and N×1, tall-thin 8×240, wide-short 1280×8, 16-slice
    one-row-per-slice, ULRA non-trivial alpha (verifies the alpha
    plane bypasses the `spec/04` §6 RGB decorrelation), solid-colour
    sweep over 7 luma values and every FourCC × predictor pair.
  - Encoder-determinism probe (`double_roundtrip_bytes_stable`):
    `encode → decode → re-encode` produces identical bytes — a
    constructive check on the package-merge tie-breaking.

  This adds 16 integration tests on top of round 1's 49 unit tests
  (65 total) and exercises ~3 000 self-roundtrip cells per `cargo
  test` invocation. Round 1 already implemented every documented
  FourCC + predictor + RGB decorrelation; round 2 broadens the
  corpus to the Auditor's 1018-cell Variant-B matrix without
  introducing new wire-format surface.

- **Round 1 — classic-family decoder + encoder.** Full Ut Video
  classic-family wire-format support: ULRG / ULRA / ULY0 / ULY2 /
  ULY4. Built clean-room against `docs/video/utvideo/spec/00..06`
  (no FFmpeg / Win32 / VLC source read). Public surface:
  - [`Fourcc`] (5 variants) + [`Extradata`] parsing per `spec/01`.
  - [`decode_frame`] — `00dc` chunk payload → per-plane decoded
    samples; walks plane-by-plane per `spec/02`, applies the
    `frame_info`-named predictor per `spec/04`, undoes RGB
    decorrelation for ULRG/ULRA per `spec/04` §6.
  - [`encode_frame`] — per-plane samples → chunk payload; mirror
    of the decoder pipeline for self-roundtrip testing.
  - [`HuffmanTable`] — RFC-1951-mirrored canonical Huffman code
    construction per `spec/05` §2.2; bit reader / writer for
    32-bit-LE-word, MSB-first-within-word slice data per
    `spec/05` §4.
  - [`Predictor`] (None / Left / Gradient / Median) with per-slice
    +128 first-pixel seed per `spec/04` §§3, 4, 5, 7.
  - `register_codecs` / `register` — wire into `oxideav-core`'s
    [`CodecRegistry`] under codec id `"utvideo"` with all five
    classic FourCCs claimed.
- Self-roundtrip integration suite covering 5 FourCCs × 4
  predictors × {1, 2, 3, 4, 7, 8} slice counts, plus solid-colour,
  high-entropy, and non-square dimension corners. 49 tests total.

### Notes

- AVI / VfW carriage (FourCC handling at the container level,
  `00dc` chunk wrapping, `BITMAPINFOHEADER` extradata, `idx1`
  reservation) is **out of scope** for this crate per the
  workspace policy: that work belongs in `oxideav-avi`. Callers
  hand us [`StreamConfig`] + `00dc` chunk-payload bytes; we hand
  back per-plane samples.
- Round 1 deliberately defers FFmpeg byte-equality (no behavioural
  fixture corpus is in `tables/` yet); decoder correctness is
  pinned by an in-crate self-roundtrip and by spec-derived unit
  tests reproducing the byte traces in `spec/05` §3.1 verbatim.

### Changed

- Clean-room rebuild from a fresh orphan `master`. The previous
  implementation was retired by the OxideAV docs audit dated
  2026-05-06; the prior history is preserved on the `old` branch.
  See `README.md` for the rebuild scope and the strict-isolation
  workspace the Implementer rounds will draw from.
