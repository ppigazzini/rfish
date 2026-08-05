//! The indexing tables the Syzygy encoding is built on.
//!
//! A tablebase file is a flat array of values, and a position's entry is found by turning
//! the position into a single integer. These tables are what make that integer small enough
//! for the file to exist at all: they collapse the board's eightfold symmetry, they number
//! the legal king placements rather than all 4096 pairs, and they count combinations rather
//! than permutations for identical pieces.
//!
//! Every one is derived here rather than written out, because each is a consequence of the
//! others and a transcribed constant would not follow a change in the derivation.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp: Tablebases::init`.

use std::sync::LazyLock;

use crate::board::bitboard::KING_ATTACKS;
use crate::board::types::{File, Rank, SQUARE_NB, Square};

/// The largest piece count the format supports.
pub const TB_PIECES: usize = 7;

/// How far a square sits off the a1-h8 diagonal: negative below it, positive above.
#[inline]
#[must_use]
pub fn off_a1h8(sq: Square) -> i32 {
    sq.rank().index() as i32 - sq.file().index() as i32
}

/// Everything the index computation needs, built once.
#[derive(Debug)]
pub struct IndexTables {
    /// Squares a2..h7 numbered 0..47, ordered so the pawn with the HIGHEST value is the
    /// leading one — the pawn nearest an edge, and among equal files the one on the lowest
    /// rank. That ordering is what the file's four per-file sub-tables are split on.
    pub map_pawns: [i32; SQUARE_NB],
    /// A square below the a1-h8 diagonal, numbered 0..27.
    pub map_b1h1h7: [i32; SQUARE_NB],
    /// A square in the a1-d1-d4 triangle, numbered 0..9. The squares ON the diagonal are
    /// numbered last, which is what lets the caller distinguish them by index alone.
    pub map_a1d1d4: [i32; SQUARE_NB],
    /// The 462 legal, non-mirrored placements of two kings, indexed by the first king's
    /// triangle number and the second king's square.
    pub map_kk: [[i32; SQUARE_NB]; 10],
    /// `binomial[k][n]`: ways to choose `k` elements from `n`.
    pub binomial: [[i32; SQUARE_NB]; 6],
    /// `lead_pawn_idx[count][square]`: the index contributed by the leading pawn.
    pub lead_pawn_idx: [[i32; SQUARE_NB]; 6],
    /// `lead_pawns_size[count][file]`: how many placements the leading group has.
    pub lead_pawns_size: [[i32; 4]; 6],
}

impl IndexTables {
    fn build() -> IndexTables {
        let mut t = IndexTables {
            map_pawns: [0; SQUARE_NB],
            map_b1h1h7: [0; SQUARE_NB],
            map_a1d1d4: [0; SQUARE_NB],
            map_kk: [[0; SQUARE_NB]; 10],
            binomial: [[0; SQUARE_NB]; 6],
            lead_pawn_idx: [[0; SQUARE_NB]; 6],
            lead_pawns_size: [[0; 4]; 6],
        };

        // Squares below the a1-h8 diagonal, in square order.
        let mut code = 0;
        for s in Square::all() {
            if off_a1h8(s) < 0 {
                t.map_b1h1h7[s.index()] = code;
                code += 1;
            }
        }

        // The a1-d1-d4 triangle. Off-diagonal squares first, then the diagonal ones, so
        // "is this square on the diagonal" is answerable from the index.
        let mut diagonal = Vec::new();
        code = 0;
        for s in Square::all() {
            if s.index() > 27 {
                break;
            }
            if off_a1h8(s) < 0 && s.file() <= File::D {
                t.map_a1d1d4[s.index()] = code;
                code += 1;
            } else if off_a1h8(s) == 0 && s.file() <= File::D {
                diagonal.push(s);
            }
        }
        for s in diagonal {
            t.map_a1d1d4[s.index()] = code;
            code += 1;
        }

        // The 462 king pairs. When the first king is ON the diagonal the second may not be
        // above it, or the position would be a mirror of one already counted.
        let mut both_on_diagonal = Vec::new();
        code = 0;
        for idx in 0..10 {
            for s1 in Square::all() {
                if s1.index() > 27 {
                    break;
                }
                // b1 maps to 0 and so does every square outside the triangle; the extra
                // test is what excludes the latter.
                if t.map_a1d1d4[s1.index()] != idx || (idx == 0 && s1.index() != 1) {
                    continue;
                }
                for s2 in Square::all() {
                    let adjacent = (KING_ATTACKS[s1.index()]
                        | crate::board::bitboard::Bitboard::from_square(s1))
                    .contains(s2);
                    if adjacent {
                        continue; // The kings would be touching.
                    }
                    if off_a1h8(s1) == 0 && off_a1h8(s2) > 0 {
                        continue; // First on the diagonal, second above it.
                    }
                    if off_a1h8(s1) == 0 && off_a1h8(s2) == 0 {
                        both_on_diagonal.push((idx as usize, s2));
                    } else {
                        t.map_kk[idx as usize][s2.index()] = code;
                        code += 1;
                    }
                }
            }
        }
        for (idx, s2) in both_on_diagonal {
            t.map_kk[idx][s2.index()] = code;
            code += 1;
        }

        // Pascal's rule.
        t.binomial[0][0] = 1;
        for n in 1..64 {
            for k in 0..6.min(n + 1) {
                t.binomial[k][n] = if k > 0 { t.binomial[k - 1][n - 1] } else { 0 }
                    + if k < n { t.binomial[k][n - 1] } else { 0 };
            }
        }

        // The pawn numbering, and the leading-pawn index built from it in the same pass.
        //
        // A pawn on a2 leaves 47 squares another pawn could occupy without being "before"
        // it; every rank up removes two more, because the mirrored file is excluded too.
        let mut available = 47;
        for lead_count in 1..=5usize {
            for f in 0..4usize {
                // The index restarts per file: the file's sub-table is separate, so the
                // same index range is reused.
                let mut idx = 0;
                for r in 1..=6usize {
                    let sq = Square::make(File::new(f), Rank::new(r));
                    if lead_count == 1 {
                        t.map_pawns[sq.index()] = available;
                        available -= 1;
                        t.map_pawns[sq.flip_file().index()] = available;
                        available -= 1;
                    }
                    t.lead_pawn_idx[lead_count][sq.index()] = idx;
                    idx += t.binomial[lead_count - 1][t.map_pawns[sq.index()] as usize];
                }
                t.lead_pawns_size[lead_count][f] = idx;
            }
        }

        t
    }

