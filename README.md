# oxideav-utvideo

Pure-Rust Ut Video lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

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
