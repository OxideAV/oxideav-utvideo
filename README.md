# oxideav-utvideo

Pure-Rust Ut Video lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 18 — content-adaptive trait-path predictor heuristic.** Round
17 wired the `oxideav_core::Encoder` trait path with a hardcoded
`Predictor::Gradient` for every frame: callers driving the codec
through the framework's trait surface had no way to switch predictor
short of dropping out of the trait and using the direct
`encode_frame(EncodedFrame { predictor, .. })` API. Round 18 replaces
that hardcoded default with a per-frame entropy-driven heuristic.
The new `predict::choose_predictor(plane, width, plane_height) ->
Predictor` samples up to `HEURISTIC_SAMPLE_ROWS = 8` leading rows of
the input plane under each of the four candidate predictors (None /
Left / Gradient / Median), computes `Σ count[s] · log2(N / count[s])`
on each residual histogram (the Huffman code-length lower bound per
`spec/05` §2.2), and picks the predictor with the lowest bit cost.
The trait encoder runs this on plane 0 (luma for YUV / G for RGB)
and applies the result to every plane of the frame — matching the
single per-frame predictor that `frame_info` bits 8..9 encode on
the wire (`spec/02` §6.1). Tie-break order is `Gradient → Median →
Left → None`, mirroring the round-15/16 dense-kernel benchmark
ordering (Gradient was both the fastest dense kernel AND most often
the best compressor on natural content). The direct-API
`encode_frame(EncodedFrame { predictor, .. })` path is unchanged —
callers that hand in an explicit predictor get it verbatim. Round 18
also adds `UtVideoEncoder::set_predictor(Some(Predictor::X))` as an
override hook so callers that need to pin a specific predictor (or
restore exact round-17 byte-equality) can do so without touching the
direct API.

New `tests/round18_predictor_heuristic.rs` (17 tests) plus 3 new
`src/registry.rs` unit tests pin five invariant groups:

- **Content-discrimination** — constant-plane → `None` (single-symbol
  histogram, entropy 0); horizontal-stripes (`row = 7r mod 256`) →
  `Left` or `Gradient` (both collapse to near-zero entropy under the
  documented tie-break); 2D linear ramp (`r + c mod 256`) →
  `Gradient` / `Median` / `Left` (the dense gradient predictor is
  exact on row-plus-column patterns); xorshift32 noise → `None`
  or `Left` (Gradient / Median both spread the histogram on
  uncorrelated content).
- **Determinism** — 20 repeated calls on identical input return the
  same predictor (no float-hash / iteration-order regression); rows
  past `HEURISTIC_SAMPLE_ROWS` cannot change the heuristic's choice
  (sampling budget is fixed).
- **Degenerate-input guard** — `width = 0` / `height = 0` returns the
  documented `Gradient` fallback; `width = 1` / `height = 1` /
  `height < HEURISTIC_SAMPLE_ROWS` don't panic and return one of
  the four documented predictors.
- **Trait-path round-trip with heuristic** — every FOURCC × content
  pattern survives `encode_frame_via_trait → decode_frame_via_trait`
  bit-exact (the heuristic-chosen predictor must be a valid
  decoder input — `Predictor::Gradient`'s round-1 hand-crafted
  decoder support is exercised here transparently via the trait).
