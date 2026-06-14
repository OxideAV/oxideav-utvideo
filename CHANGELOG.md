# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 301 — fuzz depth: cross-accessor invariant pinning for the
  round-244..291 typed inspector accessors.** The `inspect_utvideo`
  cargo-fuzz target (round 228) predates every typed, decode-free
  `PlaneLayout` / `FrameLayout` accessor added across rounds 244 / 250 /
  255 / 261 / 275 / 291, so none of their documented cross-accessor
  invariants were exercised on attacker-shaped chunk bytes. Two new
  properties now run in both the libFuzzer target and its stable-CI
  mirror (`tests/round228_inspect_fuzz_properties.rs`): **Property 4**
  asserts, on every successful `peek_frame`, the descriptor-byte
  conservation law `active + unused + single == 256` (`spec/05` §2.1),
  the strictly-ascending no-zero-tier `code_length_histogram` and its
  scalar projections (`min`/`max`/`min_count`, Σ count ==
  `active_symbol_count`), the single-symbol path (`spec/05` §6.1)
  forcing all length counters to 0, the `kraft_numerator` /
  `is_kraft_complete` consistency (`== 2^max` iff complete, `spec/05`
  §2.2 step 3), and the geometry identities `total_pixels ==
  width*height`, word-aligned `slice_data_total`, `total_size == 256 +
  4*num_slices + slice_data_total` (`spec/02` §5), rolled up to the
  frame-level `total_size` / `total_slice_data_bytes` /
  `all_planes_kraft_complete` identities. **Property 5** pins that a
  successful `decode_frame` implies `all_planes_kraft_complete()` —
  `HuffmanTable::build` rejects any incomplete descriptor (`spec/05`
  §2.2) and the single-symbol path is complete by definition (`spec/05`
  §6.1). No `src/` change; pure test/fuzz hardening of already-shipped
  accessors.

- **Round 291 — decode-free `is_kraft_complete` predicate on
  `inspect::PlaneLayout` + `all_planes_kraft_complete` frame roll-up on
  `inspect::FrameLayout`.** Folds the round-275 integer `kraft_numerator`
  (`spec/05` §2.2 step 3) and the round-21 `is_single_symbol` flag into
  the completeness `bool` a caller wants: "does this plane's 256-byte
  descriptor form a complete prefix code — i.e. would `HuffmanTable::build`
  accept it?" Three shapes the wire format admits map to the predicate:
  the `spec/05` §6.1 single-symbol path (empty histogram, flag-recognised)
  → `true`; an active canonical codebook → `true` iff
  `kraft_numerator() == 2^max_code_length` (Kraft equality, `spec/05`
  §2.2 step 3); an empty / all-`255`-unused descriptor (no active byte,
  not single-symbol) → `false`. The value over `kraft_numerator()` alone
  is that `peek_frame` is a pure byte-walk that does **not** reject a
  Kraft-incomplete descriptor (unlike `decode_frame`, which trips
  `Error::KraftViolation` at `HuffmanTable::build` time), so the predicate
  is the decode-free "is this frame decode-ready?" oracle the inspector
  path otherwise lacked. `FrameLayout::all_planes_kraft_complete()` rolls
  the per-plane predicate up over a frame. Both are additive methods —
  no field or signature change. Pinned by 8 dedicated integration tests
  (`tests/round291_kraft_complete.rs`) covering the all-FOURCC ×
  all-predictor encoder-complete sweep, the single-symbol /
  single-length / all-unused / Kraft-incomplete / Kraft-excess shapes,
  the predicate-iff-`decode_frame`-succeeds equivalence, and the
  `kraft_numerator` identity agreement — plus 2 in-file unit tests.
  Test count 345 → 355.
- **Round 261 — typed `min_code_length_symbol_count` accessor on
  `inspect::PlaneLayout`.** Extends the decode-free per-frame
  layout with a sixth decode-free typed semantic primitive — the
  multiplicity of the shortest-length tier in this plane's
  canonical Huffman codebook, i.e. the count of `code_length[s]`
  entries equal to `min_code_length` over the active range
  `1..=254` per `spec/05` §2.1. Joins the existing
  `is_single_symbol` flag (round 21), the per-slice row-range /
  pixel-count fields on `SliceLayout` (round 241), the
  `active_symbol_count` field (round 244), the `max_code_length`
  field (round 250), and the `min_code_length` field (round 255).
  Range `0..=256`: the value collapses to `0` on the `spec/05`
  §6.1 single-symbol path (matches the `min_code_length`
  `0`-collapse on the same path); reports `1..=256` for canonical
  Kraft codebooks (`1` when the shortest tier is occupied by
  exactly one symbol, `>= 2` on the `spec/05` §6.2 two-symbol
  `{1, 1}` case and the `spec/05` §6.3 / §6.4 single-length
  descriptors, saturating at `active_symbol_count` when
  `min_code_length == max_code_length`). Populated by `peek_frame`
  in the same descriptor pass that already computes
  `is_single_symbol`, `active_symbol_count`, `max_code_length`,
  and `min_code_length` — the existing 256-byte descriptor
  scan tracks the multiplicity inline (count resets to `1` on a
  strictly-smaller active byte, increments when the running min is
  re-seen), keeping the inspector's `O(plane_count * num_slices)`
  complexity intact and avoiding a sixth `.iter().filter(|b| *b ==
  min).count()` walk. The new field is additive on a `Vec`-carrying
  struct (`PlaneLayout` keeps its `Debug` / `Clone` / `PartialEq` /
  `Eq` derives); existing callers reading the byte-offset / slice /
  single-symbol / active-count / max-length / min-length fields see
  no behavioural change. Six dedicated tests in
  `tests/round261_min_code_length_symbol_count.rs` plus two in-file
  unit tests in `src/inspect.rs` pin the field semantics: the
  single-symbol `0`-collapse, the multi-symbol positive count, the
  descriptor-byte rescan invariant, the typed bound by
  `active_symbol_count`, the Kraft typed upper bound
  `<= 2^min_code_length` per `spec/05` §2.2 step 3, and the
  saturation-at-`active_symbol_count` invariant on the
  single-length-descriptor path (`spec/05` §6.3 / §6.4) driven by
  a `spec/05` §6.2-shaped two-value checkerboard fixture.

