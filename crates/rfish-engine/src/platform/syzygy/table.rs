//! One tablebase file: its identity, its layout, and its lazy load.
//!
//! A table is identified by a MATERIAL KEY — the Zobrist hash of the piece counts, with no
//! square information — because that is what decides which file answers a position. Two
//! keys per table: one for each side being the stronger, since a file stores only
//! `KRvK` and never `KvKR`.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp` — `TBTable`, `set`, `set_dtz_map`.

use std::sync::{Mutex, OnceLock};

use crate::board::types::{Color, Key, Piece, PieceType, Square};
use crate::board::zobrist;

use super::pairs::{PairsData, flag, set_groups, set_sizes};

/// Which kind of table a file is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TbType {
    /// Win, draw or loss. Two sides to move, both stored.
    Wdl,
    /// Distance to zeroing. One side to move only.
    Dtz,
}

impl TbType {
    /// The file extension.
    #[must_use]
    pub const fn ext(self) -> &'static str {
        match self {
            TbType::Wdl => "rtbw",
            TbType::Dtz => "rtbz",
        }
    }

    /// The four magic bytes the file must begin with.
    #[must_use]
    pub const fn magic(self) -> [u8; 4] {
        match self {
            TbType::Dtz => [0xD7, 0x66, 0x0C, 0xA5],
            TbType::Wdl => [0x71, 0xE8, 0x23, 0x5D],
        }
    }

    /// How many sides to move the file stores.
    #[must_use]
    pub const fn sides(self) -> usize {
        match self {
            TbType::Wdl => 2,
            TbType::Dtz => 1,
        }
    }
}

/// The material key of a piece multiset, as `Position` computes it.
///
/// Hashing the COUNTS rather than the squares is what makes it a table identity: every
/// position with these pieces, wherever they stand, maps to the same file.
#[must_use]
pub fn material_key(counts: &[[i32; 7]; 2]) -> Key {
    let mut key = 0;
    for c in Color::ALL {
        for pt in PieceType::REAL {
            let pc = Piece::new(c, pt);
            for n in 0..counts[c.index()][pt.index()] {
                key ^= zobrist::psq(pc, Square::new(8 + n as usize));
            }
        }
    }
    key
}

/// Parse a table code such as `KRPvKR` into per-colour piece counts.
///
/// Returns `None` for anything that is not a table name — which is most of what a tablebase
/// directory contains.
#[must_use]
pub fn parse_code(code: &str) -> Option<[[i32; 7]; 2]> {
    let (white, black) = code.split_once('v')?;
    if white.is_empty() || black.is_empty() {
        return None;
    }
    let mut counts = [[0i32; 7]; 2];
    for (side, text) in [(0usize, white), (1, black)] {
        if !text.starts_with('K') {
            return None;
        }
        for ch in text.bytes() {
            let pt = PieceType::from_char(ch)?;
            counts[side][pt.index()] += 1;
        }
    }
    Some(counts)
}

/// One table's identity plus, once loaded, its layout.
pub struct TbTable {
    pub kind: TbType,
    /// The material key with the stronger side as White.
    pub key: Key,
    /// The same with the colours swapped.
    pub key2: Key,
    pub piece_count: usize,
    pub has_pawns: bool,
    /// True when some non-king piece appears exactly once, which lets the leading group be
    /// three pieces rather than the king pair.
    pub has_unique_pieces: bool,
    /// Pawns of the leading colour, then of the other.
    pub pawn_count: [u8; 2],
    /// Where the file lives.
    pub path: std::path::PathBuf,
    /// The file's bytes and parsed layout, read on first probe.
    loaded: OnceLock<Option<Loaded>>,
    /// Serialises the one-time load, so two threads probing the same table at once do not
    /// both read the file.
    load_lock: Mutex<()>,
}

impl std::fmt::Debug for TbTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TbTable")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("loaded", &self.loaded.get().is_some())
            .finish_non_exhaustive()
    }
}

/// A table's contents, once read.
pub struct Loaded {
    pub bytes: Vec<u8>,
    /// `items[side][file]`. A pawnless table uses file 0 only.
    pub items: Vec<Vec<PairsData>>,
    /// Offset of the DTZ remap table.
    pub map: usize,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded").field("bytes", &self.bytes.len()).finish_non_exhaustive()
    }
}