    /// Order two squares by their pawn numbering — the comparison the leading-pawn sort
    /// uses.
    #[inline]
    #[must_use]
    pub fn pawn_less(&self, a: Square, b: Square) -> bool {
        self.map_pawns[a.index()] < self.map_pawns[b.index()]
    }
}

/// The tables, built on first use.
pub static INDEX: LazyLock<IndexTables> = LazyLock::new(IndexTables::build);

/// Which of a pawnful table's four per-file sub-tables a position uses.
///
/// **Not a [`File`].** A board file is one of eight; a `TbFile` is one of four, and the map
/// between them is [`TbFile::for_board_file`] — which is why a leading pawn on the e-file and
/// one on the d-file share a table. The two were both `usize` and the conversion was an
/// unannotated `usize -> usize` hop, in the one module where `AGENTS.md` records that an
/// index computed one off returns a CONFIDENT wrong verdict.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct TbFile(u8);

impl TbFile {
    /// The only sub-table a pawnless position has.
    pub const ONLY: TbFile = TbFile(0);
    /// The highest sub-table index a pawnful table has.
    pub const MAX_PAWNFUL: TbFile = TbFile(3);

    /// The sub-table a leading pawn on `file` selects: how far that file is from the nearest
    /// edge, 0 for the a- and h-files and 3 for d and e.
    #[inline]
    #[must_use]
    pub const fn for_board_file(file: File) -> TbFile {
        let f = file.index();
        TbFile(if f < 7 - f { f as u8 } else { (7 - f) as u8 })
    }

    /// Index the sub-table array.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Every sub-table up to and including `self`, for the loaders that walk them.
    #[inline]
    pub fn up_to(self) -> impl Iterator<Item = TbFile> {
        (0..=self.0).map(TbFile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_king_pair_table_holds_exactly_the_462_legal_placements() {
        let t = &*INDEX;
        let mut seen = std::collections::HashSet::new();
        let mut count = 0;
        for idx in 0..10 {
            for s2 in 0..SQUARE_NB {
                let v = t.map_kk[idx][s2];
                if v != 0 {
                    seen.insert(v);
                    count += 1;
                }
            }
        }
        // 462 codes, of which one is zero and therefore invisible to the scan above.
        assert_eq!(seen.len(), 461);
        assert_eq!(count, 461);
        assert_eq!(seen.iter().copied().max(), Some(461));
    }

    #[test]
    fn the_triangle_numbers_ten_squares_with_the_diagonal_last() {
        let t = &*INDEX;
        // a1, b2, c3 and d4 are the diagonal squares of the triangle, and must be 6..9.
        for sq in [0usize, 9, 18, 27] {
            assert!(t.map_a1d1d4[sq] >= 6, "square {sq} should be numbered last");
        }
        // b1 is the first off-diagonal square and is numbered 0.
        assert_eq!(t.map_a1d1d4[1], 0);
    }

    #[test]
    fn the_below_diagonal_map_numbers_twenty_eight_squares() {
        let t = &*INDEX;
        let count = Square::all().filter(|s| off_a1h8(*s) < 0).count();
        assert_eq!(count, 28);
        let max = Square::all().filter(|s| off_a1h8(*s) < 0).map(|s| t.map_b1h1h7[s.index()]).max();
        assert_eq!(max, Some(27));
    }

    #[test]
    fn the_pawn_numbering_covers_a2_to_h7_exactly_once() {
        let t = &*INDEX;
        let mut values: Vec<i32> = (8..56).map(|i| t.map_pawns[i]).collect();
        values.sort_unstable();
        assert_eq!(values, (0..48).collect::<Vec<_>>());
        // a2 is the leading square: nothing can be more toward an edge or lower.
        assert_eq!(t.map_pawns[Square::make(File::new(0), Rank::new(1)).index()], 47);
    }

    #[test]
    fn binomial_coefficients_follow_pascals_rule() {
        let t = &*INDEX;
        assert_eq!(t.binomial[0][0], 1);
        assert_eq!(t.binomial[1][10], 10);
        assert_eq!(t.binomial[2][10], 45);
        assert_eq!(t.binomial[3][10], 120);
        // C(63,5), which is what a five-piece group over 63 free squares costs.
        assert_eq!(t.binomial[5][63], 7_028_847);
    }

    #[test]
    fn the_edge_distance_is_symmetric_about_the_centre() {
        assert_eq!(TbFile::for_board_file(File::A), TbFile::ONLY);
        assert_eq!(TbFile::for_board_file(File::H), TbFile::ONLY);
        assert_eq!(TbFile::for_board_file(File::D), TbFile::MAX_PAWNFUL);
        assert_eq!(TbFile::for_board_file(File::E), TbFile::MAX_PAWNFUL);
    }
}