- **Round 255 — typed `min_code_length` accessor on
  `inspect::PlaneLayout`.** Extends the decode-free per-frame layout
  with a fifth decode-free typed semantic primitive — the smallest
  value of `code_length[s]` over the active range of this plane's
  256-byte Huffman descriptor (`spec/02` §4 + `spec/05` §2.1:
  entries in `1..=254`). Joins the existing `is_single_symbol` flag
  (round 21), the per-slice row-range / pixel-count fields on
  `SliceLayout` (round 241), the `active_symbol_count` field (round
  244), and the `max_code_length` field (round 250) — together the
  five typed primitives surface the prefix-code's full depth band
  `[min, max]` and Kraft-coupling fingerprint without standing up a
  `HuffmanTable` or allocating a residual buffer. Range `0..=254`:
  the value collapses to `0` on the `spec/05` §6.1 single-symbol
  path (where the only non-`255` descriptor byte is the `0`
  sentinel and no entry sits in the active range), otherwise
  reports the shortest code length in bits. Per `spec/05` §2.2 +
  the wiki algorithm description, the shortest code is the all-ones
  bit pattern at this length. Three sub-shapes the field
  discriminates: `min == 1` (a length-1 code exists, satisfying
  `2^-1` of the Kraft sum); `min == max` (single-length descriptor
  per `spec/05` §6.3 / §6.4, Huffman tree collapses to flat byte
  indexing); `min < max` (general variable-length codebook, the
  algorithm in `spec/05` §2.2 iterates over `[min, max]` assigning
  codes). Populated by `peek_frame` in the same descriptor pass
  that already computes `is_single_symbol`, `active_symbol_count`,
  and `max_code_length` — the existing 256-byte descriptor slice is
  folded over once to yield all four counters simultaneously,
  keeping the inspector's `O(plane_count * num_slices)` complexity
  intact and replacing a fourth `.iter().filter().min()` walk with
  the existing single `match` loop. The new field is additive on
  the `Vec`-carrying struct (`PlaneLayout` keeps its `Debug` /
  `Clone` / `PartialEq` / `Eq` derives); existing callers reading
  only the byte-offset / slice / single-symbol / active-count /
  max-length fields see no behavioural change. Six dedicated tests
  (`tests/round255_min_code_length.rs`) pin (a) the single-symbol
  → zero min case, (b) the high-entropy → `1..=254` range across
  every FOURCC, (c) the decode-free re-scan equivalence against
  the on-wire descriptor byte slice via `descriptor_start`, (d)
  the typed `min <= max` invariant across both branches, (e) the
  Kraft upper-bound coupling against the round-244
  `active_symbol_count` accessor (`min <= floor(log2(K))` on a
  Kraft-satisfying codebook with `K >= 2`), and (f) the wire-format
  invariant that `active_symbol_count >= 1` implies
  `min_code_length >= 1` (the active range starts at `1`, not `0`).
  Headline estimate unchanged at **decode ~97% / encode ~97%** —
  round 255 surfaces existing descriptor-byte semantics through a
  typed accessor, not new bitstream capability.

- **Round 250 — typed `max_code_length` accessor on
  `inspect::PlaneLayout`.** Extends the decode-free per-frame layout
  with a fourth decode-free typed semantic primitive — the largest
  value of `code_length[s]` over the active range of this plane's
  256-byte Huffman descriptor (`spec/02` §4 + `spec/05` §2.1:
  entries in `1..=254`). Joins the existing `is_single_symbol` flag
  (round 21), the per-slice row-range / pixel-count fields on
  `SliceLayout` (round 241), and the `active_symbol_count` field
  (round 244) — together the four typed primitives let a container
  indexer pick a decode strategy (`spec/05` §7.3: flat `2^k`-entry
  table vs. multi-stage table vs. tree walk) without standing up a
  `HuffmanTable` or allocating a residual buffer. Range `0..=254`:
  the value collapses to `0` on the `spec/05` §6.1 single-symbol
  path (where the only non-`255` descriptor byte is the `0`
  sentinel and no entry sits in the active range), otherwise
  reports the longest code length in bits. The wire-format upper
  bound is `254` per `spec/05` §7.2; `spec/05` §7.1 reports `16`
  as the maximum observed across the behavioural corpus.
  Populated by `peek_frame` in the same descriptor pass that
  already computes `is_single_symbol` and `active_symbol_count` —
  the existing 256-byte descriptor slice is folded over once to
  yield all three counters simultaneously, keeping the inspector's
  `O(plane_count * num_slices)` complexity intact and replacing
  three separate `.iter().filter()` walks with a single `match`
  loop. The new field is additive on the `Vec`-carrying struct
  (`PlaneLayout` keeps its `Debug` / `Clone` / `PartialEq` / `Eq`
  derives); existing callers reading only the byte-offset / slice
  / single-symbol / active-count fields see no behavioural change.
  Six dedicated tests
  (`tests/round250_max_code_length.rs`) pin (a) the single-symbol
  → zero max case, (b) the high-entropy → `1..=254` range across
  every FOURCC, (c) the decode-free re-scan equivalence against
  the on-wire descriptor byte slice via `descriptor_start`, (d)
  the `spec/05` §7.1 empirical bound (`max <= 16` on the
  corpus-like fixtures), (e) the `spec/05` §7.3 64 KiB-table-size
  bound (`2^max <= 2^16`), and (f) the typed Kraft lower-bound
  coupling against the round-244 `active_symbol_count` accessor
  (`max >= ceil(log2(active))` on a Kraft-satisfying codebook).
  Headline estimate unchanged at **decode ~97% / encode ~97%** —
  round 250 surfaces existing descriptor-byte semantics through a
  typed accessor, not new bitstream capability. Test count: 321
  (was 313, +6 dedicated round-250 tests + 2 in-file unit tests).