impl TbTable {
    /// Build a table's identity from its code and path. Reads nothing.
    #[must_use]
    pub fn new(kind: TbType, code: &str, path: std::path::PathBuf) -> Option<TbTable> {
        let counts = parse_code(code)?;
        let piece_count: i32 = counts.iter().flatten().sum();

        let mut has_unique_pieces = false;
        for side in &counts {
            for pt in [1usize, 2, 3, 4, 5] {
                if side[pt] == 1 {
                    has_unique_pieces = true;
                }
            }
        }
        let has_pawns = counts[0][1] + counts[1][1] > 0;

        // The leading colour is the side with FEWER pawns, because that compresses better.
        // With no black pawns at all, white leads.
        let lead_white = counts[1][1] == 0 || (counts[0][1] != 0 && counts[1][1] >= counts[0][1]);
        let pawn_count = if lead_white {
            [counts[0][1] as u8, counts[1][1] as u8]
        } else {
            [counts[1][1] as u8, counts[0][1] as u8]
        };

        let key = material_key(&counts);
        let swapped = [counts[1], counts[0]];
        let key2 = material_key(&swapped);

        Some(TbTable {
            kind,
            key,
            key2,
            piece_count: piece_count as usize,
            has_pawns,
            has_unique_pieces,
            pawn_count,
            path,
            loaded: OnceLock::new(),
            load_lock: Mutex::new(()),
        })
    }

    /// The table's contents, reading the file on the first call.
    ///
    /// Returns `None` when the file is missing or corrupt — a probe then reports failure
    /// and the search carries on without tablebases, which is the only safe response to a
    /// table nobody can read.
    pub fn loaded(&self) -> Option<&Loaded> {
        if let Some(slot) = self.loaded.get() {
            return slot.as_ref();
        }
        let _guard = self.load_lock.lock().ok()?;
        let _ = self.loaded.set(self.read());
        self.loaded.get().and_then(Option::as_ref)
    }

    /// Read and parse the file.
    fn read(&self) -> Option<Loaded> {
        let bytes = std::fs::read(&self.path).ok()?;
        if bytes.len() < 5 || bytes[..4] != self.kind.magic() {
            return None;
        }
        self.parse(bytes)
    }

    /// Lay out the sub-tables over the file's bytes.
    ///
    /// The order is the format's and every step consumes exactly what the previous one
    /// sized — a single miscounted byte shifts everything after it and produces plausible
    /// wrong values rather than a failure.
    fn parse(&self, bytes: Vec<u8>) -> Option<Loaded> {
        /// The first payload byte says whether the file stores both colour assignments,
        /// and whether the position has pawns. Both must agree with what the code implied.
        const SPLIT: u8 = 1;
        const HAS_PAWNS: u8 = 2;

        let mut off = 4; // past the magic
        let head = *bytes.get(off)?;
        if (head & HAS_PAWNS != 0) != self.has_pawns {
            return None;
        }
        if (head & SPLIT != 0) != (self.key != self.key2) {
            return None;
        }
        off += 1;

        let sides = if self.kind.sides() == 2 && self.key != self.key2 { 2 } else { 1 };
        let max_file = if self.has_pawns { 3 } else { 0 };
        let pp = self.has_pawns && self.pawn_count[1] != 0;

        let mut items: Vec<Vec<PairsData>> =
            (0..sides).map(|_| (0..=max_file).map(|_| PairsData::default()).collect()).collect();

        for f in 0..=max_file {
            let b0 = *bytes.get(off)?;
            let b1 = if pp { *bytes.get(off + 1)? } else { 0 };
            let order = [
                [i32::from(b0 & 0xF), if pp { i32::from(b1 & 0xF) } else { 0xF }],
                [i32::from(b0 >> 4), if pp { i32::from(b1 >> 4) } else { 0xF }],
            ];
            off += 1 + usize::from(pp);

            for k in 0..self.piece_count {
                let byte = *bytes.get(off)?;
                for (i, side) in items.iter_mut().enumerate() {
                    side[f].pieces[k] = if i != 0 { byte >> 4 } else { byte & 0xF };
                }
                off += 1;
            }
            // A pawnful table leads with a PAWN in both colour views -- the index computation
            // reads `pieces[0]` as the lead pawn's piece code and the whole file split is
            // built on it. Upstream never checks, because its own writer produced the file;
            // a corrupt one names something else, and the probe would then index a pawn table
            // by a knight. Refuse it here, where there is still an error channel to refuse
            // through, so the invariant the prober asserts on is one the parse established.
            if self.has_pawns
                && items.iter().any(|side| side[f].pieces[0] & 0x7 != PieceType::Pawn.index() as u8)
            {
                return None;
            }
            for (i, side) in items.iter_mut().enumerate() {
                set_groups(
                    &mut side[f],
                    self.piece_count,
                    self.has_pawns,
                    self.has_unique_pieces,
                    self.pawn_count,
                    order[i],
                    f,
                );
            }
        }

        off += off & 1; // word alignment

        for f in 0..=max_file {
            for side in &mut items {
                off = set_sizes(&mut side[f], &bytes, off)?;
            }
        }

        let map = off;
        if self.kind == TbType::Dtz {
            off = set_dtz_map(&mut items, &bytes, off, max_file)?;
        }

        for f in 0..=max_file {
            for side in &mut items {
                side[f].sparse_index = off;
                off += side[f].sparse_index_size * 6;
            }
        }
        for f in 0..=max_file {
            for side in &mut items {
                side[f].block_length = off;
                off += side[f].block_length_size as usize * 2;
            }
        }
        for f in 0..=max_file {
            for side in &mut items {
                // The payload of each sub-table is 64-byte aligned.
                off = (off + 0x3F) & !0x3F;
                side[f].data = off;
                off += side[f].blocks_num as usize * side[f].sizeof_block;
            }
        }
        if off > bytes.len() {
            return None;
        }

        Some(Loaded { bytes, items, map })
    }

