//! The compressed value stream, and how a value is read out of it.
//!
//! A tablebase file's payload is compressed with **Recursive Pairing**: the most frequent
//! adjacent pair of symbols is repeatedly replaced by a new symbol, so a single symbol at
//! the end expands into a whole run of values. On top of that sits a canonical Huffman
//! code, and on top of THAT a block index so a value can be found without decoding from the
//! start.
//!
//! # Offsets, not pointers
//!
//! Upstream memory-maps the file and stores raw pointers into it. rfish reads the file into
//! a `Vec<u8>` and stores OFFSETS, which is the same information with the aliasing question
//! removed — see `docs/05-tablebases.md` for why a mapping is not available here.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp` — `PairsData`, `set_sizes`, `set_groups`,
//! `decompress_pairs`.

use super::tables::{INDEX, TB_PIECES};

/// Flags a table carries. All but the last describe DTZ tables.
pub mod flag {
    /// The table stores only one side to move.
    pub const STM: u8 = 1;
    /// Values are remapped through a per-table lookup.
    pub const MAPPED: u8 = 2;
    /// A win's distance is already in plies rather than moves.
    pub const WIN_PLIES: u8 = 4;
    /// A loss's distance is already in plies.
    pub const LOSS_PLIES: u8 = 8;
    /// The remap table is 16 bits wide rather than 8.
    pub const WIDE: u8 = 16;
    /// Every position in the table holds the same value, stored in place of the minimum
    /// symbol length.
    pub const SINGLE_VALUE: u8 = 128;
}

/// Everything needed to read one (side, file) sub-table.
#[derive(Clone, Debug, Default)]
pub struct PairsData {
    pub flags: u8,
    pub max_sym_len: u8,
    /// Doubles as the stored value when [`flag::SINGLE_VALUE`] is set.
    pub min_sym_len: u8,
    pub blocks_num: u32,
    pub sizeof_block: usize,
    pub span: usize,
    /// Offset of the per-length lowest symbol table.
    pub lowest_sym: usize,
    /// Offset of the expansion tree: three bytes per symbol, two 12-bit children.
    pub btree: usize,
    /// Offset of the per-block value counts.
    pub block_length: usize,
    pub block_length_size: u32,
    /// Offset of the sparse index into `block_length`.
    pub sparse_index: usize,
    pub sparse_index_size: usize,
    /// Offset of the compressed payload.
    pub data: usize,
    /// `base64[l - min_sym_len]` is the lowest symbol of length `l`, right-padded to 64
    /// bits, so a symbol's length can be found by comparison alone.
    pub base64: Vec<u64>,
    /// The bitstream's top [`LEN_TAB_MAX_BITS`] bits -> the `base64` index they decode to,
    /// or [`NO_FAST_LEN`] where one bucket spans two lengths and the walk has to run.
    pub len_tab: Vec<u8>,
    /// 64 minus how many of the stream's top bits [`PairsData::len_tab`] is indexed by.
    pub len_tab_shift: u32,
    /// How many values each symbol expands into, minus one.
    pub symlen: Vec<u8>,
    /// The piece order the encoding groups by.
    pub pieces: [u8; TB_PIECES],
    /// Where each group's contribution starts in the index.
    pub group_idx: [u64; TB_PIECES + 1],
    /// How many pieces each group holds, zero-terminated.
    pub group_len: [i32; TB_PIECES + 1],
    /// Offsets into the DTZ remap table, one per WDL outcome.
    pub map_idx: [u16; 4],
}

/// How many of the bitstream's leading bits the length table is indexed by, at most.
///
/// The table is `1 << min(max_sym_len, this)` bytes, sized to the TABLE rather than to the
/// cap: a small alphabet gets a small table and touches fewer cache lines for it. Uncapped it
/// would want `1 << 63` entries.
const LEN_TAB_MAX_BITS: u32 = 12;

/// The length table's "this bucket spans two lengths; walk it". Cannot collide with a real
/// answer, which is an index into `base64` and so below 64.
const NO_FAST_LEN: u8 = 0xFF;