- **Round 244 — typed `active_symbol_count` accessor on
  `inspect::PlaneLayout`.** Extends the decode-free per-frame
  layout with a third decode-free typed semantic field — the count
  of symbols carrying an explicit code length in this plane's
  256-byte Huffman descriptor (`spec/02` §4 + `spec/05` §2.1:
  entries in the active range `1..=254`). Joins the existing
  `is_single_symbol` flag (round 21) and the row-range / pixel-count
  fields on `SliceLayout` (round 241), giving a container indexer
  the third pre-decode diagnostic primitive — "how many active
  symbols does this plane's codebook carry?" — without standing up
  a `HuffmanTable` or allocating a residual buffer. Two
  well-formed shapes the wire format permits per `spec/05`: `0`
  active symbols paired with `is_single_symbol == true` (the lone
  `code_length[s] == 0` entry from `spec/05` §6.1 is NOT itself an
  active code), and `2..=256` active symbols on a Kraft-satisfying
  multi-symbol canonical codebook (`spec/05` §2.2 step 3). Populated
  by `peek_frame` in the same descriptor pass that already computes
  `is_single_symbol` — the existing 256-byte descriptor slice is
  folded over once to yield both flags simultaneously, keeping the
  inspector's `O(plane_count * num_slices)` complexity intact. A
  companion
  `PlaneLayout::unused_symbol_count()` convenience method surfaces
  the `code_length[s] == 255` sentinel count from `spec/05` §2.1
  so the typed identity `active + unused + (single ? 1 : 0) == 256`
  is a one-liner cross-check. The new field is additive on a
  `Vec`-carrying struct (`PlaneLayout` keeps `Debug` / `Clone` /
  `PartialEq` / `Eq` derives); existing callers reading only the
  byte-offset fields see no behavioural change. Six dedicated tests
  (`tests/round244_active_symbol_count.rs`) pin (a) the
  single-symbol → zero-active-count case, (b) high-entropy → `>= 2`
  active across every FOURCC + every multi-slice configuration,
  (c) the partition identity across mixed-entropy + single-symbol
  cases, (d) the typed biconditional `is_single_symbol ⇔
  active_symbol_count == 0` on constant-content frames, (e) the
  decode-free re-scan equivalence against the on-wire descriptor
  byte slice via `descriptor_start`, and (f) the
  `unused_symbol_count()` re-scan against the `0xff` byte count.
  Headline estimate unchanged at **decode ~97% / encode ~97%** —
  round 244 surfaces existing descriptor-byte semantics through a
  typed accessor, not new bitstream capability. Test count: 313
  (was 307, +6).

- **Round 241 — typed slice-header row accessor on
  `inspect::SliceLayout`.** Extends the decode-free per-frame layout
  with the partitioning fields the wiki formula
  (`spec/02` §5.2) derives from `(plane_height, num_slices,
  slice_index)` alone: `row_start: u32`, `row_end: u32`,
  `pixel_count: u32`, plus a `SliceLayout::row_count()` convenience
  method and a `PlaneLayout::total_pixels()` cross-check.
  Populated by `peek_frame` in the same pass that already builds the
  per-slice byte extents — zero additional buffer allocation, no
  Huffman state. Lets a container indexer / diagnostic tool answer
  "which rows does slice `s` produce" and "how many residual symbols
  is plane `k` going to emit" before any Huffman pass runs, matching
  the `n_pixels` argument shape of
  `HuffmanTable::decode_slice` (`spec/05` §6 pseudocode). The new
  fields are additive (`SliceLayout` keeps its `Copy` / `PartialEq`
  derives); existing callers reading only `start` / `end` see no
  behavioural change. Six dedicated tests
  (`tests/round241_slice_header_rows.rs`) pin the spec's worked
  example `R2-uly2-testsrc-16x17-s3` → `(5, 6, 6)`, the gap-free /
  overlap-free partition invariant, the `Σ pixel_count == width *
  height` identity, the `num_slices > plane_height` zero-row
  edge case (`spec/02` §5.1), the YUV-4:2:0 chroma half-height
  propagation, and the `pixel_count == n_pixels` Huffman-argument
  shape. Headline estimate unchanged at **decode ~97% / encode ~97%**
  — round 241 surfaces existing header math through a typed
  accessor, not new bitstream capability. Test count: 307 (was 301,
  +6).

- **Round 238 — per-slice predictor microbench
  (`benches/predict_slice.rs`).** Adds the fifth criterion bench,
  isolating the per-slice spatial-predictor primitives
  `predict::apply_slice` (inverse) and `predict::forward_slice`
  (forward) — the four-mode branch (None / Left / Gradient / Median)
  over a single slice's row strip with the universal `+128`
  first-pixel seed (`spec/04` §§3, 4, 7, 5) — that the existing
  `decode` / `encode` full-frame benches observe only inside the
  full pipeline. Three groups: `predict_inverse_slice`,
  `predict_forward_slice`, `predict_choose_predictor`, each over
  `Predictor × (w, rows) ∈ {(64, 64), (256, 256), (1920, 1080)} ×
  {natural, flat}`. Per-byte throughput pinned on the 1080p natural
  axis: inverse None ~72 GiB/s, Left ~3.6 GiB/s, Gradient ~1.5 GiB/s,
  Median ~533 MiB/s; forward None ~74 GiB/s, Left ~54 GiB/s, Gradient
  ~28 GiB/s, Median ~1.75 GiB/s — exposes the per-slice
  serial-cumulative dependency in `apply_*` as a separate axis from
  the full-pipeline number and identifies Median as the
  profile-guided next-step target. No public API change; no new
  test (bench-only addition); test count unchanged at 301.

- **Round 232 — direct Huffman-layer fuzz coverage.** The existing
  three cargo-fuzz targets (`decode_utvideo` / `encode_utvideo_frame`
  / `inspect_utvideo`) reach `HuffmanTable::build` + `decode_slice`
  only after the per-frame byte walk (`spec/02` §§4, 5) has accepted
  the chunk shape; on random bytes the walk rejects long before the
  Huffman layer runs. Round 232 adds a fourth cargo-fuzz target
  (`fuzz/fuzz_targets/huffman_codec.rs`) feeding a 256-byte descriptor
  + slice tail straight into `HuffmanTable::build` /
  `HuffmanTable::decode_slice` below the byte walk. Three properties
  are pinned on every input: (a) panic-freedom — `build` returns a
  `Result` on any 256-byte descriptor; (b) panic-freedom —
  `decode_slice` returns a `Result` on any
  `(slice_data, n_pixels)` against a built table; (c) on a
  synthesised Kraft-valid descriptor (uniform-length-`k` over `2^k`
  active symbols), a fuzz-derived symbol sequence encoded via
  `BitWriter::write_code(code_for(sym))` and decoded via
  `decode_slice` recovers the input bit-exactly. A 100,000-iteration
  libFuzzer smoke run found zero crashes.