    /// The sub-table for a side to move and a leading-pawn file.
    #[must_use]
    pub fn pairs<'a>(&self, loaded: &'a Loaded, stm: usize, file: usize) -> &'a PairsData {
        let side = stm % loaded.items.len();
        let f = if self.has_pawns { file } else { 0 };
        &loaded.items[side][f]
    }
}

/// Lay out the DTZ value remap tables, returning the offset just past them.
///
/// DTZ files store a compact code per position and a per-outcome lookup that turns it into a
/// distance. The four entries are the four WDL outcomes, and their widths differ per table.
fn set_dtz_map(
    items: &mut [Vec<PairsData>],
    bytes: &[u8],
    mut off: usize,
    max_file: usize,
) -> Option<usize> {
    let base = off;
    for entry in items[0].iter_mut().take(max_file + 1) {
        let flags = entry.flags;
        if flags & flag::MAPPED == 0 {
            continue;
        }
        if flags & flag::WIDE != 0 {
            off += off & 1; // word alignment; a table can mix widths
            for slot in &mut entry.map_idx {
                *slot = u16::try_from((off - base) / 2 + 1).ok()?;
                let n = u16::from_le_bytes([*bytes.get(off)?, *bytes.get(off + 1)?]);
                off += 2 * usize::from(n) + 2;
            }
        } else {
            for slot in &mut entry.map_idx {
                *slot = u16::try_from(off - base + 1).ok()?;
                off += usize::from(*bytes.get(off)?) + 1;
            }
        }
    }
    Some(off + (off & 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_code_parses_into_piece_counts() {
        let c = parse_code("KRPvKR").expect("a real code");
        assert_eq!(c[0][PieceType::King.index()], 1);
        assert_eq!(c[0][PieceType::Rook.index()], 1);
        assert_eq!(c[0][PieceType::Pawn.index()], 1);
        assert_eq!(c[1][PieceType::Rook.index()], 1);
        assert_eq!(c[1][PieceType::Pawn.index()], 0);

        assert!(parse_code("README").is_none());
        assert!(parse_code("QvK").is_none(), "a side must start with its king");
        assert!(parse_code("KXvK").is_none());
    }

    /// The material key must be the one `Position` computes, or no table is ever found.
    #[test]
    fn the_material_key_matches_the_positions_own() {
        use crate::board::position::Position;
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", false).expect("valid");
        let counts = parse_code("KRvK").expect("a real code");
        assert_eq!(material_key(&counts), pos.st().material_key);
    }

    #[test]
    fn the_two_keys_differ_unless_the_material_is_symmetric() {
        let t = TbTable::new(TbType::Wdl, "KRvK", "x".into()).expect("built");
        assert_ne!(t.key, t.key2);
        let sym = TbTable::new(TbType::Wdl, "KRvKR", "x".into()).expect("built");
        assert_eq!(sym.key, sym.key2, "symmetric material has one key");
    }

    #[test]
    fn the_leading_colour_is_the_side_with_fewer_pawns() {
        // White has two pawns, Black one: Black leads, so pawn_count is [1, 2].
        let t = TbTable::new(TbType::Wdl, "KPPvKP", "x".into()).expect("built");
        assert_eq!(t.pawn_count, [1, 2]);
        // No black pawns at all: White leads.
        let t = TbTable::new(TbType::Wdl, "KPvK", "x".into()).expect("built");
        assert_eq!(t.pawn_count, [1, 0]);
        assert!(t.has_pawns);
    }

    #[test]
    fn unique_pieces_are_detected() {
        assert!(TbTable::new(TbType::Wdl, "KRvK", "x".into()).expect("built").has_unique_pieces);
        // Two rooks a side and nothing else: no piece appears exactly once.
        assert!(
            !TbTable::new(TbType::Wdl, "KRRvKRR", "x".into()).expect("built").has_unique_pieces
        );
    }

    #[test]
    fn the_two_file_kinds_have_different_magics() {
        assert_ne!(TbType::Wdl.magic(), TbType::Dtz.magic());
        assert_eq!(TbType::Wdl.ext(), "rtbw");
        assert_eq!(TbType::Dtz.ext(), "rtbz");
        assert_eq!(TbType::Wdl.sides(), 2);
        assert_eq!(TbType::Dtz.sides(), 1, "DTZ stores one side to move");
    }

    #[test]
    fn a_missing_file_loads_to_none_rather_than_panicking() {
        let t = TbTable::new(TbType::Wdl, "KQvK", "/nonexistent/KQvK.rtbw".into()).expect("built");
        assert!(t.loaded().is_none());
        // And the failure is cached: a second call must not retry the read.
        assert!(t.loaded().is_none());
    }
}
