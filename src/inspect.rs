//! Frame-layout inspector for Ut Video chunk payloads.
//!
//! This module exposes a **decode-free** view of the per-frame byte
//! layout: given a [`StreamConfig`] and one chunk-payload buffer, it
//! returns the trailing `frame_info` dword, the selected predictor,
//! and the per-plane byte extents of the Huffman descriptor + the
//! slice-end-offset table + each slice's bit-stream range. No Huffman
//! decode runs and no residual buffer is allocated.
//!
//! ## Why a separate inspector path
//!
//! The full decoder ([`crate::decode_frame`]) does the same byte-walk
//! before kicking off Huffman + inverse-predict, but it only surfaces
//! the trailing `frame_info` dword on the [`crate::DecodedFrame`]
//! output — the per-plane descriptor / slice-table / slice-data byte
//! offsets are consumed internally and never reach the caller. A
//! container indexer / diagnostic tool / pre-decode statistics pass
//! often needs exactly those offsets:
//!
//! - A muxer-side indexer that wants `(predictor, slice_count,
//!   per_plane_compressed_size)` for every frame in a clip to drive
//!   bit-budget planning, without paying the Huffman-decode cost.
//! - A diagnostic that wants to point at "which plane went bad" when
//!   a downstream consumer reports a corrupt frame — the inspector's
//!   error path carries `plane_idx` on every malformed-structure
//!   variant; the full decoder ([`crate::decode_frame`]) currently
//!   surfaces the byte offset relative to the chunk payload only.
//! - A test harness that wants to round-trip wire-format invariants
//!   (offsets monotonic, word-aligned, descriptor-byte sum-rule from
//!   `spec/05` §2.2) without re-implementing the byte walk.
//!
//! The inspector path is also a clean place to put a single source of
//! truth for "what's the wire size of plane k of an arbitrary
//! ([`Fourcc`], width, height, num_slices, descriptor) tuple"
//! — useful when a caller wants to *pre-compute* an upper or lower
//! bound on a frame's encoded size before encoding runs.
//!
//! ## Spec anchors
//!
//! All extents map directly onto `spec/02` §§1, 2, 4, 5 + the
//! trailing 4-byte `frame_info` dword (`spec/02` §6 + §6.1 for the
//! predictor field). The same parse rules the full decoder uses —
//! monotonic non-decreasing slice-end offsets (`spec/02` §5),
//! 4-byte word-alignment of every slice-end value (`spec/05` §4.1),
//! and the total-length identity `payload = Σ plane_size + 4` — apply
//! verbatim. The inspector reports the precise error variant the
//! full decoder would surface at the same point in the walk.

use crate::error::{Error, Result};
use crate::fourcc::{Predictor, StreamConfig};

/// One slice's wire-format byte extent within the chunk payload,
/// plus the typed slice-header fields the partitioning rule
/// (`spec/02` §5.2) derives from `(plane_height, num_slices,
/// slice_index)` alone.
///
/// The two layers are independent (`spec/02` §5.2 final paragraph):
/// `start` / `end` carry the **compressed-byte** range pulled
/// straight from the per-plane `slice_end_offsets` table, while
/// `row_start` / `row_end` / `pixel_count` carry the **decoded-pixel**
/// extent computed from the wiki partitioning formula. Both layers
/// are decode-free — no Huffman state is needed to populate either —
/// but they answer different questions ("which bytes carry this
/// slice's bit-stream?" vs. "which plane rows does this slice
/// produce, and how many residual symbols will the Huffman pass
/// emit?").
///
/// `start <= end` always. A zero-length slice (`start == end`) is
/// legal — it arises when `num_slices > plane_height` and the
/// per-slice `floor(ph*(s+1)/N) - floor(ph*s/N)` row count collapses
/// to zero rows (`spec/02` §5.1 — empty bit-stream allowed).
///
/// `row_start <= row_end <= plane_height` always. `pixel_count` is
/// `(row_end - row_start) * plane_width` per the
/// `decode_slice_residuals(n_pixels)` argument shape in `spec/05`
/// §6 (the per-slice Huffman pass emits exactly `pixel_count`
/// residual bytes, including the trailing pad bits the bit reader
/// consumes from the word-aligned slice tail per `spec/05` §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceLayout {
    /// Absolute byte offset of this slice's first bit-stream byte.
    pub start: usize,
    /// Absolute byte offset just past this slice's last bit-stream byte.
    pub end: usize,
    /// First plane row this slice produces (inclusive), per the
    /// `spec/02` §5.2 partitioning rule
    /// `row_start = floor((plane_height * slice_index) / num_slices)`.
    pub row_start: u32,
    /// One past the last plane row this slice produces, per the
    /// `spec/02` §5.2 partitioning rule
    /// `row_end = floor((plane_height * (slice_index + 1)) / num_slices)`.
    /// Equal to `row_start` for empty slices
    /// (`num_slices > plane_height`).
    pub row_end: u32,
    /// Number of residual symbols this slice's Huffman pass emits:
    /// `(row_end - row_start) * plane_width`. Matches the
    /// `n_pixels` argument shape of
    /// [`crate::huffman::HuffmanTable::decode_slice`] (`spec/05` §6,
    /// behavioural pseudocode).
    pub pixel_count: u32,
}

impl SliceLayout {
    /// Byte length of this slice's bit-stream. `end - start`.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True iff `len() == 0`. Useful for the empty-slice edge case
    /// surfaced by `num_slices > plane_height`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Number of plane rows this slice produces: `row_end - row_start`.
    /// Equal to `pixel_count / plane_width` when `plane_width > 0`.
    #[inline]
    pub fn row_count(&self) -> u32 {
        self.row_end - self.row_start
    }
}