- **Round 232 — stable-CI mirror at
  `tests/round232_huffman_codec_fuzz_properties.rs` (9 tests).**
  Drives the three fuzz properties on a deterministic seed corpus —
  empty input, all-zero descriptor, all-sentinel descriptor, 64-input
  xorshift sweep, truncated slice tails of length 0..=128, every
  uniform-length-`k` descriptor for `k ∈ 1..=8`, a skewed two-symbol
  roundtrip — so a regression surfaces in the regular `cargo test`
  lane instead of waiting for the daily fuzz run. Plus two
  deterministic-only enumeration properties the libFuzzer target
  can't easily reach: every single-symbol descriptor `sym ∈ 0..=255`
  round-trips through `decode_slice(&[], n_pixels)` correctly, and
  every "two-zero-sentinel" descriptor pair is checked for the
  `MultipleSingleSymbolSentinels` rejection.

- **Round 228 — fuzz coverage for the decode-free inspector.** Round 21
  exposed the public `peek_frame_info` + `peek_frame` byte-walk as a
  separate publicly-reachable parser of attacker-controlled chunk
  bytes; round 228 wires a third cargo-fuzz target
  (`fuzz/fuzz_targets/inspect_utvideo.rs`) that pins three properties:
  (a) panic-freedom — both inspector entrypoints always return a
  `Result`; (b) containment — every reported byte offset
  (`descriptor_start`, `end_offsets_start`, `slice_data_start`,
  per-slice `start` / `end`) lies inside `[0, payload.len())` and
  respects `descriptor_start <= end_offsets_start <= slice_data_start
  <= slice.start <= slice.end <= payload.len()`; (c) inspector /
  decoder agreement — when `decode_frame` succeeds on the same
  `(cfg, payload)`, `peek_frame` must also succeed AND the predictor +
  trailing `frame_info` dword must match between the two parsers.
  Header layout (`FOURCC mod 5 / width-seed / height-seed /
  slice-seed`) matches `decode_utvideo` so corpus entries are
  cross-useful.
- **Round 228 — stable-CI mirror at
  `tests/round228_inspect_fuzz_properties.rs` (9 tests).** Drives the
  three fuzz properties on a deterministic seed corpus — empty input,
  truncated-to-4-bytes-only-frame-info, 64-input xorshift sweep,
  garbage appended after frame_info, three truncation regimes,
  every length 0..=15 to `peek_frame_info`, every length 0..=299 to
  `peek_frame` — so a regression surfaces in the regular `cargo test`
  lane instead of waiting for the daily fuzz run. Plus a fourth
  deterministic-only property: every `(FOURCC, predictor, num_slices)
  ∈ {Ulrg, Ulra, Uly0, Uly2, Uly4} × {None, Left, Gradient, Median} ×
  {1, 2, 4, 8}` (80 cells) round-trips
  `encode_frame → peek_frame → decode_frame` and the inspector output
  is checked field-by-field against the decoder output.

- **Round 21 — decode-free frame-layout inspector (`inspect` module).**
  New public `peek_frame(cfg, chunk_payload) -> FrameLayout` and
  `peek_frame_info(chunk_payload) -> (u32, Predictor)` surface the
  per-frame byte layout without running any Huffman decode or
  allocating a residual buffer. `FrameLayout` carries per-plane
  byte extents (`descriptor_start`, `end_offsets_start`,
  `slice_data_start`, plus a `Vec<SliceLayout>` of per-slice
  `(start, end)` ranges), an `is_single_symbol` flag derived from
  the `spec/05` §6.1 descriptor-sentinel pattern, the trailing
  `frame_info` dword, and the predictor decoded from bits 8..9.
  Convenience accessors: `FrameLayout::total_size()` /
  `total_slice_data_bytes()`, `PlaneLayout::total_size()` /
  `slice_data_total()`, `SliceLayout::len()` / `is_empty()`. The
  inspector applies the same parse rules the full decoder uses
  (monotonic slice-end offsets, 4-byte word alignment per
  `spec/05` §4.1, total-length identity `payload = Σ plane_size + 4`)
  and surfaces the same `Error` variants
  (`MissingFrameInfo`, `ChunkTooShort`, `NonMonotonicSliceOffsets`,
  `SliceNotWordAligned`, `InvalidSliceCount`,
  `MultipleSingleSymbolSentinels`, `KraftViolation`) at the same
  point in the walk. Complexity is `O(plane_count * num_slices)`.
  Use cases: container-side indexers that want
  `(predictor, slice_count, per_plane_compressed_size)` per frame
  without the Huffman-decode cost; diagnostic tools that want
  per-plane byte extents on a corrupt frame; test harnesses pinning
  wire-format invariants.
- `inspect::{FrameLayout, PlaneLayout, SliceLayout, peek_frame,
  peek_frame_info}` — re-exported from crate root.

### Changed

- **Round 18 — content-adaptive trait-path predictor heuristic.**
  Round 17 wired the [`oxideav_core::Encoder`] trait path with a
  hardcoded `Predictor::Gradient` for every frame. Round 18 replaces
  that default with a per-frame heuristic: the trait encoder now calls
  [`predict::choose_predictor`] on the first plane (luma for YUV /
  G for RGB) and uses the result for every plane of that frame. The
  heuristic samples up to `HEURISTIC_SAMPLE_ROWS = 8` leading rows
  under each of the four candidate predictors (None / Left / Gradient /
  Median) and picks the one whose residual histogram has the lowest
  Shannon-entropy proxy — i.e. the per-plane Huffman code-length lower
  bound (`spec/05` §2.2). Tie-break order is `Gradient → Median →
  Left → None`, matching the round-15/16 dense-kernel benchmark
  ordering. The single per-frame predictor mirrors what `frame_info`
  bits 8..9 encode on the wire (`spec/02` §6.1). Direct-API callers of
  `encode_frame(EncodedFrame { predictor, .. })` are unaffected — they
  still hand in an explicit predictor verbatim.