- **Non-regression on the entropy floor** — for every test fixture
  the heuristic's choice produces full-frame entropy within
  `0.5 bit/sample` of the four-way minimum (slack covers the
  sampled-row vs. full-frame entropy gap), AND strictly beats
  `Predictor::None` on Gradient-friendly content (regression guard
  against the heuristic collapsing to "always None" or "always pick
  the first candidate without computing entropy").

The 3 new registry-module unit tests exercise `set_predictor` directly
(no `Any`-downcasting on `Box<dyn Encoder>` today) by constructing a
`UtVideoEncoder` through `build_encoder_config`, then asserting the
encoded packet's trailing `frame_info` dword carries the correct
predictor bits (8..9, `spec/02` §6.1) after pinning to
`Predictor::Left`.

**258 tests** (was 238, +20). Headline estimate unchanged at
**decode ~97% / encode ~97%** — round 18 closes the trait-path
predictor-policy gap on the existing encode surface (every frame now
gets a content-appropriate predictor instead of a one-size-fits-all
Gradient), not new bitstream capability. ULH*/HBD/Lite/interlaced
remain blocked on out-of-corpus docs.

**Round 17 — `Encoder` trait wiring from `CodecParameters` +
end-to-end integration suite.** Round 14 closed the analogous gap on
the decoder side: the registry `make_decoder` factory now derives the
`StreamConfig` at construction time, so trait-driven decode works
without callers having to downcast and call a private `configure()`
hook. The encoder path stayed direct-API-only — the
`oxideav_core::Encoder` trait was not implemented, capabilities did
not advertise `with_encode()`, and no factory was registered. Round 17
mirrors round 14 on the encode side. After this round
`CodecCapabilities::with_encode()` is set, `CodecInfo::encoder` points
at `registry::make_encoder`, and `oxideav_core::CodecRegistry::has_encoder`
returns `true` for `"utvideo"`. A populated `CodecParameters` (FourCC
in `tag` OR a YUV `pixel_format` + `width` / `height` + extradata)
constructs an `Encoder` that takes `VideoFrame` through `send_frame` /
`receive_packet` and produces wire-format payload bytes the decoder
trait accepts. Missing extradata is synthesised via
`Extradata::ffmpeg_for(fc, 1)`; a populated 16-byte block (`spec/01`
§4) is preserved verbatim. Stride-padded plane buffers are repacked
tight before encode so the producer's SIMD alignment leaks transparently
to the wire. All emitted packets carry `flags.keyframe = true`
(`spec/02` §1 — Ut Video is intra-only, stateless across frames).

New `tests/round17_encoder_trait_integration.rs` (26 tests) pins six
groups of invariants:

- **Factory happy path** — every FourCC (ULRG/ULRA/ULY0/ULY2/ULY4)
  constructs via `params.tag`; YUV trio also constructs via
  `pixel_format` (Yuv420P→ULY0, Yuv422P→ULY2, Yuv444P→ULY4);
  `output_params` reflects the resolved identification surface;
  populated caller extradata round-trips through to `output_params`
  (slice-count = 4 preserved).
- **Trait-path byte-equality** — `send_frame` + `receive_packet`
  produces the same bytes a direct `encode_frame(EncodedFrame)` call
  would for every FourCC at single-slice 16×16 and multi-slice 32×32
  (latter crosses the round-5 parallel-encode auto-dispatch).
- **State-machine contract** — `NeedMore` before `send_frame`, `Eof`
  after `flush`, double-`send_frame` rejection, `NeedMore` after
  draining `receive_packet`, `flags.keyframe = true` on every emitted
  packet, audio-frame rejection, PTS pass-through path.
- **Factory construction-time rejection** — missing tag AND
  pixel_format, missing dims, packed-RGB (`Rgb24` / `Rgba`) and
  `Gray8` pixel formats (the planar GBR(A) wire layout cannot be
  silently derived — `spec/04` §6 + `spec/02` §3.1), ULY0 odd-width
  AND odd-height, ULY2 odd-width, truncated extradata all surface
  `Error::Invalid` at `make_encoder` time.
- **Plane-count + stride validation** — `send_frame` rejects 3-plane
  ULRA / short plane buffers / stride below plane width;
  stride-padded buffers are repacked tight and produce bytes
  byte-identical to a direct tight-input encode.
- **End-to-end round-trip via the traits** — encode through
  `Encoder::send_frame` / `receive_packet`, decode through round-14
  `Decoder::send_packet` / `receive_frame`, and assert sample-equal
  per-plane output for every FourCC including the 32×32 ULY4
  4-slice parallel-encode path.

**238 tests** (was 212, +26). Headline estimate moves to
**decode ~97% / encode ~97%** — round 17 closes the framework
integration gap on the existing encode surface (the encoder bit on
the workspace README capability column now reflects what the codec
crate already shipped on the direct API), not new bitstream
capability. ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus
docs.

**Round 16 — row-strided None + Left predictor refactor.** Round 15
hoisted the row-0 / column-0 branches out of the Gradient and Median
inner loops. The None and Left paths still iterated with per-pixel
`plane[r * width + c]` index arithmetic; the round-15 prose explicitly
called them "already tight cumulative loops" and left them alone.
Round 16 converts them to row-strided `chunks_exact_mut(width)`
iteration so the inner row sees a fixed `width` slice — the compiler
can elide the per-pixel bounds check, and `apply_none` lowers to a
straight `copy_from_slice` (memcpy intrinsic) over the slice-strip's
rows. Mirror change on the encoder side (`forward_slice` for the
`Predictor::None` and `Predictor::Left` arms). The output is
bit-for-bit identical (all 195 prior tests still pass byte-equal); the
round is depth-mode code structure / bounds-check elision, not new
bitstream capability.

New `tests/round16_predictor_row_stride.rs` (17 tests) pins the
byte-equality invariants the row-strided refactor must keep:

- **`apply_none` is a pure row-strided copy** across the slice's row
  range (`spec/04` §3 — identity predictor): round-trip every FOURCC
  at single-slice / multi-slice / uneven-slice (`ph % N != 0`) /
  zero-row-slice (`N > ph`) regimes.
- **`apply_left` is the continuous-wrap Left predictor**: column 0
  of row r reads `sample[r-1, W-1]` inside the slice (`spec/04` §4 +
  §4.1.1 — per-slice +128 seed at the very first pixel only).
  Constant-zero and constant-V plane decode signatures pinned (the
  row-strided refactor MUST carry `prev` across the `chunks_exact_mut`
  row boundary or the cumulative sum corrupts); row-constant plane
  (each row r filled with `7r mod 256`) explicitly exercises the
  row-to-row state-carry seam.
- **Encode/decode byte-equality at the auto-dispatch threshold**:
  320×240 ULY4 / ULY2 with 8 slices crosses
  `PARALLEL_PIXEL_THRESHOLD` and exercises the parallel encode +
  parallel decode paths under None and Left; the round-strided
  refactor must produce the same bytes the serial path would.
- **Determinism**: two encodes of the same input produce identical
  bytes under None and Left (no iteration-order regression).
- **Minimal-width edge case**: `width = 1` reduces every row to a
  single pixel; the `chunks_exact_mut(1)` iterator must not skip rows
  or panic.
- **Cross-predictor parity**: the same input plane round-trips
  bit-exact under all four predictors at single-slice — restates the
  round-2 pattern-matrix invariant for the None/Left subset after the
  refactor.

**212 tests** (was 195, +17). Headline estimate unchanged at
**decode ~97% / encode ~96%**. ULH*/HBD/Lite/interlaced remain blocked
on out-of-corpus docs.

**Round 15 — profile-driven Gradient + Median predictor refactor.**
Decoder `apply_gradient` / `apply_median` had four per-pixel branches
(row-0, column-0, etc.) checked inside the inner loop; round 15 hoists
those special-cases out so the dense interior runs branch-free as a
tight cumulative add over `row[c-1]` + the row-above delta. Mirror fix
on the encoder side (`forward_gradient` / `forward_median`). Same
bit-for-bit output (every one of the 195 tests still passes), but on
the criterion baseline (`benches/decode.rs` + `benches/encode.rs`):

| Bench                                 | Round 11  | Round 15  | Δ        |
| ------------------------------------- | --------- | --------- | -------- |
| `decode_ulrg_1080p_single` (Grad)     | 41.5 ms   | 32.6 ms   | **-24%** |
| `decode_uly2_1080p_single` (Grad)     | 27.3 ms   | 21.3 ms   | **-22%** |
| `decode_parallel_scaling/serial/1`    | 17.9 ms   | 14.3 ms   | **-20%** |
| `decode_parallel_scaling/parallel/8`  |  2.7 ms   |  2.26 ms  | **-16%** |
| `encode_ulrg_1080p_single` (Grad)     | 38.8 ms   | 30.2 ms   | **-22%** |
| `encode_uly2_1080p_single` (Grad)     | 23.9 ms   | 19.5 ms   | **-18%** |
| `encode_parallel_scaling/serial/1`    | 16.1 ms   | 13.1 ms   | **-19%** |

Decoder serial throughput rises from ~143 MiB/s to ~185 MiB/s on a
1080p Gradient frame; the parallel/8 path crosses 1 GiB/s (974 → 1140
MiB/s). Slice-parallel speedup at 1280×720 ULY4 stays high at 6.2×.
`apply_left` / `apply_none` already ran as tight cumulative loops and
are unchanged. Headline estimate unchanged at **decode ~97% / encode
~96%** — round 15 is depth-mode performance, not new bitstream
capability.

**Round 14 — `Decoder` trait wiring from `CodecParameters` + end-to-end
integration suite.** The registry [`make_decoder`] factory in
`src/registry.rs` previously ignored `params.tag` / `params.extradata` /
`params.width` / `params.height` and left the internal [`StreamConfig`]
as `None`, relying on a private `configure()` hook that callers driving
the codec through the `oxideav_core::Decoder` trait could not reach.
The wiring now mirrors the `oxideav-huffyuv` pattern: at factory time we
derive the FourCC from `CodecParameters.tag` (`CodecTag::Fourcc`), parse
`params.extradata` via [`Extradata::parse`], and validate dims via
[`StreamConfig::new`]. Malformed extradata or chroma-constraint
violations surface as `Error::InvalidData` at construction time so the
container learns "this stream cannot decode" before any packet is
dispatched. Missing pieces (no tag, no dims, empty extradata) leave
`cfg` as `None` so the `configure()` hook still works for legacy
callers. Net effect: any container that hands us a populated
`CodecParameters` (which `oxideav-avi` does today) now gets a working
trait-driven decoder without downcasting.

New `tests/round14_decoder_trait_integration.rs` (21 tests) pins five
groups of invariants: factory happy path on every FourCC (plane count,
stride, per-plane payload length match `spec/02` §3); trait-path
byte-equality against a direct `decode_frame` call (no transform);
state-machine contract (`NeedMore` before `send_packet`, `Eof` after
`flush`, double-`send_packet` rejection, PTS pass-through); factory
construction-time rejection of truncated / Huffman-clear / interlaced /
wrong-`frame_info_size` extradata + ULY0 / ULY2 odd-width dim
violations; deferred-config path when extradata / tag / dims are
missing. Plus capability-flag preservation (`utvideo_sw` / `lossless` /
`intra_only` / `decode`) and `ProbeContext` resolution cross-check.
**195 tests** (was 174, +21). Headline estimate unchanged at
**decode ~97% / encode ~96%** — round 14 closes the framework
integration gap on the existing decode surface, not new bitstream
capability. ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus
docs.

**Round 13 — `ErrorCategory` classifier + exhaustive `Display`
regression suite + `InvalidSliceCount` message accuracy fix.** The
crate's error surface (18 [`Error`] variants) has shipped without a
structured way for callers to react to a failure — they either
pattern-match every variant (brittle: a new variant added in a future
round silently falls through at the call site) or rely on the
informal "log the `Display` text" pattern. Round 13 adds an
[`ErrorCategory`] enum with four buckets:

- **`MalformedStream`** — per-frame wire bytes don't match spec/02 +
  spec/05 (`ChunkTooShort`, `NonMonotonicSliceOffsets`,
  `SliceNotWordAligned`, `KraftViolation`,
  `MultipleSingleSymbolSentinels`, `HuffmanDecodeFailure`,
  `SliceTruncated`, `MissingFrameInfo` — 8 variants). A muxer-level
  caller MAY skip the offending packet and resync at the next
  keyframe.
- **`ApiMisuse`** — caller violated the typed contract
  (`InvalidSliceCount`, `EncoderPlaneSizeMismatch`, `InvalidInput`
  — 3 variants). The call cannot succeed without caller-side fixes.
- **`Unsupported`** — wire data structurally valid on a code path
  this build doesn't implement (`HuffmanBitClear`,
  `InterlacedNotSupported`, `UnsupportedPrediction` — 3 variants).
  Bounded out-of-corpus paths per `audit/00-report.md` §5.2.
