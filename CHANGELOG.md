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

- Bit-exact ffmpeg interop fixtures for `ULRA` (gbrap, alpha plane),
  `ULY0` (yuv420p, half-half subsampled chroma), and `ULY4` (yuv444p,
  full-resolution chroma) with predictors NONE/LEFT/MEDIAN — 9 new
  64×48 single-frame AVIs (15 fixtures total). Exercises the alpha
  plane code path, the 4:2:0 chroma row-partition, and the 4:4:4
  no-subsample plane handling.

- Pro UQ (10-bit) and Pack UM (SymPack) FourCC catalogue + extradata
  parsing now reachable. `PlaneShape::from_fourcc` returns a
  `Family::Pro` / `Family::Pack` shape (with `bit_depth = 10` for UQ,
  YUV-side `Yuv4xxP10Le` PixelFormats; UQRG/UQRA placeholder map to
  `Rgb48Le` / `Rgba64Le` until a `Gbrp10*` variant lands in
  oxideav-core), `ExtraData::parse` now exercises the previously
  unreachable `parse_pro` and `parse_pack` branches, and
  `UtVideoDecoder::new` rejects each non-classic family with an
  explicit "decode not yet implemented; see trace doc §6 / §7" error
  rather than the prior generic "only classic UL is wired" message.
  Unit-tested across all six pro tags (`UQRG/UQRA/UQY0/UQY2`) and
  pack tags (`UMRG/UMRA/UMY2/UMY4` plus their `UMH*` BT.709 twins),
  including a regression assertion that no `UMY0` / `UMH0` exists per
  trace doc §7 / §12.1.
- Per-pixel unit coverage for the GRADIENT and MEDIAN inverse
  predictors: hand-crafted forward → inverse round-trip tests over
  6×4 / 5×4 plane fragments (covering row-0 LEFT seed, column-0 TOP
  step, gradient interior, MEDIAN row-1-col-0 collapse), plus the
  trace doc §8.1 divergence example asserting Ut Video's MEDIAN
  diverges from JPEG-LS clip-MED on `A+B-C` overflow neighbourhoods.
  Closes the GRADIENT validation gap left by the missing ffmpeg
  encoder support.

### Changed

- README: corrected the interop coverage line. FFmpeg's `utvideo`
  encoder rejects `-pred gradient` with `AVERROR_PATCHWELCOME`, so the
  GRADIENT inverse-predictor — though implemented and trace-doc
  verified per spec §8 — has no third-party reference encoding to
  validate against. Bit-exact interop is now stated as
  ULRG/ULRA/ULY0/ULY2/ULY4 × NONE/LEFT/MEDIAN.

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