/// One plane's wire-format byte extents within the chunk payload.
///
/// The four sub-regions are laid out contiguously in that order:
///
/// ```text
/// descriptor: [descriptor_start .. descriptor_start + 256)
/// slice_end_offset_table: [end_offsets_start .. end_offsets_start + 4*num_slices)
/// slice data: [slice_data_start .. slice_data_start + slice_data_total)
/// ```
///
/// where `slice_data_total = slices[num_slices - 1].end - slice_data_start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Wire-format plane index (0..plane_count). Plane 0 is the luma
    /// plane (YUV) or G plane (RGB[A]) per `spec/02` §3.
    pub plane_idx: usize,
    /// Plane width in samples (per-FourCC chroma-subsampled for U/V).
    pub width: u32,
    /// Plane height in samples (per-FourCC chroma-subsampled for U/V).
    pub height: u32,
    /// Absolute byte offset of this plane's 256-byte Huffman code-length
    /// descriptor (`spec/02` §4).
    pub descriptor_start: usize,
    /// Absolute byte offset of the slice-end-offset table
    /// (`spec/02` §5). Length is `4 * num_slices` bytes.
    pub end_offsets_start: usize,
    /// Absolute byte offset of the first slice's bit-stream
    /// (`spec/02` §5 + `spec/05` §4).
    pub slice_data_start: usize,
    /// Per-slice byte extents (length `num_slices`). The end-offsets
    /// table is parsed and converted to absolute payload offsets here.
    pub slices: Vec<SliceLayout>,
    /// Whether the Huffman descriptor encodes a single-symbol plane
    /// (sentinel: code-length 0 appears for exactly one symbol, all
    /// other code-lengths are 255). `spec/05` §6.1. The plane carries
    /// zero slice-data bytes when this is true.
    pub is_single_symbol: bool,
    /// Decode-free count of symbols carrying an explicit code length
    /// in this plane's 256-byte descriptor — i.e. the count of
    /// `code_length[s]` entries with value in the active range
    /// `1..=254` per `spec/05` §2.1 (entries with value `0` are the
    /// single-symbol sentinel from `spec/05` §6.1, entries with value
    /// `255` are the unused-symbol sentinel from `spec/05` §2.1).
    ///
    /// Range: `0..=256`. Two well-formed shapes the decoder accepts:
    ///
    /// - `0` — paired with `is_single_symbol == true`, the plane
    ///   carries a single repeated symbol and `slice_data_total == 0`
    ///   (`spec/05` §6.1). The lone `code_length[s] == 0` entry is
    ///   the sentinel, NOT counted as an active code.
    /// - `2..=256` — the plane has a multi-symbol canonical Huffman
    ///   codebook satisfying the Kraft equality
    ///   `Σ 2^(-code_length[s]) == 1` over the active set
    ///   (`spec/05` §2.1 + §2.2).
    ///
    /// `1` is structurally rejectable at build time (a single
    /// non-sentinel length cannot satisfy Kraft equality on a
    /// non-trivial alphabet, `spec/05` §2.2), but `peek_frame`
    /// remains a decode-free byte-walk and surfaces the raw count
    /// here — `HuffmanTable::build` is what enforces Kraft
    /// (`spec/05` §2.2 step 3, surfaced as `Error::KraftViolation`).
    pub active_symbol_count: u32,
    /// Maximum code length present in this plane's 256-byte Huffman
    /// descriptor — the largest value of `code_length[s]` over the
    /// active range (`spec/05` §2.1: entries in `1..=254`).
    ///
    /// Range: `0..=254`. Two interpretations:
    ///
    /// - `0` — no active code lengths in the descriptor. This is the
    ///   case on (a) the `spec/05` §6.1 single-symbol plane (paired
    ///   with `is_single_symbol == true` and `active_symbol_count ==
    ///   0`) and (b) the degenerate all-unused descriptor (every byte
    ///   is the `255` sentinel — structurally rejected by
    ///   `HuffmanTable::build` because Kraft equality can't be
    ///   satisfied on an empty alphabet, but `peek_frame` surfaces
    ///   the raw scan).
    /// - `1..=254` — the longest canonical-Huffman code this plane
    ///   carries, in bits.
    ///
    /// Useful for a decoder selecting a decode strategy without
    /// building the codebook: per `spec/05` §7.3, a flat
    /// `2^max_code_length`-entry lookup table is appropriate when
    /// the maximum is `<= 16`; a multi-stage table is preferable for
    /// `16 < max_code_length <= 24`; only a tree-walk decoder works
    /// for the full wire-format upper bound of `254` (`spec/05` §7.2).
    /// `spec/05` §7.1 reports `16` as the maximum observed across the
    /// behavioural corpus — but the `1..=254` wire range is what a
    /// strictly conformant decoder must accept.
    ///
    /// Range: `0..=254`. The `255` sentinel is the "symbol unused"
    /// marker per `spec/05` §2.1, not a code length, so it never
    /// surfaces here.
    pub max_code_length: u8,
    /// Minimum code length present in this plane's 256-byte Huffman
    /// descriptor — the smallest value of `code_length[s]` over the
    /// active range (`spec/05` §2.1: entries in `1..=254`).
    ///
    /// Range: `0..=254`. Two interpretations:
    ///
    /// - `0` — no active code lengths in the descriptor. Same shapes
    ///   as [`max_code_length`] reports `0`: (a) the `spec/05` §6.1
    ///   single-symbol plane (paired with `is_single_symbol == true`
    ///   and `active_symbol_count == 0`); (b) the degenerate
    ///   all-unused descriptor (every byte is the `255` sentinel,
    ///   structurally rejected by `HuffmanTable::build` because Kraft
    ///   equality can't be satisfied on an empty alphabet, but
    ///   `peek_frame` surfaces the raw scan).
    /// - `1..=254` — the shortest canonical-Huffman code this plane
    ///   carries, in bits. Always satisfies `min_code_length <=
    ///   max_code_length`.
    ///
    /// Per the `spec/05` §2.2 construction algorithm, the shortest
    /// code is the all-ones bit pattern at this length (wiki line
    /// 34 of the source snapshot cited in `spec/05` §1); the typed
    /// accessor lets a container indexer / diagnostic tool reason
    /// about the prefix-code shape without standing up the
    /// `HuffmanTable`.
    ///
    /// Useful as a Kraft typed cross-check against the round-244
    /// [`active_symbol_count`] accessor: for `K >= 2` active symbols,
    /// Kraft equality (`Σ 2^-code_length[s] == 1` over the active
    /// set per `spec/05` §2.2 step 3) requires the smallest term
    /// `2^-min_code_length` to be `>= 1 / K`, giving the typed upper
    /// bound `min_code_length <= floor(log2(K))`. Pairs with the
    /// round-250 lower bound `max_code_length >= ceil(log2(K))`.
    ///
    /// The `255` sentinel is the "symbol unused" marker per
    /// `spec/05` §2.1 — never a code length, so the active-range
    /// scan ignores it.
    ///
    /// [`active_symbol_count`]: PlaneLayout::active_symbol_count
    /// [`max_code_length`]: PlaneLayout::max_code_length
    pub min_code_length: u8,
    /// Number of descriptor entries that share [`min_code_length`] —
    /// the multiplicity of the shortest-length tier in this plane's
    /// canonical Huffman codebook. Counted decode-free over the active
    /// range (`spec/05` §2.1: entries in `1..=254`).
    ///
    /// Range: `0..=256`. Three interpretations:
    ///
    /// - `0` — paired with `min_code_length == 0`. The plane has no
    ///   active code lengths in the descriptor — either the `spec/05`
    ///   §6.1 single-symbol path (paired with `is_single_symbol ==
    ///   true` and `active_symbol_count == 0`) or the degenerate
    ///   all-unused descriptor (every byte is the `255` sentinel,
    ///   structurally rejected by `HuffmanTable::build` but surfaced
    ///   here as a raw descriptor scan).
    /// - `1` — exactly one symbol carries the shortest code length;
    ///   that code is the all-ones bit pattern at length
    ///   `min_code_length` per `spec/05` §2.2 step 4 + §2.4. Pairs
    ///   with `active_symbol_count >= 1` and `min_code_length >= 1`.
    /// - `2..=256` — multiple symbols share the shortest length. The
    ///   `spec/05` §6.2 two-symbol `{1, 1}` case is the typed minimum
    ///   non-trivial multiplicity (`min_code_length == 1` and
    ///   `min_code_length_symbol_count == 2`); the §6.3 / §6.4
    ///   single-length descriptors saturate this at
    ///   `active_symbol_count` (every active symbol shares the one
    ///   length).
    ///
    /// Typed cross-checks against the existing counters:
    ///
    /// - `min_code_length_symbol_count <= active_symbol_count` —
    ///   trivially, the count of one length-tier is bounded by the
    ///   total active count.
    /// - When `min_code_length == max_code_length`
    ///   (single-length-descriptor path of `spec/05` §6.3 / §6.4),
    ///   `min_code_length_symbol_count == active_symbol_count` — every
    ///   active symbol sits in the same (and only) length tier.
    /// - Kraft typed lower bound: per `spec/05` §2.2 step 3, the
    ///   shortest-length tier contributes
    ///   `min_code_length_symbol_count * 2^-min_code_length` to the
    ///   Kraft sum. For Kraft equality to hold with non-negative
    ///   contributions from longer-length tiers, this term must be
    ///   `<= 1`, i.e. `min_code_length_symbol_count <=
    ///   2^min_code_length`. (Equality is the single-length-descriptor
    ///   case above.)
    ///
    /// The `255` sentinel ("symbol unused" per `spec/05` §2.1) is
    /// excluded — it isn't a code length and never enters the count.
    ///
    /// [`min_code_length`]: PlaneLayout::min_code_length
    pub min_code_length_symbol_count: u32,
    /// Per-length-tier multiplicity of this plane's 256-byte Huffman
    /// descriptor — the full **code-length histogram** over the active
    /// range (`spec/05` §2.1: entries in `1..=254`), as a compact
    /// ascending-by-length list of `(code_length, count)` pairs.
    ///
    /// This is the per-tier structure the `spec/05` §2.2 step 2 sort
    /// groups symbols into before the §2.2 step 4 code-assignment walk:
    /// each pair `(L, n)` says "`n` active symbols carry an `L`-bit
    /// code". The scalar accessors shipped through rounds 244 / 250 /
    /// 255 / 261 are all projections of this list:
    ///
    /// - [`active_symbol_count`] `== Σ n` over every pair.
    /// - [`max_code_length`] `==` the `L` of the last pair (`0` when
    ///   empty).
    /// - [`min_code_length`] `==` the `L` of the first pair (`0` when
    ///   empty).
    /// - [`min_code_length_symbol_count`] `==` the `n` of the first
    ///   pair (`0` when empty).
    ///
    /// Ordering + shape guarantees:
    ///
    /// - **Ascending by length**, strictly: each pair's `L` is greater
    ///   than the previous pair's, so the list has at most one entry per
    ///   length tier (`1..=254`, so at most 254 entries).
    /// - **No zero-count tiers**: a length carrying no active symbol is
    ///   absent from the list (it is NOT recorded as `(L, 0)`).
    /// - **Empty list** exactly when [`active_symbol_count`] `== 0` —
    ///   the `spec/05` §6.1 single-symbol path (paired with
    ///   `is_single_symbol == true`) or the degenerate all-`255`-unused
    ///   descriptor (structurally rejected by `HuffmanTable::build` but
    ///   surfaced decode-free here).
    ///
    /// Typed cross-checks the list makes available without standing up a
    /// `HuffmanTable`:
    ///
    /// - **Kraft numerator** (`spec/05` §2.2 step 3): scale the Kraft
    ///   sum `Σ n · 2^-L` by `2^max_code_length` to get the integer
    ///   `Σ n · 2^(max-L)`; a Kraft-complete descriptor satisfies
    ///   `Σ n · 2^(max-L) == 2^max_code_length` exactly. The
    ///   [`kraft_numerator`] convenience returns that integer so a
    ///   container indexer can validate a descriptor's prefix-code
    ///   completeness decode-free (the same equality
    ///   `HuffmanTable::build` enforces as `Error::KraftViolation`,
    ///   `spec/05` §2.2). A single-length descriptor (`spec/05` §6.3 /
    ///   §6.4) is the degenerate one-pair list `[(L, 2^L)]`.
    ///
    /// The `0` sentinel (`spec/05` §6.1) and the `255` sentinel
    /// ("symbol unused" per `spec/05` §2.1) are both excluded — neither
    /// is a code length, so neither enters the histogram.
    ///
    /// [`active_symbol_count`]: PlaneLayout::active_symbol_count
    /// [`max_code_length`]: PlaneLayout::max_code_length
    /// [`min_code_length`]: PlaneLayout::min_code_length
    /// [`min_code_length_symbol_count`]: PlaneLayout::min_code_length_symbol_count
    /// [`kraft_numerator`]: PlaneLayout::kraft_numerator
    pub code_length_histogram: Vec<(u8, u32)>,
}

