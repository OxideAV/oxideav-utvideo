# oxideav-utvideo

Pure-Rust Ut Video lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

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