- **Round 18 — `UtVideoEncoder::set_predictor` override hook.**
  Callers that need to pin a specific predictor on the trait path
  (testing / external policy / round-17 byte-equality) downcast and
  call `set_predictor(Some(Predictor::Gradient))`; passing `None`
  re-enables the heuristic. The hook is `#[allow(dead_code)]`-gated
  for the trait-object case (no `Any`-downcasting on `Box<dyn
  Encoder>` today) and reachable from the registry-module test suite.

### Added

- `predict::choose_predictor(plane, width, plane_height) -> Predictor`
  — per-plane heuristic returning the predictor with the lowest
  Shannon-entropy proxy on a sampled-row residual histogram. Public.
- `predict::HEURISTIC_SAMPLE_ROWS = 8` — the sample-row budget. Public
  constant so external callers can size their own pre-encode sampling
  to match the heuristic's working set.
- `tests/round18_predictor_heuristic.rs` — 17 tests pinning the
  heuristic across six invariant groups: content-discrimination on
  synthesised plane shapes (constant-plane → None, horizontal-stripes
  → Left/Gradient, linear-ramp → Gradient/Median/Left); determinism
  across repeated calls AND invariance under garbage rows past the
  sample budget; degenerate-input guard (zero-dim / width=1 /
  height=1 / height<sample_budget); trait-path round-trip under
  heuristic on every FourCC + on solid-plane + on linear-ramp;
  non-regression on entropy floor (heuristic choice within 0.5
  bit/sample of the full-frame optimum, strictly beats `None` on
  Gradient-friendly content); direct-API independence (explicit-
  predictor encode bytes diverge under different predictors AND
  decode-equal to the same plane). Plus 3 unit tests inside
  `src/registry.rs` exercising the `set_predictor` override hook
  through the inspected `frame_info` bits 8..9 of the encoded packet.

- **Round 17 — `Encoder` trait wiring from `CodecParameters` +
  end-to-end integration suite.** Mirrors round 14 (decoder trait
  wiring) on the encode side. The registry now advertises
  `CodecCapabilities::with_encode()` and installs
  `registry::make_encoder` so `CodecRegistry::has_encoder("utvideo")`
  returns `true` and `first_encoder(&CodecParameters)` constructs a
  trait-driven encoder. The encoder:
    * derives `Fourcc` from `params.tag` when present, else from
      `params.pixel_format` (Yuv420P→ULY0, Yuv422P→ULY2,
      Yuv444P→ULY4);
    * validates dims via `StreamConfig::new` (ULY0 even-W+H, ULY2
      even-W) at factory time;
    * synthesises a default-slice extradata via
      `Extradata::ffmpeg_for(fc, 1)` when `params.extradata` is empty,
      and preserves a populated 16-byte block verbatim (round-trips
      slice-count through to `output_params`);
    * accepts `Frame::Video` through `send_frame`, validates the plane
      count against `Fourcc::plane_count()`, repacks stride-padded
      plane buffers tight before encoding, and emits a `Packet` with
      `flags.keyframe = true` (`spec/02` §1 — Ut Video is intra-only);
    * rejects non-video frames, double `send_frame` without intervening
      `receive_packet`, and stride below plane-width.

### Added

- `tests/round17_encoder_trait_integration.rs` — 26 tests pinning six
  invariant groups: factory happy path on every FourCC × derivation
  route (tag vs. pixel format); trait-path byte-equality against a
  direct `encode_frame` call at single- and multi-slice; state-machine
  contract (`NeedMore` / `Eof` / double-send / keyframe / non-video
  rejection); factory construction-time rejection (missing
  tag+pixel_format, missing dims, packed RGB / `Gray8`, ULY0/ULY2
  dimension constraint violations, truncated extradata); plane-count +
  stride validation (wrong plane count, short plane, stride-padded
  repack tight); end-to-end round-trip through both traits and via the
  pixel-format derivation path, including the 32×32 ULY4 4-slice
  parallel-encode auto-dispatch path.

### Changed (cont.)

- **Round 16 — row-strided None + Left predictor refactor.** Round 15
  hoisted the row-0 / column-0 branches out of the Gradient and Median
  inner loops; the None and Left paths still iterated with per-pixel
  `plane[r * width + c]` index arithmetic. Round 16 rewrites
  `predict::apply_none` / `apply_left` and the matching forward
  `Predictor::None` / `Predictor::Left` arms in `forward_slice` to use
  row-strided `chunks_exact_mut(width)` / `chunks_exact(width)` over
  the slice-strip's rows. The inner row sees a fixed `width` slice so
  the compiler can elide the per-pixel bounds check; `apply_none`
  lowers to a straight `copy_from_slice` (memcpy intrinsic).

  Output remains bit-for-bit identical — the round is depth-mode code
  structure / bounds-check elision, not new bitstream capability. All
  195 prior tests still pass byte-equal.

  Wall: `docs/video/utvideo/spec/04` (None/Left predictor definitions,
  read-only — §3 identity, §4 continuous-wrap Left, §4.1.1 per-slice
  +128 seed) + `crates/oxideav-utvideo/{src/predict.rs,
  tests/round16_predictor_row_stride.rs}` (in-crate). Spec text only;
  correctness rests on the per-mode predictor identities the new
  row-strided loops compute pointwise the same as the prior interleaved
  loops.

### Added

- `tests/round16_predictor_row_stride.rs` — 17 tests pinning the
  byte-equality invariants the row-strided refactor must keep:
  `apply_none` round-strip on every FOURCC × `{single, multi, uneven,
  zero-row}` slice regimes; `apply_left` constant-zero / constant-V /
  row-constant plane signatures (forcing the row-to-row state-carry
  seam through `chunks_exact_mut`); 320×240 auto-dispatch byte-equality
  on parallel paths; encode determinism; minimal-width-1 edge case;
  cross-predictor parity. **212 tests** (was 195, +17). Headline
  estimate unchanged at **decode ~97% / encode ~96%**. ULH*/HBD/Lite/
  interlaced remain blocked on out-of-corpus docs.