impl PlaneLayout {
    /// Total wire bytes occupied by this plane:
    /// `256 + 4 * num_slices + slice_data_total`.
    pub fn total_size(&self) -> usize {
        let slice_total = self
            .slices
            .last()
            .map(|s| s.end.saturating_sub(self.slice_data_start))
            .unwrap_or(0);
        256 + 4 * self.slices.len() + slice_total
    }

    /// Sum of slice bit-stream byte lengths (the `slice_data_total`
    /// from `spec/02` §5). Always a multiple of 4 for a well-formed
    /// plane (`spec/05` §4.1).
    pub fn slice_data_total(&self) -> usize {
        self.slices
            .last()
            .map(|s| s.end.saturating_sub(self.slice_data_start))
            .unwrap_or(0)
    }

    /// Total residual-symbol count across every slice of this plane.
    /// Equal to `width * height` for any well-formed
    /// [`PlaneLayout`] — the per-slice partitioning rule
    /// (`spec/02` §5.2) covers `[0, plane_height)` with no overlap
    /// and no gap, so `Σ slice.pixel_count == plane_width *
    /// plane_height`. Useful as a typed cross-check against the
    /// header-derived plane size before any Huffman pass runs.
    pub fn total_pixels(&self) -> u64 {
        self.slices.iter().map(|s| u64::from(s.pixel_count)).sum()
    }

    /// Number of bits in the unused-symbol set: `256 -
    /// active_symbol_count - 1` when [`is_single_symbol`] is true,
    /// `256 - active_symbol_count` otherwise. Mirrors the count of
    /// `code_length[s] == 255` sentinel entries in the descriptor
    /// per `spec/05` §2.1 (the "symbol unused, no code is assigned"
    /// bullet). Useful as a typed cross-check: an entropy-coding
    /// audit can confirm
    /// `active_symbol_count + unused_symbol_count + (single? 1 : 0)
    /// == 256` against the on-wire descriptor without a second pass
    /// over the byte slice.
    ///
    /// [`is_single_symbol`]: PlaneLayout::is_single_symbol
    pub fn unused_symbol_count(&self) -> u32 {
        let single = if self.is_single_symbol { 1 } else { 0 };
        256 - self.active_symbol_count - single
    }