/// The `base64` index of the symbol at the head of `buf` — the walk the length table replaces.
///
/// Bounded by the table: the padding makes `base64` a total order for a WELL-FORMED payload,
/// so the search always stops. Corrupt bits need not, and running off the end of a `Vec` is
/// the panic the whole bounding pass in this file is about.
#[inline(always)]
fn walk_len(base64: &[u64], buf: u64) -> usize {
    let mut len = 0usize;
    while len + 1 < base64.len() && buf < base64[len] {
        len += 1;
    }
    len
}

/// Fill [`PairsData::len_tab`] from a finished `base64`.
///
/// **Every entry is VERIFIED rather than derived.** The argument for the table is that a code
/// no longer than K bits owns a whole number of buckets of the stream's top K bits, because
/// `base64` is right-padded to 64 — so one load answers what the walk searched for. Rather
/// than trust that argument against a file from a mirror, each bucket is walked at BOTH ends
/// and keeps its answer only when the two agree. A bucket that straddles a length boundary,
/// on any table however malformed, says `NO_FAST_LEN` and reaches the walk, so the decode is
/// exact by construction instead of by proof.
fn build_len_tab(d: &mut PairsData) {
    let bits = u32::from(d.max_sym_len).clamp(1, LEN_TAB_MAX_BITS);
    d.len_tab_shift = 64 - bits;
    d.len_tab = vec![NO_FAST_LEN; 1usize << bits];
    let within = (1u64 << d.len_tab_shift) - 1;
    for bucket in 0..d.len_tab.len() {
        let lo = (bucket as u64) << d.len_tab_shift;
        let len = walk_len(&d.base64, lo);
        if len == walk_len(&d.base64, lo | within) && len < usize::from(NO_FAST_LEN) {
            d.len_tab[bucket] = len as u8;
        }
    }
}