### Previously (Round 15)

- **Round 15 — profile-driven Gradient + Median predictor refactor.**
  The four per-pixel branches inside `predict::apply_gradient` /
  `apply_median` (`r == r_start && c == 0` → 128 seed; `r == r_start`
  → Left-of-current; `c == 0` → above-of-column-0; else MED / GRAD)
  were checked at every pixel of every plane. Round 15 hoists the
  row-0 and column-0 special cases out of the inner loop so the dense
  interior runs branch-free as a tight cumulative add over `row[c-1]`
  + the row-above delta. Mirror change on the encoder side
  (`forward_gradient` / `forward_median`).

  Measured wall-clock on the criterion baseline (same 8-core host as
  Round 11; `benches/decode.rs` + `benches/encode.rs`):

  | Bench                                 | Round 11  | Round 15  | Δ       |
  | ------------------------------------- | --------- | --------- | ------- |
  | `decode_ulrg_1080p_single` (Grad)     | 41.5 ms   | 32.6 ms   | **-24%** |
  | `decode_uly2_1080p_single` (Grad)     | 27.3 ms   | 21.3 ms   | **-22%** |
  | `decode_parallel_scaling/serial/1`    | 17.9 ms   | 14.3 ms   | **-20%** |
  | `decode_parallel_scaling/parallel/8`  |  2.7 ms   |  2.26 ms  | **-16%** |
  | `encode_ulrg_1080p_single` (Grad)     | 38.8 ms   | 30.2 ms   | **-22%** |
  | `encode_uly2_1080p_single` (Grad)     | 23.9 ms   | 19.5 ms   | **-18%** |
  | `encode_parallel_scaling/serial/1`    | 16.1 ms   | 13.1 ms   | **-19%** |

  Decoder serial throughput rises from ~143 MiB/s to ~185 MiB/s on a
  1080p Gradient frame; parallel/8 crosses the 1 GiB/s mark
  (974 → 1140 MiB/s). The encoder gains ~20% across all path variants
  because `predict::forward_slice` runs the same branch hierarchy
  fix. Slice-parallel speedup ratio at 1280×720 ULY4 stays high at
  6.2× (was 6.7× pre-refactor — both serial and parallel paths
  improved, the ratio reflects the serial-baseline gain).

  All 195 tests still pass byte-identical (no algorithmic change —
  the optimisation is purely a branch / index-arithmetic refactor
  that produces identical output bit-for-bit to the prior path).
  `apply_left` / `apply_none` already ran as tight loops with no
  per-pixel branch tree and are unchanged. Headline estimate
  unchanged at **decode ~97% / encode ~96%** — Round 15 is depth-mode
  performance, not new bitstream capability. ULH*/HBD/Lite/interlaced
  remain blocked on out-of-corpus docs.

  Wall: `docs/video/utvideo/spec/04` (predictor definitions, read-only,
  for the algorithmic invariants underpinning the branch-hoist
  proof) + `crates/oxideav-utvideo/{src/predict.rs, benches/decode.rs,
  benches/encode.rs}` (in-crate). Spec text only; the refactor's
  correctness rests on the per-mode predictor identities in spec/04
  §§3–5, which the new branch-hoisted loops compute pointwise the
  same as the prior interleaved loops.

## [0.0.2](https://github.com/OxideAV/oxideav-utvideo/releases/tag/v0.0.2) - 2026-05-29

### Other

- Round 14 — Decoder trait wiring from CodecParameters + end-to-end integration
- Round 13 — ErrorCategory classifier + exhaustive Display regression suite
- add encode_utvideo_frame target + stable-CI mirror (round 12)
- Round 11 — criterion benchmarks for decode + encode hot paths
- Round 10 — cargo-fuzz decode harness
- Round 9 — descriptor-mutation rejection + encoder API misuse + bit-pack/unpack invariants
- Round 8 — malformed-payload decode robustness (negative tests)
- Round 7 — encoder byte-stability (idempotency) + full 1..256 slice sweep
- Round 6 — FFmpeg-pinned extradata builder + content-fixture corpus
- Round 5 — slice-parallel encode via std::thread::scope
- Round 4 — slice-parallel decode via std::thread::scope
- Round 3 — LUT-accelerated Huffman decoder + word-aligned bit peek
- Round 2 — exhaustive pattern matrix (corpus hardening)
- Round 1 — classic-family decoder + encoder
- Round 0 — clean-room rebuild scaffold (orphan master)

### Added

