# oxideav-utvideo

Pure-Rust decoder for **Ut Video**, Takeshi Umezawa's lossless intra-only
video codec — 8-bit classic family today (`ULRG`, `ULRA`, `ULY0/2/4`,
`ULH0/2/4`). Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core    = "0.1"
oxideav-utvideo = "0.0"
```

## What works today

| Area                              | State                                                          |
|-----------------------------------|----------------------------------------------------------------|
| Family                            | classic UL (8-bit)                                             |
| FourCCs                           | `ULRG`, `ULRA`, `ULY0`, `ULY2`, `ULY4`, `ULH0`, `ULH2`, `ULH4` |
| Extradata parser                  | classic 16-byte (slices, interlaced, frame\_info\_size)        |
| Per-plane canonical Huffman       | yes (Ut Video tree orientation: all-ones shortest)             |
| Predictors                        | `NONE`, `LEFT`, `GRADIENT`, `MEDIAN` (8-bit)                   |
| Single-symbol fast path           | yes (`length == 0` short-circuit)                              |
| RGB G-centred inverse transform   | yes (ULRG / ULRA)                                              |
| Multi-slice packets               | yes                                                            |
| Interop (FFmpeg → us)             | bit-exact for ULRG, ULRA, ULY0, ULY2, ULY4 with predictors NONE/LEFT/MEDIAN (15 fixtures); GRADIENT not exercised — FFmpeg's encoder rejects it (`AVERROR_PATCHWELCOME`) so no third-party reference exists |

## Not yet implemented

| Item                  | Notes                                                                |
|-----------------------|----------------------------------------------------------------------|
| Pro UQ family         | `UQRG`, `UQRA`, `UQY0`, `UQY2` — 10-bit, header order swapped (trace report §6) |
| Pack UM (SymPack)     | `UMRG`, `UMRA`, `UMY2/4`, `UMH2/4` — two-stream block-of-8 raw-bits coder (trace report §7) |
| Interlaced re-pairing | flag is parsed; the per-frame line re-pair is not yet wired          |
| YUV→RGB / colour conv | callers receive planar Y/U/V or G/B/R\[A\] and convert themselves    |

## Quick use

```rust
use oxideav_utvideo::{decode_packet, FourCc};

let frame = decode_packet(
    FourCc(*b"ULRG"),
    &extradata,         // 16-byte classic-family extradata
    width,
    height,
    &packet_bytes,      // raw frame body from the container
)?;
// frame.planes[0..3] = G, B, R for ULRG  (or Y, U, V for ULY*)
```

## Implementation notes

The bitstream reverse-engineering lives in
`docs/video/utvideo/utvideo-trace-reverse-engineering.md` of the
oxideav workspace. Highlights:

* **Word-swap**. On-disk slice bytes are read 4 at a time and the four
  bytes within each group are reversed before the MSB-first bit reader
  scans them — a VfW-era quirk inherited from MSVC `DWORD`
  bit-fiddling.
* **Canonical Huffman**. The 256-byte per-plane length table encodes
  only lengths; codewords are reconstructed deterministically with
  the convention "all-ones shortest, all-zeros longest", which is the
  *opposite* of the more common convention.
* **Predictor reset per slice**. LEFT / GRADIENT / MEDIAN state
  resets at the top of every slice. The first pixel of every LEFT
  row is seeded from `0x80`.
* **RGB G-centred residuals**. ULRG / ULRA store B and R as `B - G -
  0x80` and `R - G - 0x80` (mod 256). The inverse step runs after
  Huffman + predictor.
* **Single-symbol fast path**. A `length == 0` entry in the length
  table means "this symbol fills the plane"; the slice data block is
  empty in that case.

## License

MIT — see `LICENSE`.