    /// Integer Kraft numerator of this plane's descriptor: the
    /// `2^max_code_length`-scaled Kraft sum
    /// `Σ count · 2^(max_code_length - code_length)` over the
    /// [`code_length_histogram`] tiers (`spec/05` §2.2 step 3). Returns
    /// `0` for the empty histogram (`active_symbol_count == 0`).
    ///
    /// A canonical-Huffman descriptor satisfies Kraft **equality**
    /// (`spec/05` §2.2 step 3) iff this numerator equals the Kraft
    /// denominator `2^max_code_length` — i.e. iff
    /// `kraft_numerator() == 1 << max_code_length`. That is the same
    /// completeness condition `HuffmanTable::build` enforces (surfaced
    /// as `Error::KraftViolation` on failure, `spec/05` §2.2), available
    /// here decode-free from the on-wire descriptor alone.
    ///
    /// Every term `count · 2^(max - L)` is a non-negative integer. For
    /// realistic descriptors (`spec/05` §6.2 reports max code lengths of
    /// 8–9 from natural input, and a flat 256-symbol length-8 codebook
    /// caps a single tier at `256 · 2^7`), the whole sum fits a `u128`
    /// comfortably and the result is exact. The `spec/05` §7.2 *wire*
    /// upper bound is far larger — a descriptor byte may carry any code
    /// length in `1..=254`, so a hostile or corrupt descriptor can drive
    /// `max_code_length` up to 254, at which point a single term
    /// `2^(max - L)` already exceeds the 128-bit range. To stay
    /// panic-free on attacker-shaped bytes (the inspector is a
    /// decode-free byte-walk that does **not** reject a malformed
    /// descriptor), the accumulation is computed with checked shifts and
    /// additions and **saturates to [`u128::MAX`]** the moment a term or
    /// the running sum would overflow. A saturated return is therefore a
    /// sentinel for "numerator too large to represent" and never a
    /// legitimate Kraft-equality value (Kraft equality needs the
    /// numerator to equal `2^max_code_length`, which is itself
    /// unrepresentable once `max_code_length >= 128` — see
    /// [`is_kraft_complete`], which tests completeness exactly without
    /// materialising `2^max`). For any in-corpus codebook the value is
    /// the exact integer numerator.
    ///
    /// [`is_kraft_complete`]: PlaneLayout::is_kraft_complete
    pub fn kraft_numerator(&self) -> u128 {
        let max = self.max_code_length;
        let mut sum: u128 = 0;
        for &(len, count) in &self.code_length_histogram {
            // `len <= max` holds by construction (`max_code_length` is
            // the running max over the same active bytes the histogram
            // tiers), so `max - len` does not underflow; but `max - len`
            // can be up to 253, overflowing a 128-bit shift. Use a
            // checked shift + checked add and saturate on overflow.
            let shift = u32::from(max - len);
            let term = match u128::from(count).checked_shl(shift) {
                // `checked_shl` only masks the shift amount; an actual
                // value overflow (a 1-bit shifted past bit 127) is not
                // caught by it, so verify the shift amount is in range
                // *and* that the multiply did not drop high bits.
                Some(t) if shift < 128 && (t >> shift) == u128::from(count) => t,
                _ => return u128::MAX,
            };
            match sum.checked_add(term) {
                Some(s) => sum = s,
                None => return u128::MAX,
            }
        }
        sum
    }

    /// Decode-free predicate: does this plane's 256-byte Huffman
    /// descriptor form a **complete prefix code** per `spec/05` §2.2
    /// step 3?
    ///
    /// Three shapes the wire format permits (`spec/05` §§2.1, 2.2, 6.1)
    /// map to a `bool` as follows:
    ///
    /// - **Single-symbol path** ([`is_single_symbol`] `== true`,
    ///   `spec/05` §6.1) — `true`. The lone `code_length[s] == 0`
    ///   sentinel stands for a degenerate complete code: every pixel of
    ///   the plane decodes to symbol `s`, so the "prefix code" trivially
    ///   covers the whole alphabet the slice data uses. The histogram is
    ///   empty here, so the [`kraft_numerator`] arithmetic does not
    ///   apply; this arm is recognised by the flag.
    /// - **Active canonical codebook** ([`active_symbol_count`] `>= 1`,
    ///   non-empty histogram) — `true` iff Kraft **equality** holds:
    ///   `kraft_numerator() == 2^max_code_length` (the `2^max`-scaled
    ///   integer form of `Σ count · 2^-code_length == 1`, `spec/05`
    ///   §2.2 step 3). A descriptor whose lengths sum to **less** than a
    ///   full code (an *incomplete* tree, Kraft sum `< 1`) or to **more**
    ///   than one (an *over-subscribed* tree, Kraft sum `> 1`) returns
    ///   `false`. This is the same completeness condition
    ///   [`crate::huffman::HuffmanTable::build`] enforces as
    ///   `Error::KraftViolation` — surfaced here decode-free from the
    ///   on-wire descriptor alone, **before** any `HuffmanTable` is
    ///   stood up.
    /// - **Empty / all-`255`-unused descriptor** (no active byte, not
    ///   single-symbol) — `false`. No symbol carries a code, so no
    ///   prefix code exists; [`peek_frame`] surfaces this shape
    ///   decode-free even though `HuffmanTable::build` structurally
    ///   rejects it.
    ///
    /// `peek_frame` itself does **not** reject a Kraft-incomplete
    /// descriptor (it stays a byte-walk and never builds a Huffman
    /// table), so a container indexer that wants to confirm a frame's
    /// descriptors are decode-ready — without paying for a
    /// `HuffmanTable::build` per plane — can call this predicate on each
    /// [`PlaneLayout`]. The full decoder ([`crate::decode_frame`]) would
    /// reject any plane for which this returns `false` with
    /// `Error::KraftViolation` (or the single-symbol sentinel errors)
    /// at `HuffmanTable::build` time.
    ///
    /// [`is_single_symbol`]: PlaneLayout::is_single_symbol
    /// [`active_symbol_count`]: PlaneLayout::active_symbol_count
    /// [`kraft_numerator`]: PlaneLayout::kraft_numerator
    pub fn is_kraft_complete(&self) -> bool {
        if self.is_single_symbol {
            return true;
        }
        if self.code_length_histogram.is_empty() {
            return false;
        }
        // Test Kraft **equality** by the bottom-up binary-tree node merge
        // rather than `kraft_numerator() == 2^max`, which is
        // unrepresentable once `max_code_length >= 128` (`spec/05` §7.2
        // permits code lengths up to 254). Walk the length tiers from the
        // deepest (`max_code_length`) toward the root: at each depth the
        // running node count is the number of tree nodes that must be
        // accounted for at that level; two sibling nodes merge into one
        // parent at the next-shallower depth, so the count must be even
        // before each ascent (an odd count is an unpaired leaf — the code
        // is *incomplete*), and a count exceeding the slots available at
        // a depth is *over-subscribed*. The histogram is ascending by
        // length, so iterate it in reverse to descend the tiers from
        // `max` down to `min`. The code is complete iff exactly one node
        // (the root) remains after merging up past the shallowest tier.
        //
        // `nodes` is bounded by `active_symbol_count <= 256` at the
        // deepest tier and only ever halves-then-adds while ascending, so
        // it never exceeds 256 and the arithmetic is overflow-free for
        // any `max_code_length` in the `1..=254` wire range.
        let hist = &self.code_length_histogram;
        let mut nodes: u32 = 0;
        let mut depth = self.max_code_length;
        let mut idx = hist.len();
        loop {
            // Fold in every tier sitting exactly at `depth`.
            while idx > 0 && hist[idx - 1].0 == depth {
                nodes += hist[idx - 1].1;
                idx -= 1;
            }
            if depth == 0 {
                break;
            }
            // Ascend one level: pair siblings into parents. An odd count
            // leaves an unpaired leaf — not a complete code.
            if nodes % 2 != 0 {
                return false;
            }
            nodes /= 2;
            depth -= 1;
        }
        // After merging past depth 0 the tree is complete iff a single
        // root node remains. (All active tiers have `len >= 1`, so the
        // loop always performs at least one ascent before reaching
        // `depth == 0`.)
        nodes == 1
    }
}