- **`StreamShape`** — stream-level identification metadata
  malformed (`UnknownFourcc`, `ExtradataTruncated`,
  `InvalidFrameInfoSize`, `DimensionConstraint` — 4 variants). A
  demuxer should reject the stream rather than retry per-frame.

`Error::category()` returns the bucket; convenience predicates
`is_malformed_stream` / `is_api_misuse` / `is_unsupported` /
`is_stream_shape` cover the four-way switch directly. The classifier
`match` in `error.rs` has no `_ =>` fallback by design, so adding a
new `Error` variant requires extending the mapping in the same commit.
`ErrorCategory` is `#[non_exhaustive]` so introducing a fifth category
in a future round is a non-breaking change. Plus an in-line fix to
the `InvalidSliceCount` Display message: it read `"num_slices == 0"`
but the variant is also produced for `> 256` — the new message names
the full valid range `1..=256` (`spec/01` §4.4.3).

New `tests/round13_error_taxonomy.rs` (22 tests) pins five invariants
exhaustively across every variant: (1) every variant lives in exactly
one category (partition correctness); (2) `Display` carries the
`"oxideav-utvideo:"` crate-name prefix and is non-empty (so a future
variant without the prefix trips here); (3) Display reports the
variant's payload fields (FourCC hex, byte counts, bit positions,
plane indices); (4) the `InvalidSliceCount` message names the full
`1..=256` range, not the stale `== 0`; (5) `std::error::Error::source`
returns `None` (the crate has no inner-wrapped errors). **174 tests**
(was 152, +22). Headline estimate unchanged at **decode ~97% /
encode ~96%**; this round is depth-mode public-API ergonomics, not
new capability. ULH*/HBD/Lite/interlaced remain blocked on
out-of-corpus docs.