- **Round 14 — `Decoder` trait wiring from `CodecParameters` +
  end-to-end integration suite.** The registry `make_decoder` factory
  in `src/registry.rs` previously ignored `params.tag` /
  `params.extradata` / `params.width` / `params.height` and left the
  internal `StreamConfig` as `None`, relying on a private
  `configure()` hook that callers driving the codec through the
  `oxideav_core::Decoder` trait could not reach. The wiring now
  mirrors the `oxideav-huffyuv` pattern: at factory time we derive
  the FourCC from `CodecParameters.tag` (`CodecTag::Fourcc`), parse
  `params.extradata` via `Extradata::parse`, and validate dims via
  `StreamConfig::new`. Malformed extradata / chroma-constraint
  violations surface as `CoreError::InvalidData` at construction time
  so the container learns "this stream cannot decode" before any
  packet is dispatched. Missing pieces (no tag, no dims, empty
  extradata) leave `cfg` as `None` so the `configure()` hook still
  works for legacy callers; the first `receive_frame` then surfaces
  a "stream config not configured" diagnostic. Net effect: any
  container that hands us a populated `CodecParameters` (which
  `oxideav-avi` does today) gets a working trait-driven decoder
  without downcasting.

  New `tests/round14_decoder_trait_integration.rs` (21 tests) pins
  the contract end-to-end:

  - **Factory happy path on every FourCC** (1 test, ×5 cases):
    construct a `Decoder` via `CodecRegistry::first_decoder`, feed a
    chunk-payload produced by `encode_frame`, and assert the
    `Frame::Video` carries the right plane count, per-plane stride
    (= plane width), and per-plane payload length (= plane area).
  - **Trait-path byte equality** (1 test): the trait wrapper's
    `Frame::Video.planes[i].data` matches a direct `decode_frame`
    call on the same payload, byte-for-byte, so the registry
    introduces no transform between the codec output and the trait
    callable.
  - **State-machine contract** (5 tests): `receive_frame` before
    `send_packet` returns `NeedMore`; `flush` then `receive_frame`
    returns `Eof`; double `flush` is idempotent and still ends in
    `Eof`; double `send_packet` without `receive_frame` rejects;
    `pts` (`Some(_)` and `None`) flows through unchanged.
  - **Construction-time rejection** (4 tests): truncated extradata,
    Huffman-bit-clear extradata, interlaced-bit-set extradata, and
    wrong `frame_info_size` all return `Error::InvalidData` from
    `first_decoder`, never reaching a packet.
  - **Construction-time dim validation** (2 tests): ULY0 odd width
    and ULY2 odd width are rejected at factory time per `spec/02`
    §3.2.
  - **Construction-time deferral** (3 tests): empty extradata,
    missing `params.tag`, and missing `params.width` all defer
    config so the legacy `configure()` path stays usable. Decoding
    a packet then surfaces the "not configured" diagnostic.
  - **Cross-check** (3 tests): plane-label round-trip across all 5
    FourCCs, `ProbeContext`-routed FourCC resolution maps to the
    same codec id the factory is built against, multi-slice (128×128
    ULY4, 4 slices) trait decode delivers the expected plane shape.
  - **Capability flags + codec_id accessor** (2 tests): the
    `caps.implementation` / `caps.lossless` / `caps.intra_only` /
    `caps.decode` flags survive the round-14 wiring change;
    `Decoder::codec_id` returns the registered id.

  **195 tests** (was 174, +21). Headline estimate unchanged at
  **decode ~97% / encode ~96%** — round 14 closes the framework
  integration gap on the existing decode surface, not new
  bitstream capability. ULH*/HBD/Lite/interlaced remain blocked
  on out-of-corpus docs.

- **Round 13 — `ErrorCategory` classifier + exhaustive `Display`
  regression suite.** The 18-variant [`Error`] surface had no
  structured taxonomy: callers either pattern-matched every variant
  (brittle: a new variant added in a future round silently fell
  through at the call site) or relied on the `Display` text. Round
  13 adds an [`ErrorCategory`] enum with four buckets
  (`MalformedStream` / `ApiMisuse` / `Unsupported` / `StreamShape`),
  a `category()` accessor on `Error`, and four convenience predicates
  (`is_malformed_stream` / `is_api_misuse` / `is_unsupported` /
  `is_stream_shape`).

  - **`MalformedStream`** (8 variants): per-frame wire bytes don't
    match `docs/video/utvideo/spec/02` + `spec/05`. `ChunkTooShort`,
    `NonMonotonicSliceOffsets`, `SliceNotWordAligned`,
    `KraftViolation`, `MultipleSingleSymbolSentinels`,
    `HuffmanDecodeFailure`, `SliceTruncated`, `MissingFrameInfo`.
    A muxer-level caller MAY skip the offending packet and resync.
  - **`ApiMisuse`** (3 variants): caller violated the typed
    contract. `InvalidSliceCount`, `EncoderPlaneSizeMismatch`,
    `InvalidInput`. Programming bug, not corrupt wire data.
  - **`Unsupported`** (3 variants): structurally valid wire on a
    code path this build doesn't implement. `HuffmanBitClear`
    (raw-slice mode), `InterlacedNotSupported`,
    `UnsupportedPrediction`. Bounded out-of-corpus paths per
    `audit/00-report.md` §5.2.
  - **`StreamShape`** (4 variants): stream-level identification
    metadata malformed. `UnknownFourcc`, `ExtradataTruncated`,
    `InvalidFrameInfoSize`, `DimensionConstraint`. A demuxer should
    reject the stream, not retry per-frame.

  The classifier `match` in `error.rs` has no `_ =>` fallback by
  design: adding a new `Error` variant requires extending the
  category mapping in the same commit. `ErrorCategory` is
  `#[non_exhaustive]` so a fifth category in a future round is a
  non-breaking change.

  Plus a Round-1 message-accuracy fix: `Error::InvalidSliceCount`
  Display previously read `"num_slices == 0"`, but the variant is
  also produced for `num_slices > 256` (encoder, `Extradata::ffmpeg_for`,
  decoder). The new message names the full valid range:
  `"num_slices out of range (must be 1..=256 per spec/01 §4.4.3)"`.
  A regression test pins both the new message form and the absence
  of the stale `"== 0"` substring.

  New `tests/round13_error_taxonomy.rs` (22 tests):
  - **Display invariants** (15 tests): every variant's Display starts
    with the `"oxideav-utvideo:"` crate-name prefix and is non-empty;
    variant payload fields (FourCC hex bytes, byte counts, bit
    positions, plane indices, inner `&'static str` messages) all
    surface in the formatted output.
  - **Category mapping** (5 tests): each of the four categories has
    its variant list pinned to its [`ErrorCategory`] mapping; an
    `every_variant_belongs_to_exactly_one_category` partition test
    cross-checks that the four `is_*` predicates mutually exclude
    (exactly one returns `true` for every value); a
    `category_count_matches_variant_count` gate asserts the fixture
    list length is 18 (drift trips a clear assertion).
  - **`std::error::Error::source`** (1 test): every variant returns
    `None`. The crate has no wrapped third-party errors; future
    inadvertent wrapping trips this test.
  - **`ErrorCategory` derives usable** (1 test): `Copy` + `PartialEq`
    + `Eq` + `Hash` + `Debug` are all reachable downstream
    (`HashSet<ErrorCategory>` membership confirmed).

  **174 tests** (was 152, +22). No new public API surface beyond the
  classifier; no spec change; no wire-format change. Headline
  estimate unchanged at decode ~97% / encode ~96% — round 13 hardens
  the public error-handling contract, not the codec capability.

