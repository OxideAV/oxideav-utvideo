# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