**Round 12 — second cargo-fuzz target: encode-then-decode roundtrip.**
The decoder fuzz harness from round 10 covers the attacker-facing surface
(arbitrary bytes through `decode_frame`). The encoder is a different
shape of risk — its input is a typed `EncodedFrame` (FourCC + dims +
predictor + slice count + per-plane samples) but a caller that mis-sizes
a plane buffer or picks a slice count larger than any row is a real
integration bug to handle without panicking, and on top of that the
encoder's own decoder MUST round-trip its bytes bit-exactly or the
self-roundtrip invariant the round-1 tests pin on hand-picked fixtures
silently regresses on some other shape. This round adds a second
target, **`encode_utvideo_frame`**, that drives `(fourcc × dims ≤ 32×32
× predictor × num_slices × pixels)` through `encode_frame` → `decode_frame`
and asserts every plane survives the roundtrip bit-exactly. A
**stable-CI mirror** at `tests/fuzz_seed_corpus_encode.rs` (11 tests,
mirroring the r160 h261 RTCP-fuzz pattern verbatim) runs the same
driver logic against the committed seed corpus + a handful of inline
adversarial buffers (empty input, 5-byte-only header, all-ones, every
FourCC × Left, every predictor × ULY2, slice-count > height, 32×32 ULY4
upper bound, ULRA 4-plane alpha) so a regressed encoder or an
encoder/decoder skew trips the regular CI matrix instead of waiting for
the next daily fuzz run to notice. 8 committed seeds under
`fuzz/corpus/encode_utvideo_frame/` cover the 5 FourCCs × 4 predictors
× single/multi-slice cross-product at small dims. Headline estimate
unchanged at **decode ~97% / encode ~96%**; this round is depth-mode
robustness coverage, not new capability.

