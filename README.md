# oxideav-utvideo

Pure-Rust Ut Video lossless codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 4 — slice-parallel decode.** `decode_frame` now auto-dispatches
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
or threadpool-controlled callers. 80 tests = 52 unit + 16 round-2
matrix + 6 round-3 LUT probes + 6 round-4 parallel-correctness
probes.

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
