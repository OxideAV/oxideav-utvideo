# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