**Round 11 — criterion benchmarks for decode + encode + Huffman LUT +
RGB decorrelate.** The crate is decoder/encoder feature-complete on the
classic-family wire and saturated against the spec corpus (decode ~97% /
encode ~96% — round 10 added a daily 30-minute decode fuzz harness and
round 9 the descriptor / API-misuse rejection sweep). This round adds
criterion benchmarks so future optimisation work has a baseline:

- `benches/decode.rs` — full-frame ULRG and ULY2 decode at 1920×1080
  single-slice, plus a `bench_with_input` slice-parallel scaling table
  at 1280×720 ULY4 with `N ∈ {1, 2, 4, 8}` covering both
  `decode_frame_serial` and `decode_frame_parallel`.
- `benches/encode.rs` — symmetric coverage on `encode_frame` (ULRG /
  ULY2 1080p single-slice + 720p ULY4 slice-parallel scaling). The
  encoder's Amdahl-bounded ceiling (per-plane Huffman length build is
  single-threaded by construction) shows in the scaling curve.
- `benches/huffman_lut.rs` — `HuffmanTable::decode_slice` microbench
  isolating the round-3 12-bit-prefix LUT kernel. Two regimes:
  `max_len = 12` (pure LUT fast-path) and `max_len = 14` (top two
  tiers fall through to the slow-path length-tier scan).
- `benches/rgb_decorrelate.rs` — microbench for
  `predict::{forward,inverse}_decorrelate_rgb` (`spec/04` §6) across
  `n_samples ∈ {64K, 256K, 1M, 1920×1080}`.

Measured wall-clock on this 8-core host (release profile; criterion
median of 10 samples):

| Bench                                       | Time      | Throughput     |
| ------------------------------------------- | --------- | -------------- |
| `decode_ulrg_1080p_single` (Gradient)       | 40.56 ms  | 146 MiB/s      |
| `decode_uly2_1080p_single` (Gradient)       | 26.65 ms  | 148 MiB/s      |
| `decode_parallel_scaling/serial/1`          | 17.78 ms  | 148 MiB/s      |
| `decode_parallel_scaling/parallel/8`        |  2.67 ms  | 987 MiB/s      |
| `encode_ulrg_1080p_single` (Gradient)       | 37.00 ms  | 160 MiB/s      |
| `encode_uly2_1080p_single` (Gradient)       | 24.07 ms  | 164 MiB/s      |
| `encode_parallel_scaling/serial/1`          | 15.98 ms  | 165 MiB/s      |
| `encode_parallel_scaling/parallel/8`        |  ~3 ms    | ~875 MiB/s     |
| `huffman_lut_pure_max12/262144`             |  1.02 ms  | 257 Melem/s    |
| `huffman_lut_fallback_max14/262144`         |  1.32 ms  | 199 Melem/s    |
| `rgb_inverse_decorrelate/2073600`           | 73.8 µs   | 26.2 GiB/s     |
| `rgb_forward_decorrelate/2073600`           | 76.7 µs   | 25.2 GiB/s     |

The parallel-decode speedup at `N = 8` is ~6.7× over the same-frame
serial baseline (was ~5.6× in the round-4 hand-timed perf-smoke), and
the LUT fast-path adds ~22% over the fallback search at the largest
input. All inputs are synthesised on-the-fly from a deterministic
xorshift32 — no committed binary fixtures. Headline estimate unchanged
at **decode ~97% / encode ~96%**; this round is depth-mode benchmark
coverage, not new capability.