/// Decode-free per-frame layout view of a chunk payload.
///
/// Produced by [`peek_frame`]. Carries the per-plane byte extents,
/// the trailing `frame_info` dword, and the predictor decoded from
/// bits 8..9 of `frame_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLayout {
    /// Per-plane wire extents in on-wire plane order (`spec/02` §3).
    pub planes: Vec<PlaneLayout>,
    /// Trailing 4-byte `frame_info` dword (`spec/02` §6). Bits 8..9
    /// carry the predictor; the other bits surface here verbatim for
    /// diagnostics.
    pub frame_info: u32,
    /// Predictor selected by `frame_info` bits 8..9 (`spec/02` §6.1).
    pub predictor: Predictor,
    /// Number of slices per plane (from `cfg`, surfaced here for
    /// convenience).
    pub num_slices: usize,
}

impl FrameLayout {
    /// Total compressed bit-stream byte count across all planes
    /// (`spec/02` §§2, 5). Excludes the 256-byte descriptors, the
    /// per-plane slice-end-offset tables, and the trailing
    /// `frame_info` dword.
    pub fn total_slice_data_bytes(&self) -> usize {
        self.planes.iter().map(|p| p.slice_data_total()).sum()
    }

    /// Total compressed bytes attributable to the codec wire format
    /// (i.e. the chunk payload length). Identity:
    /// `total_size == Σ plane_total_size + 4`. Useful as a cross-check
    /// against the chunk-payload byte count the demuxer hands us.
    pub fn total_size(&self) -> usize {
        self.planes.iter().map(|p| p.total_size()).sum::<usize>() + 4
    }

    /// Decode-free predicate: do **all** planes of this frame carry a
    /// complete prefix code per `spec/05` §2.2 step 3?
    ///
    /// Frame-level roll-up of [`PlaneLayout::is_kraft_complete`] —
    /// `true` iff every plane returns `true` (vacuously `true` for a
    /// zero-plane frame, which [`peek_frame`] never produces). A `false`
    /// here means at least one plane's 256-byte descriptor would be
    /// rejected by [`crate::huffman::HuffmanTable::build`]
    /// (`Error::KraftViolation` or a single-symbol sentinel error), so
    /// the full decoder ([`crate::decode_frame`]) would fail on this
    /// frame. Lets a container indexer gate "is this frame decode-ready?"
    /// on a single byte-walk without standing up a `HuffmanTable` per
    /// plane.
    pub fn all_planes_kraft_complete(&self) -> bool {
        self.planes.iter().all(|p| p.is_kraft_complete())
    }
}

/// Lightweight peek at the trailing 4-byte `frame_info` dword + the
/// predictor it selects.
///
/// This does NOT validate any per-plane structure — the caller MUST
/// already trust the chunk payload is at least 4 bytes long. Returns
/// `Error::MissingFrameInfo` if the chunk payload is shorter than 4
/// bytes.
///
/// `frame_info` bit layout per `spec/02` §6.1:
///
/// - bits 0..7: reserved (FFmpeg writes 0).
/// - bits 8..9: predictor mode (0 = none, 1 = left, 2 = gradient,
///   3 = median).
/// - bits 10..31: reserved.
pub fn peek_frame_info(chunk_payload: &[u8]) -> Result<(u32, Predictor)> {
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let n = chunk_payload.len();
    let frame_info = u32::from_le_bytes(chunk_payload[n - 4..n].try_into().unwrap());
    Ok((frame_info, Predictor::from_frame_info(frame_info)))
}