- **Round 12 — second cargo-fuzz target: encode-then-decode roundtrip.**
  Round 10 added the `decode_utvideo` target covering the attacker-facing
  surface (arbitrary bytes → `decode_frame`); the encoder is a different
  shape of risk (typed input, caller-driven), and on top of that the
  encoder/decoder pair must round-trip bit-exactly or the self-roundtrip
  invariant the round-1 tests pin on hand-picked fixtures silently
  regresses on some other shape. This round adds **`encode_utvideo_frame`**
  (registered explicitly in `fuzz/Cargo.toml` — no reusable-workflow
  auto-discovery dependency) that drives
  `(fourcc × dims ≤ 32×32 × predictor × num_slices × pixels)` through
  `encode_frame` → `decode_frame` and asserts every plane survives the
  roundtrip bit-exactly. The 32×32 dim cap keeps the fuzzer's budget
  on encoder/decoder logic (Huffman length builder, slice-range
  arithmetic, RGB decorrelate, bit-pack/unpack symmetry) rather than
  the trivial "allocate 4 GiB" branch the format's syntax allows.
  A **stable-CI mirror** at `tests/fuzz_seed_corpus_encode.rs` (11
  tests, mirroring the r160 h261 RTCP-fuzz pattern verbatim) runs the
  same driver logic against the committed seed corpus + a handful of
  inline adversarial buffers (empty input, 5-byte-only header,
  all-ones, deterministic-random, every FourCC × Left, every predictor
  × ULY2, slice-count-above-height, 32×32 ULY4 upper bound, ULRA
  4-plane alpha) so a regressed encoder or an encoder/decoder skew
  trips the regular CI matrix instead of waiting for the next daily
  fuzz run to notice. 8 committed seeds under
  `fuzz/corpus/encode_utvideo_frame/` cover the 5 FourCCs × 4
  predictors × single/multi-slice cross-product at small dims.
  No new public API. Headline estimate unchanged at decode ~97% /
  encode ~96%; this round is depth-mode robustness coverage, not new
  capability.

- **Round 11 — criterion benchmarks for the decode + encode hot paths.**
  The crate is saturated on the classic-family wire (decode ~97% /
  encode ~96%) with a daily fuzz harness in place; this round adds a
  baseline criterion bench suite so future optimisation work has a
  before/after measurement target.

  - `benches/decode.rs` (3 bench groups). Full-frame ULRG decode at
    1920×1080 single-slice (`decode_ulrg_1080p_single`); same at
    ULY2 1920×1080 (`decode_uly2_1080p_single`); and a
    `bench_with_input` scaling table at 1280×720 ULY4 with
    `N ∈ {1, 2, 4, 8}` slices covering both `decode_frame_serial`
    and `decode_frame_parallel` so the slice-parallel speedup is one
    chart row in criterion's output.
  - `benches/encode.rs` (3 bench groups). Symmetric coverage on
    `encode_frame` — ULRG / ULY2 1080p single-slice plus the
    `N ∈ {1, 2, 4, 8}` slice-parallel scaling at 1280×720 ULY4. The
    encoder's Amdahl-bounded ceiling (per-plane Huffman length build
    stays single-threaded by construction — the parallel slices share
    one codebook) is visible in the curve.
  - `benches/huffman_lut.rs` (2 bench groups). Isolated
    `HuffmanTable::decode_slice` microbench. `huffman_lut_pure_max12`
    builds a descriptor with `max_len = LUT_BITS = 12` so every code
    resolves on the round-3 LUT fast path; `huffman_lut_fallback_max14`
    uses `max_len = 14` so the top two tiers fall through to the
    slow-path length-tier binary search (the realistic high-entropy
    regime per `spec/02` §4.2). `bench_with_input` over
    `n_pixels ∈ {4096, 16384, 65536, 262144}` shows linear scaling and
    pins per-symbol decode rate.
  - `benches/rgb_decorrelate.rs` (2 bench groups). Microbench for the
    `predict::forward_decorrelate_rgb` / `inverse_decorrelate_rgb`
    primitives (`spec/04` §6 — the ULRG / ULRA inter-plane
    `B' = B - G + 128` / `R' = R - G + 128` and inverse transforms).
    `bench_with_input` over `n_samples ∈ {64K, 256K, 1M, 1920×1080}`
    pins the per-byte kernel rate.

  Measured median wall-clock on an 8-core host (release profile):

  | Bench                                       | Time      | Throughput     |
  | ------------------------------------------- | --------- | -------------- |
  | `decode_ulrg_1080p_single` (Gradient)       | 40.56 ms  | 146 MiB/s      |
  | `decode_uly2_1080p_single` (Gradient)       | 26.65 ms  | 148 MiB/s      |
  | `decode_parallel_scaling/serial/8`          | 17.67 ms  | 149 MiB/s      |
  | `decode_parallel_scaling/parallel/8`        |  2.67 ms  | 987 MiB/s      |
  | `encode_ulrg_1080p_single` (Gradient)       | 37.00 ms  | 160 MiB/s      |
  | `encode_uly2_1080p_single` (Gradient)       | 24.07 ms  | 164 MiB/s      |
  | `encode_parallel_scaling/parallel/8`        |  ~3 ms    | ~875 MiB/s     |
  | `huffman_lut_pure_max12/262144`             |  1.02 ms  | 257 Melem/s    |
  | `huffman_lut_fallback_max14/262144`         |  1.32 ms  | 199 Melem/s    |
  | `rgb_inverse_decorrelate/2073600`           | 73.8 µs   | 26.2 GiB/s     |
  | `rgb_forward_decorrelate/2073600`           | 76.7 µs   | 25.2 GiB/s     |

  The 8-slice parallel-decode speedup at 1280×720 lands at ~6.7× over
  the 1-slice serial baseline (was ~5.6× per the round-4 hand-timed
  perf-smoke; the criterion methodology with batched iterations and
  warm cache narrows the noise floor). The LUT fast-path is ~22%
  faster than the fallback search at the largest input. All inputs
  are synthesised on-the-fly from a deterministic xorshift32 PRNG;
  no committed binary fixtures.

  Headline estimate unchanged at **decode ~97% / encode ~96%** — round
  11 is depth-mode benchmark coverage, not new capability.
  ULH*/HBD/Lite/interlaced remain blocked on out-of-corpus docs.

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