**Round 10 — cargo-fuzz decode harness.** The encoder is feature-complete
for all five FourCCs × four predictors (None/Left/Gradient/Median) with
RGB inter-plane decorrelation, multi-slice, and a slice-parallel path —
the self-roundtrip suite already pins `decode ∘ encode == identity` across
the entire 5×4 matrix, so this round adds a continuous-fuzzing harness on
the decoder (the attacker-facing surface) instead of new capability. New
`fuzz/` cargo-fuzz crate with a `decode_utvideo` target: it synthesises a
small `StreamConfig` (FourCC + ≤64×64 even dims + 1..=16 slices) from a
4-byte header prefix of the input and feeds the remainder to
`decode_frame`, asserting the call always *returns* a `Result` —
never panics / aborts / OOMs — for arbitrary chunk-payload bytes.
Dimensions are capped so the budget lands on genuine parser defects
(descriptor / offset-table index math, slice-range arithmetic, the
Huffman bit reader) rather than format-legitimate large allocations.
Local run: **21.8M executions in 61 s, 0 crashes, RSS flat at ~419 MB**,
458 edges covered. A daily scheduled `Fuzz` workflow gives the target the
full 30-minute budget. Headline estimate unchanged at **decode ~97% /
encode ~96%**. ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus
docs.

**Round 9 — descriptor-mutation rejection + encoder API misuse +
bit-pack/unpack invariants.** New `tests/round9_descriptor_and_api_robustness.rs`
extends Round 8's negative-test surface in three directions left
untested. (1) **Plane-0 256-byte Huffman descriptor mutations**: Round 8
covered slice-data byte-flips but deliberately left the descriptor span
alone (different guard family — `huffman::HuffmanTable::build` raises
`KraftViolation` and `MultipleSingleSymbolSentinels` rather than
`SliceTruncated` / `HuffmanDecodeFailure`). The new suite pins the
integration path: a real encoded frame whose plane-0 descriptor is
mutated trips `MultipleSingleSymbolSentinels` (two zero-codelen
sentinels), `KraftViolation` on incomplete (Σ < 1), excess (Σ > 1), and
uniform-codelen-1 (Σ = 128) descriptors; plus a full single-byte-flip
sweep over the 256-byte descriptor span asserts the no-panic /
no-spurious-variant contract. (2) **Encoder API rejection**:
`encode_frame` surfaces `EncoderPlaneSizeMismatch` (wrong plane count
for ULRA, wrong per-plane buffer length on ULY0), `InvalidSliceCount`
(`num_slices == 0` and `> 256`), and `DimensionConstraint` (odd ULY0
width) — all integration-tested for the first time. (3) **Public-API
boundary checks**: `Extradata::ffmpeg_for` rejects 0 and 257 slices
with `InvalidSliceCount` and accepts 256 (the maximum, `flags` high
byte = `0xff`); `StreamConfig::new` rejects zero width / height. Plus
**`BitWriter` ⇄ `BitReader` round-trip invariant** sweep in isolation
(without going through `HuffmanTable`): every code length `L ∈ 1..=32`
× 200 codes round-trips exactly, with byte-aligned padding to 32-bit
words (`spec/05` §4.1); mixed-length code sequences cover every
bit-offset transition within a 32-bit word; `peek_bits` straddling a
word boundary returns the expected MSB-first concatenation. **141
tests** (was 118, +23). Headline estimate unchanged at **decode ~97% /
encode ~96%** — round 9 hardens the existing decode + encode surface
(rejection paths + bit-pack/unpack invariants) rather than extending
capability. ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus
docs.

**Round 8 — malformed-payload decode robustness (negative tests).**
New `tests/round8_malformed_decode.rs` pins the decoder's defensive
surface: every prior round exercises only the *happy* path
(`decode ∘ encode == identity`), so the `Err(...)` arms in
`decoder::parse_payload` + `huffman::decode_slice` had only one smoke
test (`round4` truncates 8 bytes and asserts `is_err()`) and **none
pinned the specific `Error` variant**. The new suite starts from a
valid encoder output and surgically mutates the wire bytes to trip
exactly one decoder guard, asserting the precise variant for each
malformed-payload condition the spec names: `MissingFrameInfo`
(payload `< 4` bytes, `spec/02` §6); `ChunkTooShort` at the descriptor,
offset-table, and slice-data spans plus a trailing-junk case
(`spec/02` §7); `NonMonotonicSliceOffsets` (`spec/02` §5);
`SliceNotWordAligned` (`spec/05` §4.1 — bump a slice-end-offset by 1);
and `SliceTruncated`/`HuffmanDecodeFailure` from zeroed entropy bits
(all-zero stream → longest-code-per-pixel exhausts the bit budget).
A full single-byte-flip sweep over a real slice-data span asserts the
**no-panic / no-spurious-variant contract** (a corrupt bit either
resyncs to a structurally complete frame or is rejected as one of the
two slice-data variants — never a panic, never an out-of-family
error), and a positive control re-decodes the unmutated base fixtures.
**118 tests** (was 107, +11). This is the negative half of the decode
contract — a corrupt `00dc` chunk is rejected with a diagnosable
error, never silently mis-decoded. Headline estimate unchanged at
**decode ~97% / encode ~96%**; round 8 hardens the existing decode
surface rather than extending capability. ULH*/HBD/Lite/interlaced
remain blocked on out-of-corpus docs.