/// Decode-free walk over a chunk payload that returns the per-plane
/// + per-slice byte extents and the trailing `frame_info` dword.
///
/// Runs the same parse rules [`crate::decode_frame`] uses (descriptor
/// length, slice-end-offset monotonicity, slice-end word-alignment,
/// total-length identity), surfacing the same `Error` variants the
/// full decoder would, but never builds a `HuffmanTable` and never
/// allocates a residual buffer.
///
/// Complexity is `O(plane_count * num_slices)` — one pass over the
/// chunk payload's descriptor + end-offset tables, no per-pixel work.
///
/// Use cases:
///
/// - Container indexing / pre-decode statistics.
/// - Diagnostic tooling (which plane carries the most compressed
///   bytes; is plane k single-symbol; which slice is empty).
/// - Test harnesses pinning wire-format invariants.
pub fn peek_frame(cfg: &StreamConfig, chunk_payload: &[u8]) -> Result<FrameLayout> {
    let num_slices = cfg.num_slices();
    if num_slices == 0 {
        return Err(Error::InvalidSliceCount);
    }
    if chunk_payload.len() < 4 {
        return Err(Error::MissingFrameInfo);
    }
    let frame_info_off = chunk_payload.len() - 4;

    let mut offset = 0usize;
    let plane_count = cfg.fourcc.plane_count();
    let mut planes: Vec<PlaneLayout> = Vec::with_capacity(plane_count);

    for plane_idx in 0..plane_count {
        let (pw, ph) = cfg.fourcc.plane_dim(plane_idx, cfg.width, cfg.height);

        // 256-byte Huffman descriptor.
        let descriptor_start = offset;
        if offset + 256 > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: 256,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let descriptor = &chunk_payload[offset..offset + 256];
        offset += 256;

        // Single descriptor-byte fold computing five decode-free
        // typed primitives at once (round 21 single-symbol flag,
        // round 244 active-symbol count, round 250 max code length,
        // round 255 min code length, round 261 min-length symbol
        // count). The `1..=254` active range is `spec/05` §2.1; the
        // `0` sentinel is `spec/05` §6.1; the `255` sentinel is
        // `spec/05` §2.1 ("symbol unused, no code is assigned").
        //
        // The min tracker uses `u8::MAX` as the "no active byte seen
        // yet" sentinel — safe because that value is the
        // `255 == unused` sentinel that never enters the active-range
        // arm. We collapse the sentinel to `0` after the fold so the
        // public field's `0..=254` invariant holds: the only shapes
        // the loop exits with sentinel-as-min are (a) the §6.1
        // single-symbol case and (b) the degenerate all-unused
        // descriptor, both of which the field documents as `0`.
        //
        // The min-length symbol-count tracker is reset to 1 every time
        // a strictly smaller active byte is observed and incremented
        // each time the running min is re-seen — a single-pass
        // multiplicity count of the shortest-length tier per
        // `spec/05` §2.2 step 2. When `min_code_length` stays at the
        // `u8::MAX` sentinel (no active byte), the count collapses to
        // 0 alongside the min itself (matching the §6.1 / all-unused
        // shapes documented on the field).
        let mut zero_count = 0usize;
        let mut unused_count = 0usize;
        let mut max_code_length: u8 = 0;
        let mut min_code_length: u8 = u8::MAX;
        let mut min_code_length_symbol_count: u32 = 0;
        // Per-length-tier multiplicity over the active range `1..=254`
        // (`spec/05` §2.1) — index `L` carries the count of descriptor
        // entries equal to `L`. The `0` and `255` sentinels never index
        // into the active span. Compacted into the ascending
        // `code_length_histogram` list after the fold.
        let mut length_counts = [0u32; 256];
        for &b in descriptor {
            match b {
                0 => zero_count += 1,
                255 => unused_count += 1,
                // Active range 1..=254; track running max, min, the
                // shortest-tier multiplicity, and the full per-length
                // histogram in lockstep.
                _ => {
                    length_counts[b as usize] += 1;
                    if b > max_code_length {
                        max_code_length = b;
                    }
                    if b < min_code_length {
                        min_code_length = b;
                        min_code_length_symbol_count = 1;
                    } else if b == min_code_length {
                        min_code_length_symbol_count += 1;
                    }
                }
            }
        }
        // Compact the per-length counts into an ascending-by-length list
        // of `(code_length, count)` pairs, dropping zero-count tiers
        // (`spec/05` §2.2 step 2 groups symbols by length; absent
        // lengths carry no symbol). Empty when no active byte was seen.
        let mut code_length_histogram: Vec<(u8, u32)> = Vec::new();
        for (len, &count) in length_counts.iter().enumerate() {
            if count > 0 {
                code_length_histogram.push((len as u8, count));
            }
        }
        // Collapse the "no active byte seen" sentinel to the documented
        // `0` value (`spec/05` §6.1 single-symbol path + the
        // structurally-rejected all-`255` descriptor share this shape).
        if min_code_length == u8::MAX {
            min_code_length = 0;
            // Multiplicity also collapses to 0 — no active tier exists.
            min_code_length_symbol_count = 0;
        }
        // Single-symbol detection per spec/05 §6.1: exactly one entry
        // is 0 (the sentinel symbol) and every other entry is 255
        // (the sentinel "unused").
        let is_single_symbol = zero_count == 1 && unused_count == 255;
        // Active-symbol count per spec/05 §2.1: entries with value
        // in the range 1..=254 carry an explicit code length and
        // join the canonical-Huffman alphabet. The complement
        // (zero_count + unused_count) covers both sentinels;
        // descriptor.len() is constant 256, so the subtraction is
        // well-defined and produces a u32-friendly value in 0..=256.
        let active_symbol_count = (256 - zero_count - unused_count) as u32;

        // Slice-end-offsets table.
        let end_offsets_start = offset;
        let table_bytes = num_slices * 4;
        if offset + table_bytes > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: table_bytes,
                have: frame_info_off.saturating_sub(offset),
            });
        }
        let mut end_offsets = Vec::with_capacity(num_slices);
        for s in 0..num_slices {
            let v = u32::from_le_bytes(
                chunk_payload[offset + 4 * s..offset + 4 * s + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            end_offsets.push(v);
        }
        offset += table_bytes;

        // Monotonicity + word-alignment validation per spec/02 §5 +
        // spec/05 §4.1. Surfaces the same `Error` variant the full
        // decoder would.
        let mut prev = 0usize;
        for &v in &end_offsets {
            if v < prev {
                return Err(Error::NonMonotonicSliceOffsets);
            }
            if v % 4 != 0 {
                return Err(Error::SliceNotWordAligned(v));
            }
            prev = v;
        }
        let slice_data_total = *end_offsets.last().unwrap();
        let slice_data_start = offset;

        if offset + slice_data_total > frame_info_off {
            return Err(Error::ChunkTooShort {
                offset,
                needed: slice_data_total,
                have: frame_info_off.saturating_sub(offset),
            });
        }

        // Build per-slice absolute extents + decode-free row range
        // per `spec/02` §5.2:
        //   row_start[s] = floor((plane_height * s) / num_slices)
        //   row_end[s]   = floor((plane_height * (s + 1)) / num_slices)
        // The product `plane_height * num_slices` fits in u64 in every
        // reachable case (plane_height capped by `Hp <= 65535 *
        // chroma-step` per `spec/01` §4.4.1, num_slices `<= 256` per
        // `spec/02` §5.3 — the round-241 max product is well below
        // u64::MAX), but we widen to u64 explicitly so the typed
        // accessor is overflow-safe on the architectural upper bound.
        let mut slices: Vec<SliceLayout> = Vec::with_capacity(num_slices);
        let mut prev_rel = 0usize;
        let ph64 = u64::from(ph);
        let ns64 = num_slices as u64;
        let pw32 = pw;
        for (s_idx, &end_rel) in end_offsets.iter().enumerate() {
            let row_start = (ph64 * s_idx as u64) / ns64;
            let row_end = (ph64 * (s_idx as u64 + 1)) / ns64;
            let row_count = row_end - row_start;
            slices.push(SliceLayout {
                start: slice_data_start + prev_rel,
                end: slice_data_start + end_rel,
                row_start: row_start as u32,
                row_end: row_end as u32,
                pixel_count: row_count as u32 * pw32,
            });
            prev_rel = end_rel;
        }
        offset += slice_data_total;

        planes.push(PlaneLayout {
            plane_idx,
            width: pw,
            height: ph,
            descriptor_start,
            end_offsets_start,
            slice_data_start,
            slices,
            is_single_symbol,
            active_symbol_count,
            max_code_length,
            min_code_length,
            min_code_length_symbol_count,
            code_length_histogram,
        });
    }

    if offset != frame_info_off {
        return Err(Error::ChunkTooShort {
            offset,
            needed: frame_info_off - offset,
            have: 0,
        });
    }
    let frame_info = u32::from_le_bytes(
        chunk_payload[frame_info_off..frame_info_off + 4]
            .try_into()
            .unwrap(),
    );

    Ok(FrameLayout {
        planes,
        frame_info,
        predictor: Predictor::from_frame_info(frame_info),
        num_slices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{encode_frame, EncodedFrame, PlaneInput};
    use crate::fourcc::{Extradata, Fourcc};

    fn cfg_for(fc: Fourcc, w: u32, h: u32, slices: usize) -> StreamConfig {
        let extradata = Extradata::ffmpeg_for(fc, slices).unwrap();
        StreamConfig::new(fc, w, h, extradata).unwrap()
    }

    fn encoded_for(fc: Fourcc, w: u32, h: u32, slices: usize, pred: Predictor) -> Vec<u8> {
        let plane_count = fc.plane_count();
        let mut planes = Vec::with_capacity(plane_count);
        for idx in 0..plane_count {
            let (pw, ph) = fc.plane_dim(idx, w, h);
            // Mildly non-trivial content to avoid the single-symbol
            // collapse on every test; xorshift seeded by plane index.
            let mut state = 0x1234_5678u32 ^ (idx as u32).wrapping_mul(0x9E37_79B9);
            let mut samples = Vec::with_capacity((pw * ph) as usize);
            for _ in 0..(pw * ph) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                samples.push((state & 0xff) as u8);
            }
            planes.push(PlaneInput { samples });
        }
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: pred,
            num_slices: slices,
            planes,
        };
        encode_frame(&frame).unwrap()
    }

    #[test]
    fn peek_frame_info_recovers_predictor_from_trailing_dword() {
        for &pred in &[
            Predictor::None,
            Predictor::Left,
            Predictor::Gradient,
            Predictor::Median,
        ] {
            let bytes = encoded_for(Fourcc::Uly2, 16, 16, 1, pred);
            let (frame_info, recovered) = peek_frame_info(&bytes).unwrap();
            assert_eq!(recovered, pred);
            assert_eq!((frame_info >> 8) & 0x3, pred.as_frame_info_bits() >> 8);
        }
    }

    #[test]
    fn peek_frame_info_rejects_short_buffer() {
        for short in &[&[][..], &[1u8][..], &[1, 2, 3][..]] {
            let r = peek_frame_info(short);
            assert!(matches!(r, Err(Error::MissingFrameInfo)));
        }
    }

    #[test]
    fn peek_frame_layout_round_trips_every_fourcc_single_slice() {
        for &fc in &[
            Fourcc::Ulrg,
            Fourcc::Ulra,
            Fourcc::Uly0,
            Fourcc::Uly2,
            Fourcc::Uly4,
        ] {
            let (w, h) = (16, 16);
            let cfg = cfg_for(fc, w, h, 1);
            let bytes = encoded_for(fc, w, h, 1, Predictor::Left);
            let layout = peek_frame(&cfg, &bytes).unwrap();
            // Plane count matches FOURCC.
            assert_eq!(layout.planes.len(), fc.plane_count());
            // Total-size identity: Σ plane_total_size + 4 == payload len.
            assert_eq!(layout.total_size(), bytes.len());
            // num_slices matches.
            assert_eq!(layout.num_slices, 1);
            for (i, p) in layout.planes.iter().enumerate() {
                assert_eq!(p.plane_idx, i);
                let (pw, ph) = fc.plane_dim(i, w, h);
                assert_eq!(p.width, pw);
                assert_eq!(p.height, ph);
                assert_eq!(p.slices.len(), 1);
                // Slice extents are non-empty for these high-entropy planes.
                assert!(!p.slices[0].is_empty());
                // Slice ends on a 4-byte boundary.
                assert_eq!(p.slices[0].end.saturating_sub(p.slice_data_start) % 4, 0);
                // Wire-byte ordering: descriptor < end_offsets < slice_data.
                assert!(p.descriptor_start < p.end_offsets_start);
                assert!(p.end_offsets_start < p.slice_data_start);
                assert!(p.slice_data_start <= p.slices[0].start);
            }
        }
    }

    #[test]
    fn peek_frame_layout_round_trips_multi_slice() {
        let fc = Fourcc::Uly4;
        let (w, h) = (32, 32);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        assert_eq!(layout.num_slices, 4);
        assert_eq!(layout.predictor, Predictor::Gradient);
        for p in &layout.planes {
            assert_eq!(p.slices.len(), 4);
            // Slices are contiguous: end_n == start_{n+1}.
            for w in p.slices.windows(2) {
                assert_eq!(w[0].end, w[1].start);
            }
            // First slice starts at slice_data_start.
            assert_eq!(p.slices[0].start, p.slice_data_start);
        }
        // Total-size identity holds for multi-slice frames too.
        assert_eq!(layout.total_size(), bytes.len());
    }

    #[test]
    fn peek_frame_layout_matches_decode_frame_predictor_and_frame_info() {
        for &fc in &[Fourcc::Ulrg, Fourcc::Uly2, Fourcc::Uly4] {
            for &pred in &[
                Predictor::None,
                Predictor::Left,
                Predictor::Gradient,
                Predictor::Median,
            ] {
                let cfg = cfg_for(fc, 16, 16, 1);
                let bytes = encoded_for(fc, 16, 16, 1, pred);
                let layout = peek_frame(&cfg, &bytes).unwrap();
                let decoded = crate::decode_frame(&cfg, &bytes).unwrap();
                assert_eq!(layout.predictor, decoded.predictor);
                assert_eq!(layout.frame_info, decoded.frame_info);
            }
        }
    }

    #[test]
    fn peek_frame_detects_single_symbol_descriptor() {
        // A constant-content plane encodes via a single-symbol Huffman
        // descriptor (slice_data_total == 0) per spec/05 §6.1.
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![42u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![137u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![200u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            // Predictor::None preserves the constant content as a
            // constant residual stream — every residual byte is the
            // same value, hitting the single-symbol descriptor path
            // (`spec/05` §6.1). Other predictors emit a `+128` seed
            // residual at the per-slice row-0 col-0, breaking the
            // single-symbol invariant.
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        // Every plane is single-symbol -> empty slice + flag set.
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert_eq!(p.slice_data_total(), 0);
            assert!(p.slices[0].is_empty());
        }
        // Total payload is the bound `3 * (256 + 4 * 1) + 4 = 784` bytes
        // (spec/02 §2 + §4 + §5 + §6: 3 planes × (256-byte descriptor +
        // 1 × 4-byte end-offset entry + 0 slice bytes) + trailing 4-byte
        // frame_info dword).
        assert_eq!(layout.total_size(), 3 * (256 + 4) + 4);
    }

    #[test]
    fn peek_frame_rejects_missing_frame_info() {
        let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
        for short in &[&[][..], &[1u8, 2, 3][..]] {
            let r = peek_frame(&cfg, short);
            assert!(matches!(r, Err(Error::MissingFrameInfo)));
        }
    }

    #[test]
    fn peek_frame_rejects_chunk_too_short_for_descriptor() {
        let cfg = cfg_for(Fourcc::Uly2, 16, 16, 1);
        // Buffer is 16 bytes; 4 reserved for frame_info, leaving 12
        // bytes — fewer than the 256-byte descriptor of plane 0.
        let payload = vec![0u8; 16];
        let r = peek_frame(&cfg, &payload);
        assert!(matches!(r, Err(Error::ChunkTooShort { .. })));
    }

    #[test]
    fn peek_frame_rejects_non_monotonic_slice_offsets() {
        // Build a real encoded frame, then mutate one slice-end offset
        // backwards. peek_frame surfaces NonMonotonicSliceOffsets at
        // the same point the full decoder would.
        let fc = Fourcc::Uly4;
        let (w, h) = (32, 32);
        let cfg = cfg_for(fc, w, h, 4);
        let mut bytes = encoded_for(fc, w, h, 4, Predictor::Left);
        let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
        // Mutate the third slice-end offset to a value below the second.
        let off = p0.end_offsets_start + 2 * 4;
        bytes[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        let r = peek_frame(&cfg, &bytes);
        assert!(
            matches!(r, Err(Error::NonMonotonicSliceOffsets)),
            "got {r:?}"
        );
    }

    #[test]
    fn peek_frame_rejects_unaligned_slice_offset() {
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let mut bytes = encoded_for(fc, w, h, 1, Predictor::Left);
        let p0 = peek_frame(&cfg, &bytes).unwrap().planes[0].clone();
        // Bump the single end-offset by 1 — no longer a multiple of 4.
        let existing = u32::from_le_bytes(
            bytes[p0.end_offsets_start..p0.end_offsets_start + 4]
                .try_into()
                .unwrap(),
        );
        let bumped = (existing + 1).to_le_bytes();
        bytes[p0.end_offsets_start..p0.end_offsets_start + 4].copy_from_slice(&bumped);
        let r = peek_frame(&cfg, &bytes);
        assert!(matches!(r, Err(Error::SliceNotWordAligned(_))), "got {r:?}");
    }

    #[test]
    fn peek_frame_max_code_length_zero_on_single_symbol_planes() {
        // A constant-content plane drives `spec/05` §6.1 single-symbol
        // descriptors; the only non-`255` descriptor byte is the `0`
        // sentinel, so no entry falls in the active range `1..=254` and
        // the typed max is exactly 0.
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![10u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![77u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![222u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert_eq!(p.max_code_length, 0);
        }
    }

    #[test]
    fn peek_frame_max_code_length_matches_descriptor_byte_scan() {
        // On a multi-symbol Kraft codebook the typed accessor must
        // equal an independent rescan of the descriptor bytes via the
        // reported `descriptor_start` offset, filtered to the active
        // range `1..=254` per `spec/05` §2.1.
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
            let rescan: u8 = descriptor
                .iter()
                .copied()
                .filter(|&b| (1..=254).contains(&b))
                .max()
                .unwrap_or(0);
            assert_eq!(
                p.max_code_length, rescan,
                "plane {} max {} != rescan {}",
                p.plane_idx, p.max_code_length, rescan
            );
        }
    }

    #[test]
    fn peek_frame_min_code_length_zero_on_single_symbol_planes() {
        // A constant-content plane drives `spec/05` §6.1 single-symbol
        // descriptors; the only non-`255` descriptor byte is the `0`
        // sentinel, so no entry falls in the active range `1..=254` and
        // the typed min is exactly 0 (matches the round-250 max-side
        // `0`-collapse on the same path).
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![10u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![77u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![222u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert_eq!(p.min_code_length, 0);
        }
    }

    #[test]
    fn peek_frame_min_code_length_matches_descriptor_byte_scan() {
        // On a multi-symbol Kraft codebook the typed accessor must
        // equal an independent rescan of the descriptor bytes via the
        // reported `descriptor_start` offset, filtered to the active
        // range `1..=254` per `spec/05` §2.1.
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
            let rescan: u8 = descriptor
                .iter()
                .copied()
                .filter(|&b| (1..=254).contains(&b))
                .min()
                .unwrap_or(0);
            assert_eq!(
                p.min_code_length, rescan,
                "plane {} min {} != rescan {}",
                p.plane_idx, p.min_code_length, rescan
            );
        }
    }

    #[test]
    fn peek_frame_min_code_length_symbol_count_zero_on_single_symbol_planes() {
        // A constant-content plane drives `spec/05` §6.1; the only
        // non-`255` descriptor byte is the `0` sentinel, so no entry
        // sits in the active range `1..=254` and the typed
        // multiplicity collapses to 0 (matches the min / max
        // `0`-collapse on the same path).
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![10u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![77u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![222u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert_eq!(p.min_code_length_symbol_count, 0);
        }
    }

    #[test]
    fn peek_frame_min_code_length_symbol_count_matches_descriptor_byte_scan() {
        // On a multi-symbol Kraft codebook the typed multiplicity
        // must equal an independent rescan of the descriptor bytes via
        // the reported `descriptor_start` offset, counting entries
        // equal to the reported `min_code_length` over the active
        // range `1..=254` per `spec/05` §2.1.
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let descriptor = &bytes[p.descriptor_start..p.descriptor_start + 256];
            let rescan: u32 = if p.min_code_length == 0 {
                0
            } else {
                descriptor
                    .iter()
                    .copied()
                    .filter(|&b| b == p.min_code_length)
                    .count() as u32
            };
            assert_eq!(
                p.min_code_length_symbol_count, rescan,
                "plane {} count {} != rescan {}",
                p.plane_idx, p.min_code_length_symbol_count, rescan
            );
        }
    }

    #[test]
    fn peek_frame_code_length_histogram_empty_on_single_symbol_planes() {
        // A constant-content plane drives the `spec/05` §6.1
        // single-symbol path; no descriptor byte lands in the active
        // range `1..=254`, so the histogram is empty and the
        // projections collapse alongside it.
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let planes = vec![
            PlaneInput {
                samples: vec![10u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![77u8; (w * h) as usize],
            },
            PlaneInput {
                samples: vec![222u8; (w * h) as usize],
            },
        ];
        let frame = EncodedFrame {
            fourcc: fc,
            width: w,
            height: h,
            predictor: Predictor::None,
            num_slices: 1,
            planes,
        };
        let bytes = encode_frame(&frame).unwrap();
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(p.is_single_symbol);
            assert!(p.code_length_histogram.is_empty());
            assert_eq!(p.kraft_numerator(), 0);
        }
    }

    #[test]
    fn peek_frame_code_length_histogram_projects_scalar_accessors() {
        // On a multi-symbol Kraft codebook the histogram is the superset
        // the round 244 / 250 / 255 / 261 scalars project from, and a
        // Kraft-complete descriptor satisfies the integer Kraft equality
        // `kraft_numerator == 2^max_code_length` (`spec/05` §2.2 step 3).
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            let hist = &p.code_length_histogram;
            assert!(!hist.is_empty());
            // Strictly ascending by length, no zero-count tiers.
            for win in hist.windows(2) {
                assert!(win[0].0 < win[1].0);
            }
            assert!(hist.iter().all(|&(_, n)| n > 0));
            // Projections.
            let total: u32 = hist.iter().map(|&(_, n)| n).sum();
            assert_eq!(total, p.active_symbol_count);
            assert_eq!(hist.first().unwrap().0, p.min_code_length);
            assert_eq!(hist.first().unwrap().1, p.min_code_length_symbol_count);
            assert_eq!(hist.last().unwrap().0, p.max_code_length);
            // Kraft equality on the integer numerator.
            assert_eq!(p.kraft_numerator(), 1u128 << p.max_code_length);
        }
    }

    #[test]
    fn peek_frame_slice_data_total_matches_plane_compressed_bytes() {
        let fc = Fourcc::Uly2;
        let (w, h) = (64, 48);
        let cfg = cfg_for(fc, w, h, 4);
        let bytes = encoded_for(fc, w, h, 4, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        // total_slice_data_bytes equals chunk size minus descriptors,
        // end-offset tables, and the trailing frame_info dword.
        let overhead = layout.planes.len() * 256 + layout.planes.len() * 4 * layout.num_slices + 4;
        assert_eq!(layout.total_slice_data_bytes(), bytes.len() - overhead);
    }

    #[test]
    fn is_kraft_complete_true_on_encoder_output() {
        // Every plane the encoder emits carries a complete prefix code
        // (`spec/05` §2.2 step 3); the predicate folds the single-symbol
        // flag + the kraft_numerator identity into one bool.
        let fc = Fourcc::Uly4;
        let (w, h) = (48, 32);
        let cfg = cfg_for(fc, w, h, 2);
        let bytes = encoded_for(fc, w, h, 2, Predictor::Gradient);
        let layout = peek_frame(&cfg, &bytes).unwrap();
        for p in &layout.planes {
            assert!(p.is_kraft_complete(), "plane {} not complete", p.plane_idx);
        }
        assert!(layout.all_planes_kraft_complete());
    }

    #[test]
    fn is_kraft_complete_false_on_incomplete_descriptor() {
        // A single codelen-1 entry, everything else 255: Kraft sum 1/2 < 1
        // (`spec/05` §2.2 step 3). peek_frame accepts the byte-walk;
        // the predicate reports false (and decode_frame would
        // KraftViolation).
        let fc = Fourcc::Uly4;
        let (w, h) = (16, 16);
        let cfg = cfg_for(fc, w, h, 1);
        let mut bytes = encoded_for(fc, w, h, 1, Predictor::Left);
        for b in bytes[0..256].iter_mut() {
            *b = 255;
        }
        bytes[42] = 1;
        let layout = peek_frame(&cfg, &bytes).unwrap();
        assert!(!layout.planes[0].is_kraft_complete());
        assert!(!layout.all_planes_kraft_complete());
    }
}
