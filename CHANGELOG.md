# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.2](https://github.com/OxideAV/oxideav-utvideo/compare/v0.0.1...v0.0.2) - 2026-05-04

### Other

- accept Pro UQ + Pack UM FourCCs (extradata path now reachable)
- add GRADIENT and MEDIAN forward+inverse round-trip tests
- rustfmt fixup for ULRA assert_eq line length
- add ULRA + ULY0 + ULY4 ffmpeg interop fixtures (15 total)
- replace never-match regex with semver_check = false

### Added

- **Interlaced decode (ULRG)**: bit-exact against two ffmpeg FATE
  samples (`utvideo_rgb_64x48_int_gradient.avi` and
  `utvideo_rgb_64x48_int_median.avi`). The interlaced GRADIENT
  inverse predictor uses a stride-2 neighbourhood (each field
  predicted from its same-field row 2 lines up) with a field-parity
  rule at column 0: even rows (top field) use TOP; odd rows (bottom
  field) use the full gradient formula with above-left wrapping
  linearly to the last pixel of the row two above. MEDIAN uses the
  same shared-first-two-rows LEFT scan plus stride-2 mid_pred with
  consistent linear-scan wrapping. Both predictors are verified pixel-
  exact against ffmpeg's `utvideo` decoder output across all 64×48×3
  planes.

- **Pro UQ (10-bit) decoder**: full packet decode for `UQRG`, `UQRA`,
  `UQY0`, `UQY2`. Reads the 4-byte `frame_info` header at the START
  of the packet (unlike classic which places it at the END), extracts
  slice count from `((frame_info >> 16) & 0xFF) + 1`, reads per-plane
  layout in `[offsets | slice-data | 1024-byte Huffman lengths]` order,
  decodes 10-bit symbols via `HuffTable10` (1024-symbol canonical
  Huffman), applies LEFT predictor mod 1024 with seed `0x200`, and
  packs the u16 output as LE bytes (stride = width × 2). 10-bit
  G-centred RGB inverse transform (`R = (R'+G−0x200) & 0x3FF`) applied
  for UQRG/UQRA. GRADIENT/MEDIAN silently fall back to NONE per spec.

- **Pack UM / SymPack decoder**: full packet decode for `UMRG`, `UMRA`,
  `UMY2`, `UMY4`, `UMH2`, `UMH4`. Reads the 8-byte packet header,
  separates the packed and control bit-streams, decodes each slice via
  a block-of-8 LE-bit coder (3-bit control word `b`; `b==0` → 8 zero
  residuals; `b>0` → `8×(b+1)` packed bits with sign-flip mapping
  `pixel = ((~p & sub) << (8−b)) + p − sub mod 256`). Hardcoded
  GRADIENT predictor (trace doc §12.1). `LeBitReader` added to
  `huffman.rs` for LSB-first bit reads.

- Bit-exact ffmpeg interop fixtures for `ULRA` (gbrap, alpha plane),
  `ULY0` (yuv420p, half-half subsampled chroma), and `ULY4` (yuv444p,
  full-resolution chroma) with predictors NONE/LEFT/MEDIAN — 9 new
  64×48 single-frame AVIs (15 fixtures total). Exercises the alpha
  plane code path, the 4:2:0 chroma row-partition, and the 4:4:4
  no-subsample plane handling.

- Per-pixel unit coverage for the GRADIENT and MEDIAN inverse
  predictors: hand-crafted forward → inverse round-trip tests over
  6×4 / 5×4 plane fragments (covering row-0 LEFT seed, column-0 TOP
  step, gradient interior, MEDIAN row-1-col-0 collapse), plus the
  trace doc §8.1 divergence example asserting Ut Video's MEDIAN
  diverges from JPEG-LS clip-MED on `A+B-C` overflow neighbourhoods.
  Closes the GRADIENT validation gap left by the missing ffmpeg
  encoder support.

### Changed

- `UtVideoDecoder::new` now accepts all three families (Classic, Pro,
  Pack) instead of returning an error for Pro and Pack.

- README: corrected the interop coverage line. FFmpeg's `utvideo`
  encoder rejects `-pred gradient` with `AVERROR_PATCHWELCOME`, so the
  GRADIENT inverse-predictor — though implemented and trace-doc
  verified per spec §8 — has no third-party reference encoding to
  validate against. Bit-exact interop is now stated as
  ULRG/ULRA/ULY0/ULY2/ULY4 × NONE/LEFT/MEDIAN plus interlaced
  GRADIENT/MEDIAN (ULRG, ffmpeg FATE samples).

## [0.0.1] - 2026-05-02

### Added

- Initial scaffold: extradata parser for the classic UL family
  (`ULRG`, `ULRA`, `ULY0/2/4`, `ULH0/2/4`).
- Per-plane canonical-Huffman decoder over the 256-byte length table
  with the Ut Video tree orientation (longer codewords toward the left,
  all-ones shortest, all-zeros longest).
- 32-bit-word byte-swap of slice data; MSB-first bit reader.
- Predictor enum from `frame_info` bits 8-9: NONE / LEFT / GRADIENT /
  MEDIAN. Inverse predictors implemented for NONE, LEFT, GRADIENT,
  MEDIAN over 8-bit samples.
- Slice-offset table (cumulative end positions); per-plane image-rows
  partitioned by integer height/N\_slices.
- ULRG (gbrp) and ULY2 (yuv422p) decode paths verified against
  `ffmpeg -c:v utvideo` output (bit-exact pixel match).
- Single-symbol "length=0" fast path.
- G-centred RGB inverse colour transform for ULRG/ULRA.