**Round 7 — encoder byte-stability (idempotency) + full slice-count
boundary sweep.** New `tests/round7_idempotency.rs` adds the *byte*-level
encoder invariants no prior round asserted (earlier suites check only
the *pixel* round-trip `decode ∘ encode == identity`): (1) `encode_frame`
is **deterministic and path-invariant** — two calls, and the serial /
parallel / auto-dispatch entry points, all emit byte-identical payloads,
pinning the Huffman tie-break (`spec/05` §2.2) and re-stating round-5
parallel-encode correctness as a byte equality; (2) `encode ∘ decode ∘
encode` is a **byte-stable transcode fixed point** (5 FOURCCs ×
4 predictors × 3 entropy regimes × 2 slice counts at a non-divisible
96×70), strictly stronger than pixel round-trip. Plus a **full
`num_slices ∈ 1..=256` sweep** at heights chosen so `ph % N != 0` for
most `N` and `N > ph` for the tail — exercising uneven-row and zero-row
slices (zero slice-data bytes per `spec/02` §5.1) across all five FOURCCs
and four predictors, with an edge test at the `ph*(s+1)/N`
integer-division transition. **107 tests** (was 100, +7). Headline
estimate unchanged at **decode ~97% / encode ~96%** — round 7 hardens
the existing encode/decode surface rather than extending capability;
ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus docs.

**Round 6 — FFmpeg-pinned extradata builder + content-fixture corpus.**
New [`Extradata::ffmpeg_for(fourcc, num_slices)`] builder produces the
16-byte extradata block FFmpeg 7.1.2's `utvideo` encoder writes — all
five FOURCCs, all 1..256 slice counts, byte-identical to `spec/01` §5
test-set `T1`. Closes audit/00-report.md §5.2 implementer-resolvable
open items 1 (encoder-version semantics: mirror `0x0100_00f0`) and 2
(RGB source-format tag: mirror `00 00 01 18` / `00 00 02 18`). New
[`Fourcc::ffmpeg_source_format_tag`] accessor exposes the per-FOURCC
4-byte tag. Round-6 also adds a deterministic 336-cell content-fixture
corpus exercising eight content-style synthetic patterns (solid /
horizontal-gradient / diagonal-gradient / vertical-stripes /
horizontal-stripes / 8×8 checker / LCG noise / sparse impulses) ×
4 predictors × 5 FOURCCs at 128×96 + a 16-cell 256×192 8-slice smoke
pass, with **compressed-size bounds**: universal `8 bits/sample`
ceiling on every cell, exact `3*(256 + 4*num_slices) + 4 = 784` byte
equality on the Solid pattern (single-symbol Huffman per plane), and
ordering invariants (`Solid << GradientDiag/Gradient << Noise/None`).
**100 tests = 61 unit + 16 round-2 matrix + 6 round-3 LUT + 6 round-4
parallel-decode + 7 round-5 parallel-encode + 4 round-6 content
fixtures**, up from 87 in round 5 (+13 tests).
Workspace-README headline estimate: **decode ~97% / encode ~96%**
(was decode 95% / encode 94%) — the +2/+2 reflects the FFmpeg
extradata-level interop closure and the broader corpus.

**Round 5 — slice-parallel encode.** `encode_frame` now auto-dispatches
multi-slice frames whose pixel count crosses
`encoder::PARALLEL_PIXEL_THRESHOLD` (64 Ki px ≈ 320×200) onto a
`std::thread::scope` pool, mirroring the round-4 decoder fan-out.
Within each plane both stages that are slice-independent per the
spec — forward predict (per-slice `+128` seed, `spec/04` §§3.1, 4, 5,
7) and per-slice Huffman bit-pack (self-contained per-slice
bit-stream, `spec/02` §5) — fan out across worker threads; the
per-plane Huffman code-length build sits between them on the parent
thread (it aggregates a cross-slice histogram). Output bytes match
the serial path exactly on the 288-cell ULY0 matrix + RGB family +
256-slice stress + roundtrip suite. Measured 320×240 → 1280×720 ULY4
8-slice encode (gradient): serial 1.94 → 9.29 ms, parallel 1.72 →
2.84 ms, **1.13× → 3.28× speedup** on an 8-core host. The encoder's
speedup ceiling is lower than the decoder's because the per-plane
Huffman length build (histogram + package-merge) is single-threaded
by construction — the parallel slices share one codebook per plane.
Explicit `encode_frame_serial` / `encode_frame_parallel` entry
points are kept for latency-sensitive callers or threadpool-driven
flows. 87 tests = 52 unit + 16 round-2 matrix + 6 round-3 LUT + 6
round-4 parallel-decode + 7 round-5 parallel-encode.