/// Read a little-endian `u16`.
///
/// **Bounded, and that is the point.** A table file is untrusted input from a mirror, and
/// every offset below is derived from the file's OWN header — so a corrupt header aims a
/// read past the end of the mapping. In safe Rust that is a bounds check, which panics and
/// takes the process with it: a denial of service reachable from a downloaded file. The
/// fuzzer in `fuzz.rs` found exactly that on its first run, at `u32_be`, reading offset 7824
/// of a 7824-byte table.
///
/// Out of range reads as zero rather than propagating: `decompress` returns a plain `i32`
/// with nowhere to report a failure, and a wrong verdict from a table the user corrupted is
/// a better outcome than a dead engine. For a VALID table every read here is in range, so
/// nothing about the shipped behaviour changes — `tb` still matches upstream on all 264
/// probes and the bench signature is untouched.
#[inline]
fn u16_le(b: &[u8], off: usize) -> u16 {
    b.get(off..off + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

/// Read a little-endian `u32`. Bounded for the reason [`u16_le`] gives.
#[inline]
fn u32_le(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a big-endian `u32`. The compressed payload is big-endian; everything around it is
/// little-endian, and mixing the two up produces a plausible wrong value rather than an
/// error. Bounded for the reason [`u16_le`] gives.
#[inline]
fn u32_be(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a big-endian `u64`. Bounded for the reason [`u16_le`] gives.
#[inline]
fn u64_be(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8).map_or(0, |s| {
        let mut v = 0u64;
        for &byte in s {
            v = (v << 8) | u64::from(byte);
        }
        v
    })
}

impl PairsData {
    /// A symbol's left child, or its stored value at a leaf.
    #[inline]
    fn left(&self, b: &[u8], sym: usize) -> usize {
        let o = self.btree + 3 * sym;
        b.get(o..o + 3).map_or(0, |n| (usize::from(n[1] & 0xF) << 8) | usize::from(n[0]))
    }

    /// A symbol's right child, or `0xFFF` at a leaf.
    #[inline]
    fn right(&self, b: &[u8], sym: usize) -> usize {
        let o = self.btree + 3 * sym;
        // A read past the end answers LEAF rather than zero: zero is a real symbol index, so
        // it would send the descent to symbol 0 and could cycle, where a leaf terminates.
        b.get(o..o + 3).map_or(0xFFF, |n| (usize::from(n[2]) << 4) | usize::from(n[1] >> 4))
    }

    /// A symbol's expanded length, with an out-of-range symbol reading as a leaf.
    ///
    /// The symbol index is decoded FROM the payload, so a corrupt block names symbols the
    /// tree does not have. Zero terminates the descent, which is what a refusal looks like
    /// from inside a function with no error channel.
    #[inline]
    fn symlen_of(&self, sym: usize) -> u8 {
        self.symlen.get(sym).copied().unwrap_or(0)
    }
}

/// Read the value at `idx` out of the compressed stream.
///
/// Three lookups stacked on each other: the sparse index finds a nearby block, the block
/// lengths walk to the exact one, the Huffman code finds the symbol at that offset, and the
/// pairing tree expands the symbol down to a single value.
#[must_use]
pub fn decompress(d: &PairsData, bytes: &[u8], idx: u64) -> i32 {
    // A table where every position has the same value stores it in place of the minimum
    // symbol length, and has no payload at all.
    if d.flags & flag::SINGLE_VALUE != 0 {
        return i32::from(d.min_sym_len);
    }

    // The sparse index records, for every `span` values, which block holds it and at what
    // offset. Interpolating from the nearest entry leaves a small correction to walk.
    let k = (idx / d.span as u64) as usize;
    let mut block = u32_le(bytes, d.sparse_index + 6 * k) as usize;
    let mut offset = i64::from(u16_le(bytes, d.sparse_index + 6 * k + 4));
    offset += idx as i64 % d.span as i64 - (d.span / 2) as i64;

    // Walk to the block that actually contains the value.
    //
    // Both walks are BOUNDED by the block count, which upstream leaves implicit. Upstream can:
    // its interpolation is exact for a table its own writer produced, so the correction is a
    // step or two. A corrupt sparse index puts the walk anywhere -- backwards past block zero,
    // or forwards forever once a truncated read answers a length of zero -- and neither is a
    // memory error here, only a subtraction overflow and an infinite loop. Clamping ends the
    // walk at the edge and lets the probe return a wrong verdict for a file the user broke.
    let last_block = d.block_length_size.saturating_sub(1) as usize;
    block = block.min(last_block);
    while offset < 0 && block > 0 {
        block -= 1;
        offset += i64::from(u16_le(bytes, d.block_length + 2 * block)) + 1;
    }
    while offset > i64::from(u16_le(bytes, d.block_length + 2 * block)) && block < last_block {
        offset -= i64::from(u16_le(bytes, d.block_length + 2 * block)) + 1;
        block += 1;
    }

    // Decode symbols from the head of the block until the offset falls inside one.
    let mut ptr = d.data + block * d.sizeof_block;
    let mut buf = u64_be(bytes, ptr);
    ptr += 8;
    let mut buf_size = 64i32;
    let mut sym;

    loop {
        // Every symbol of a given length is a consecutive integer, so the length is found
        // by comparing the padded buffer against the lowest symbol of each length — and for
        // every bucket whose whole span decodes to one length, ONE LOAD answers it. The walk
        // this replaces was 1,648,117,166 instructions on the probing workload, 15.9% of it
        // and the largest single line in the reader; it is a data-dependent loop, so it cost
        // the branch predictor as well as the pipeline.
        let mut len = usize::from(
            d.len_tab.get((buf >> d.len_tab_shift) as usize).copied().unwrap_or(NO_FAST_LEN),
        );
        if len == usize::from(NO_FAST_LEN) {
            len = walk_len(&d.base64, buf);
        }
        let shift = 64usize.saturating_sub(len + usize::from(d.min_sym_len));
        sym = (buf.wrapping_sub(d.base64[len]) >> shift) as usize;
        sym += usize::from(u16_le(bytes, d.lowest_sym + 2 * len));

        if offset < i64::from(d.symlen_of(sym)) + 1 {
            break;
        }
        offset -= i64::from(d.symlen_of(sym)) + 1;
        let consumed = len + usize::from(d.min_sym_len);
        // The bit window, bounded. A symbol is at most 64 bits wide in a real table, so
        // neither shift here can reach the width of the type -- but `consumed` is computed
        // from the file and `buf_size` follows it, so a corrupt payload drives both past 64
        // and a shift that wide is a panic. Shifting a `u64` out entirely IS zero, so the
        // saturating answer is also the arithmetically right one.
        buf = buf.checked_shl(consumed as u32).unwrap_or(0);
        buf_size -= consumed as i32;

        if buf_size <= 32 {
            buf_size += 32;
            let shift = 64 - buf_size;
            if (0..64).contains(&shift) {
                buf |= u64::from(u32_be(bytes, ptr)) << shift;
            }
            ptr += 4;
        }
    }

    // Expand the symbol. Recursive Pairing makes a symbol's two children adjacent in the
    // value sequence, so the offset chooses a side and the search descends.
    //
    // The step count is bounded by the number of symbols. A valid tree is acyclic, so the
    // descent visits each symbol at most once and the bound never binds; a corrupt btree can
    // name a symbol as its own ancestor, and then only the bound ends the loop.
    let mut steps = d.symlen.len() + 1;
    while d.symlen_of(sym) != 0 && steps > 0 {
        steps -= 1;
        let l = d.left(bytes, sym);
        if offset < i64::from(d.symlen_of(l)) + 1 {
            sym = l;
        } else {
            offset -= i64::from(d.symlen_of(l)) + 1;
            sym = d.right(bytes, sym);
        }
    }
    d.left(bytes, sym) as i32
}

/// How many values a symbol expands into, minus one.
///
/// Written iteratively over an explicit stack rather than recursively: the pairing tree can
/// be thousands deep on a large table, and a recursive walk overflows the thread stack.
fn compute_symlen(d: &mut PairsData, bytes: &[u8], root: usize, visited: &mut [bool]) -> bool {
    let mut stack = vec![(root, false)];
    while let Some((s, expanded)) = stack.pop() {
        if expanded {
            let sr = d.right(bytes, s);
            let sl = d.left(bytes, s);
            d.symlen[s] = d.symlen[sl].wrapping_add(d.symlen[sr]).wrapping_add(1);
            continue;
        }
        if visited[s] {
            continue;
        }
        visited[s] = true;
        let sr = d.right(bytes, s);
        if sr == 0xFFF {
            // A leaf: the left field holds the value, and the symbol is worth one value.
            d.symlen[s] = 0;
            continue;
        }
        let sl = d.left(bytes, s);
        // A child outside the symbol table means the btree is not a btree. REFUSE, rather
        // than clamp: this is parse time, the caller has an error channel, and a refused
        // table is probed as if absent -- which is a correct answer, where a verdict decoded
        // from a tree that does not close is a wrong one presented as fact.
        if sl >= d.symlen.len() || sr >= d.symlen.len() {
            return false;
        }
        // Re-push this symbol to be summed once both children are known.
        stack.push((s, true));
        if !visited[sr] {
            stack.push((sr, false));
        }
        if !visited[sl] {
            stack.push((sl, false));
        }
    }
    true
}

/// Read one sub-table's sizes and symbol tables, returning the offset just past them.
///
/// # Errors
/// Returns `None` when the file ends inside the structure.
pub fn set_sizes(d: &mut PairsData, bytes: &[u8], mut off: usize) -> Option<usize> {
    d.flags = *bytes.get(off)?;
    off += 1;

    if d.flags & flag::SINGLE_VALUE != 0 {
        d.blocks_num = 0;
        d.block_length_size = 0;
        d.span = 0;
        d.sparse_index_size = 0;
        d.min_sym_len = *bytes.get(off)?;
        return Some(off + 1);
    }

    // The last live group index is the table's total size, which is what the sparse index
    // is sized against.
    let terminator = d.group_len.iter().position(|&l| l == 0).unwrap_or(TB_PIECES);
    let tb_size = d.group_idx[terminator];

    // Both widths are stored as a shift, so a byte past 63 is not a large table -- it is a
    // shift wider than the type, which panics under the gate profile's overflow checks and
    // silently masks in release. Neither is a table; refuse the file.
    let block_shift = *bytes.get(off)?;
    off += 1;
    let span_shift = *bytes.get(off)?;
    off += 1;
    if block_shift >= 32 || span_shift >= 32 {
        return None;
    }
    d.sizeof_block = 1usize << block_shift;
    d.span = 1usize << span_shift;
    d.sparse_index_size = tb_size.div_ceil(d.span as u64) as usize;
    let padding = u32::from(*bytes.get(off)?);
    off += 1;
    d.blocks_num = u32_le(bytes, off);
    off += 4;
    // Padded so a sparse-index entry can never point past the end of the block lengths.
    d.block_length_size = d.blocks_num + padding;
    d.max_sym_len = *bytes.get(off)?;
    off += 1;
    d.min_sym_len = *bytes.get(off)?;
    off += 1;
    d.lowest_sym = off;

    // A maximum below the minimum underflows the length, and the code table it would size is
    // meaningless anyway.
    if d.max_sym_len < d.min_sym_len {
        return None;
    }
    let n = usize::from(d.max_sym_len) - usize::from(d.min_sym_len) + 1;
    d.base64 = vec![0u64; n];

    // The canonical code is ordered so a longer symbol has a lower numeric value. Building
    // the table from the top down and then right-padding each entry gives the property the
    // decoder relies on: `base64[i-1] >= s64 >= base64[i]` for any symbol of length i.
    for i in (0..n.saturating_sub(1)).rev() {
        let lo_i = u64::from(u16_le(bytes, d.lowest_sym + 2 * i));
        let lo_i1 = u64::from(u16_le(bytes, d.lowest_sym + 2 * (i + 1)));
        // Wrapping, because upstream's expression is unsigned and so wraps: for a valid table
        // `lo_i >= lo_i1` and neither side wraps at all, but a corrupt table inverts the pair
        // and a bare `-` would trap under the gate profile's overflow checks.
        d.base64[i] = d.base64[i + 1].wrapping_add(lo_i).wrapping_sub(lo_i1) / 2;
    }
    for i in 0..n {
        // `i + min_sym_len` is a symbol length in bits: at least one and at most 64 in a real
        // table. A corrupt header can claim zero or more than 64, and either end makes the
        // shift as wide as the type or wider -- undefined in C++, a panic here.
        let shift = 64usize.checked_sub(i + usize::from(d.min_sym_len)).filter(|&s| s < 64)?;
        d.base64[i] <<= shift;
    }
    build_len_tab(d);

    off += n * 2;
    let sym_count = usize::from(u16_le(bytes, off));
    off += 2;
    d.symlen = vec![0u8; sym_count];
    d.btree = off;

    let mut visited = vec![false; sym_count];
    for sym in 0..sym_count {
        if !visited[sym] && !compute_symlen(d, bytes, sym, &mut visited) {
            return None;
        }
    }

    Some(off + sym_count * 3 + (sym_count & 1))
}

/// Work out which pieces are encoded together, and what each group contributes to the
/// index.
///
/// A group is a run of identical pieces, except for the leading group: without pawns it is
/// the first three pieces (or the two kings when there is no unique piece besides them),
/// and with pawns the pawns come first.
pub fn set_groups(
    d: &mut PairsData,
    piece_count: usize,
    has_pawns: bool,
    has_unique_pieces: bool,
    pawn_count: [u8; 2],
    order: [i32; 2],
    file: usize,
) {
    let t = &*INDEX;
    let mut n = 0usize;
    let mut first_len: i32 = if has_pawns {
        0
    } else if has_unique_pieces {
        3
    } else {
        2
    };
    d.group_len[0] = 1;

    for i in 1..piece_count {
        first_len -= 1;
        if first_len > 0 || d.pieces[i] == d.pieces[i - 1] {
            d.group_len[n] += 1;
        } else {
            n += 1;
            d.group_len[n] = 1;
        }
    }
    n += 1;
    d.group_len[n] = 0;

    // Each group's multiplier is the number of ways the groups AFTER it can be placed, so
    // the whole position encodes as a mixed-radix number. The order the groups appear in is
    // a per-table parameter, which is why it is read from the file rather than assumed.
    let pp = has_pawns && pawn_count[1] != 0;
    let mut next = if pp { 2 } else { 1 };
    let mut free_squares = 64 - d.group_len[0] - if pp { d.group_len[1] } else { 0 };
    let mut idx: u64 = 1;

    let mut k = 0i32;
    while next < n || k == order[0] || k == order[1] {
        if k == order[0] {
            d.group_idx[0] = idx;
            idx *= if has_pawns {
                t.lead_pawns_size[d.group_len[0] as usize][file] as u64
            } else if has_unique_pieces {
                31332
            } else {
                462
            };
        } else if k == order[1] {
            d.group_idx[1] = idx;
            idx *= t.binomial[d.group_len[1] as usize][48 - d.group_len[0] as usize] as u64;
        } else {
            d.group_idx[next] = idx;
            idx *= t.binomial[d.group_len[next] as usize][free_squares as usize] as u64;
            free_squares -= d.group_len[next];
            next += 1;
        }
        k += 1;
    }
    d.group_idx[n] = idx;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_bits_are_the_ones_the_format_defines() {
        // Not tunables: each is a bit position in the file's first payload byte.
        assert_eq!(flag::STM, 1);
        assert_eq!(flag::MAPPED, 2);
        assert_eq!(flag::WIN_PLIES, 4);
        assert_eq!(flag::LOSS_PLIES, 8);
        assert_eq!(flag::WIDE, 16);
        assert_eq!(flag::SINGLE_VALUE, 128);
    }

    #[test]
    fn a_single_value_table_needs_no_payload() {
        let mut d = PairsData { flags: flag::SINGLE_VALUE, min_sym_len: 3, ..PairsData::default() };
        assert_eq!(decompress(&d, &[], 0), 3);
        assert_eq!(decompress(&d, &[], 12345), 3);
        d.min_sym_len = 0;
        assert_eq!(decompress(&d, &[], 0), 0);
    }

    #[test]
    fn the_two_endiannesses_are_not_interchangeable() {
        // The payload is big-endian and everything around it little-endian; reading one as
        // the other yields a plausible wrong number rather than a failure.
        let b = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(u32_le(&b, 0), 0x0403_0201);
        assert_eq!(u32_be(&b, 0), 0x0102_0304);
        assert_eq!(u64_be(&b, 0), 0x0102_0304_0506_0708);
        assert_eq!(u16_le(&b, 0), 0x0201);
    }

    #[test]
    fn a_symbol_tree_node_splits_into_two_twelve_bit_children() {
        // lr = [0x34, 0x12, 0x56] -> left = 0x234, right = 0x561.
        let d = PairsData { btree: 0, ..PairsData::default() };
        let b = [0x34u8, 0x12, 0x56];
        assert_eq!(d.left(&b, 0), 0x234);
        assert_eq!(d.right(&b, 0), 0x561);
    }

    /// A leaf is marked by a right child of `0xFFF`, and expands into exactly one value.
    #[test]
    fn a_leaf_symbol_expands_into_one_value() {
        let mut d = PairsData { btree: 0, symlen: vec![0; 1], ..PairsData::default() };
        let b = [0x07u8, 0xF0, 0xFF]; // left = 7, right = 0xFFF
        assert_eq!(d.right(&b, 0), 0xFFF);
        let mut visited = vec![false; 1];
        compute_symlen(&mut d, &b, 0, &mut visited);
        assert_eq!(d.symlen[0], 0);
    }

    /// A symbol expanding into two leaves is worth two values, so `symlen` is 1.
    #[test]
    fn a_pair_symbol_sums_its_children() {
        // Symbol 0 -> (1, 2); symbols 1 and 2 are leaves.
        let b = [
            0x01, 0x20, 0x00, // sym 0: left = 1, right = 2
            0x09, 0xF0, 0xFF, // sym 1: leaf, value 9
            0x0A, 0xF0, 0xFF, // sym 2: leaf, value 10
        ];
        let mut d = PairsData { btree: 0, symlen: vec![0; 3], ..PairsData::default() };
        let mut visited = vec![false; 3];
        compute_symlen(&mut d, &b, 0, &mut visited);
        assert_eq!(d.symlen[1], 0);
        assert_eq!(d.symlen[2], 0);
        assert_eq!(d.symlen[0], 1, "two values means symlen 1");
    }

    /// The pairing tree can be thousands deep; the expansion must not recurse.
    #[test]
    fn a_deep_symbol_chain_does_not_overflow_the_stack() {
        const N: usize = 20_000;
        let mut b = Vec::with_capacity(N * 3);
        for s in 0..N {
            if s + 1 < N {
                // Left child is the next symbol, right child is a shared leaf at N-1.
                let l = s + 1;
                let r = N - 1;
                b.push((l & 0xFF) as u8);
                b.push((((l >> 8) & 0xF) as u8) | (((r & 0xF) as u8) << 4));
                b.push(((r >> 4) & 0xFF) as u8);
            } else {
                b.extend_from_slice(&[0x00, 0xF0, 0xFF]);
            }
        }
        let mut d = PairsData { btree: 0, symlen: vec![0; N], ..PairsData::default() };
        let mut visited = vec![false; N];
        compute_symlen(&mut d, &b, 0, &mut visited);
        assert_eq!(d.symlen[N - 1], 0);
    }
}