**Round 4 — slice-parallel decode.** `decode_frame` auto-dispatches
multi-slice frames whose pixel count crosses
`PARALLEL_PIXEL_THRESHOLD` (64 Ki px ≈ 320×200) onto a
`std::thread::scope` pool sized at
`min(num_slices, available_parallelism())`. Slice-level parallelism
is what `spec/02` §7 names explicitly: each slice carries its own
self-contained Huffman bit-stream (`spec/02` §5) and its predictor
state restarts at the per-slice `+128` seed (`spec/04` §§3.1, 4, 5,
7), so the slices fan out without inter-slice synchronisation.
Measured 320×240 → 1280×720 ULY4 8-slice decode (gradient): serial
1.44 → 8.95 ms, parallel 0.50 → 1.59 ms, **2.87× → 5.63× speedup**
on an 8-core host. Explicit `decode_frame_serial` /
`decode_frame_parallel` entry points are kept for latency-sensitive
or threadpool-controlled callers.

**Round 3 — LUT-accelerated Huffman decode.** Decoder caches a
12-bit prefix LUT per plane (`2^12 = 4096` entries × 4 B) and
resolves the common-case Huffman code in one shift+load; codes
longer than 12 bits (max observed in the spec corpus is 16) fall
back to the existing length-tier prefix scan. `BitReader::peek_bits`
also rewritten to combine adjacent 32-bit LE words into a 64-bit
register, dropping the prior `O(n)` bit-by-bit byte read.

**Round 1 + 2 — clean-room rebuild.** Implements the five 8-bit
classic-family FourCCs (`ULRG` / `ULRA` / `ULY0` / `ULY2` / `ULY4`)
documented in
[`docs/video/utvideo/spec/`](https://github.com/OxideAV/docs/tree/master/video/utvideo/spec).
The previous implementation was retired by the OxideAV docs audit
dated 2026-05-06; the prior history is preserved on the `old`
branch (forbidden input for this rebuild).

This rebuild is methodologically Variant B (FFmpeg-as-oracle): the
spec set is built from the multimedia.cx wiki snapshot at
`docs/video/utvideo/reference/wiki/Ut_Video.wiki` plus black-box
behavioural observation of a system FFmpeg 7.1.2 binary. **No
FFmpeg / Win32 / VLC source is read at any phase.**

## Scope (round 1)

- All five 8-bit FourCCs: `ULRG`, `ULRA`, `ULY0`, `ULY2`, `ULY4`.
- All four predictors: none / left / gradient / median, with the
  per-slice +128 first-pixel seed convention pinned in `spec/04`.
- Per-plane canonical Huffman (RFC 1951 mirrored, per `spec/05` §2.2)
  + 32-bit-LE-word, MSB-first slice bit packing (`spec/05` §4).
- RGB inter-plane decorrelation (`spec/04` §6) for ULRG / ULRA.

## Out of scope

- AVI / VfW carriage (`fccHandler`, `BITMAPINFOHEADER`, `00dc`
  chunk wrapping, `idx1` index, OpenDML reservation). That belongs
  in `oxideav-avi`. Callers hand us `StreamConfig` + chunk-payload
  bytes.
- Interlaced bit (`flags & 0x00000800`); deferred per `spec/01`
  §4.4.2 (no behavioural fixture exercises it).
- High-bit-depth FourCCs (`ULH0`, `ULH2`, 10-bit ULY4) — wiki
  mentions but FFmpeg encoder does not produce.
- Raw / non-Huffman slice mode (`flags & 0x00000001 == 0`); not
  observed in the corpus.

## Public API

- [`decode_frame`] — decode one `00dc` chunk payload into per-plane
  samples (`DecodedFrame`).
- [`encode_frame`] — encode per-plane samples into one chunk
  payload.
- [`Fourcc`] / [`Extradata`] / [`StreamConfig`] / [`Predictor`] —
  identification surface.
- [`register_codecs`] / [`register`] — wire into `oxideav-core`'s
  codec registry under codec id `"utvideo"`.

## Cargo features

- **`registry`** (default): wire the crate into `oxideav-core`'s
  codec registry.
