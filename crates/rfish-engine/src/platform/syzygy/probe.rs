//! Turning a position into a table index, and reading the answer.
//!
//! The whole of Syzygy's compactness is in this file's arithmetic. A position becomes one
//! integer by collapsing every symmetry the position has: the board's eightfold reflection,
//! the interchangeability of identical pieces, and the fact that a file stores only the
//! stronger side as White.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp` — `do_probe_table`, `map_score`.

use crate::board::position::Position;
use crate::board::types::{Color, File, Piece, PieceType, Rank, Square};

use super::pairs::{decompress, flag};
use super::table::{Loaded, TbTable, TbType};
use super::tables::{INDEX, TB_PIECES, edge_distance, off_a1h8};

/// A win/draw/loss verdict, from the side to move's point of view.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(i32)]
pub enum Wdl {
    Loss = -2,
    BlessedLoss = -1,
    Draw = 0,
    CursedWin = 1,
    Win = 2,
}

impl Wdl {
    /// Reconstruct from the stored value.
    #[must_use]
    pub const fn from_i32(v: i32) -> Wdl {
        match v {
            -2 => Wdl::Loss,
            -1 => Wdl::BlessedLoss,
            1 => Wdl::CursedWin,
            2 => Wdl::Win,
            _ => Wdl::Draw,
        }
    }

    /// The verdict from the other side's point of view.
    #[must_use]
    pub const fn negate(self) -> Wdl {
        Wdl::from_i32(-(self as i32))
    }
}

/// How a probe went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProbeState {
    /// No table answered.
    Fail,
    /// The probe succeeded.
    Ok,
    /// The best move zeroes the halfmove clock, so DTZ is one ply.
    ZeroingBestMove,
    /// A DTZ table stores the other side to move; the caller must play a move first.
    ChangeStm,
}

/// Compute the index for `pos` in `entry` and read the value there.
///
/// Returns `None` when the table is one-sided and stores the other side to move — the
/// caller answers that by making a move rather than by failing.
#[allow(clippy::too_many_lines)]
pub fn do_probe_table(
    pos: &Position,
    entry: &TbTable,
    loaded: &Loaded,
    wdl: Wdl,
) -> Result<i32, ProbeState> {
    let t = &*INDEX;
    let mut squares = [Square::A1; TB_PIECES];
    let mut pieces = [0u8; TB_PIECES];
    let mut size = 0usize;
    let mut lead_pawns_count = 0usize;
    let mut lead_pawns = crate::board::bitboard::Bitboard::EMPTY;
    let mut tb_file = 0usize;

    // A file stores one colour assignment only. Two cases send us to the mirrored view: the
    // material is symmetric and it is Black to move, or Black is the stronger side.
    let symmetric_black_to_move = entry.key == entry.key2 && pos.side_to_move() == Color::Black;
    let black_stronger = pos.st().material_key != entry.key;
    let mirrored = symmetric_black_to_move || black_stronger;
    let flip_color = u8::from(mirrored) * 8;
    let flip_squares = u8::from(mirrored) * 56;
    let stm = usize::from(mirrored) ^ pos.side_to_move().index();

    if entry.has_pawns {
        // With pawns, the file is split four ways by which file the leading pawn is on
        // after mirroring. The leading pawn's colour is the table's reference colour, taken
        // from the first entry of the piece order.
        let pc = Piece::from_raw(entry.pairs(loaded, 0, 0).pieces[0] ^ flip_color);
        debug_assert_eq!(pc.piece_type(), PieceType::Pawn);
        lead_pawns = pos.pieces_of(pc.color(), PieceType::Pawn);
        for s in lead_pawns {
            squares[size] = Square::new(usize::from(s.raw() ^ flip_squares));
            size += 1;
        }
        lead_pawns_count = size;

        // The leading pawn is the one with the highest pawn number: nearest an edge, and
        // among equal files the one on the lowest rank.
        let mut best = 0;
        for i in 1..lead_pawns_count {
            if t.pawn_less(squares[best], squares[i]) {
                best = i;
            }
        }
        squares.swap(0, best);
        tb_file = edge_distance(squares[0].file().index());
    }

    // A DTZ table stores only one side to move. When it is the other one, the caller has to
    // play a move and try again rather than accept a value that is not there. A pawnless
    // table with symmetric material stores both, so the check does not apply.
    if entry.kind == TbType::Dtz {
        let flags = entry.pairs(loaded, stm, tb_file).flags;
        let stored_stm = usize::from(flags & flag::STM != 0);
        let both_sides_stored = entry.key == entry.key2 && !entry.has_pawns;
        if stored_stm != stm && !both_sides_stored {
            return Err(ProbeState::ChangeStm);
        }
    }

    // Everything but the leading pawns, mapped to the reference colour and squares.
    for s in pos.occupied() & !lead_pawns {
        squares[size] = Square::new(usize::from(s.raw() ^ flip_squares));
        pieces[size] = pos.piece_on(s).raw() ^ flip_color;
        size += 1;
    }
    debug_assert!(size >= 2);

    let d = entry.pairs(loaded, stm, tb_file);

    // Reorder into the sequence the table's encoder used; that order is what makes the
    // groups below contiguous.
    for i in lead_pawns_count..size - 1 {
        for j in i + 1..size {
            if d.pieces[i] == pieces[j] {
                pieces.swap(i, j);
                squares.swap(i, j);
                break;
            }
        }
    }

    // Fold the horizontal symmetry: the leading piece goes into the left half.
    if squares[0].file() > File::D {
        for sq in squares.iter_mut().take(size) {
            *sq = sq.flip_file();
        }
    }

    let mut idx: u64;
    if entry.has_pawns {
        idx = t.lead_pawn_idx[lead_pawns_count][squares[0].index()] as u64;
        // The remaining leading pawns are unordered, so sort them and count combinations.
        squares[1..lead_pawns_count].sort_by_key(|s| t.map_pawns[s.index()]);
        for (i, sq) in squares.iter().enumerate().take(lead_pawns_count).skip(1) {
            idx += t.binomial[i][t.map_pawns[sq.index()] as usize] as u64;
        }
    } else {
        // Without pawns there is a vertical symmetry too: the leading piece goes below the
        // fifth rank.
        if squares[0].rank() > Rank::R4 {
            for sq in squares.iter_mut().take(size) {
                *sq = sq.flip_rank();
            }
        }
        // And a diagonal one: the first leading-group piece off the a1-h8 diagonal is put
        // below it, taking the rest of the position with it.
        for i in 0..d.group_len[0] as usize {
            if off_a1h8(squares[i]) == 0 {
                continue;
            }
            if off_a1h8(squares[i]) > 0 {
                for sq in squares.iter_mut().take(size).skip(i) {
                    let v = sq.index();
                    *sq = Square::new(((v >> 3) | (v << 3)) & 63);
                }
            }
            break;
        }

        if entry.has_unique_pieces {
            // Three distinguishable pieces are numbered together, in four cases that split
            // on how many of them sit on the long diagonal. Each later square is renumbered
            // to skip the ones already used, which is what removes the impossible placements.
            let s0 = squares[0].index();
            let s1 = squares[1].index();
            let s2 = squares[2].index();
            let adjust1 = usize::from(s1 > s0);
            let adjust2 = usize::from(s2 > s0) + usize::from(s2 > s1);

            idx = if off_a1h8(squares[0]) != 0 {
                ((t.map_a1d1d4[s0] as usize * 63 + (s1 - adjust1)) * 62 + s2 - adjust2) as u64
            } else if off_a1h8(squares[1]) != 0 {
                ((6 * 63 + squares[0].rank().index() * 28 + t.map_b1h1h7[s1] as usize) * 62 + s2
                    - adjust2) as u64
            } else if off_a1h8(squares[2]) != 0 {
                (6 * 63 * 62
                    + 4 * 28 * 62
                    + squares[0].rank().index() * 7 * 28
                    + (squares[1].rank().index() - adjust1) * 28
                    + t.map_b1h1h7[s2] as usize) as u64
            } else {
                (6 * 63 * 62
                    + 4 * 28 * 62
                    + 4 * 7 * 28
                    + squares[0].rank().index() * 7 * 6
                    + (squares[1].rank().index() - adjust1) * 6
                    + (squares[2].rank().index() - adjust2)) as u64
            };
        } else {
            // Nothing distinguishable beyond the kings: number the king pair alone.
            idx = t.map_kk[t.map_a1d1d4[squares[0].index()] as usize][squares[1].index()] as u64;
        }
    }

    idx *= d.group_idx[0];

    // The remaining groups, each a run of identical pieces, contribute a combination count.
    let mut group_start = d.group_len[0] as usize;
    let mut next = 0usize;
    let mut remaining_pawns = entry.has_pawns && entry.pawn_count[1] != 0;
    loop {
        next += 1;
        let len = d.group_len[next] as usize;
        if len == 0 {
            break;
        }
        squares[group_start..group_start + len].sort_unstable();
        let mut n: u64 = 0;
        for i in 0..len {
            // Renumber past every square already used by an earlier group.
            let mut adjust = 0usize;
            for j in 0..group_start {
                adjust += usize::from(squares[group_start + i] > squares[j]);
            }
            // Remaining pawns are numbered from a2, so the first rank is skipped.
            let v = squares[group_start + i].index() - adjust - 8 * usize::from(remaining_pawns);
            n += t.binomial[i + 1][v] as u64;
        }
        remaining_pawns = false;
        idx += n * d.group_idx[next];
        group_start += len;
    }

    let value = decompress(d, &loaded.bytes, idx);
    Ok(map_score(entry, loaded, tb_file, value, wdl))
}

/// Turn a stored value into the caller's units.
///
/// For WDL that is a shift by two. For DTZ it is a per-table remap and, when the table
/// stores moves rather than plies, a doubling.
fn map_score(entry: &TbTable, loaded: &Loaded, file: usize, value: i32, wdl: Wdl) -> i32 {
    /// Which of the four stored remap tables a verdict selects, indexed by `wdl + 2`.
    const WDL_MAP: [usize; 5] = [1, 3, 0, 2, 0];

    if entry.kind == TbType::Wdl {
        return value - 2;
    }

    let d = entry.pairs(loaded, 0, file);
    let flags = d.flags;
    let mut value = value;

    if flags & flag::MAPPED != 0 {
        let slot = d.map_idx[WDL_MAP[(wdl as i32 + 2) as usize]] as usize;
        value = if flags & flag::WIDE != 0 {
            let off = entry_map(loaded) + 2 * (slot + value as usize - 1);
            i32::from(u16::from_le_bytes([loaded.bytes[off], loaded.bytes[off + 1]]))
        } else {
            i32::from(loaded.bytes[entry_map(loaded) + slot + value as usize - 1])
        };
    }

    // A cursed win or blessed loss is always stored in moves, and a plain win or loss only
    // when the table says so.
    if (wdl == Wdl::Win && flags & flag::WIN_PLIES == 0)
        || (wdl == Wdl::Loss && flags & flag::LOSS_PLIES == 0)
        || wdl == Wdl::CursedWin
        || wdl == Wdl::BlessedLoss
    {
        value *= 2;
    }
    value + 1
}

/// Where the DTZ remap table starts.
#[inline]
fn entry_map(loaded: &Loaded) -> usize {
    loaded.map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_negates_to_its_mirror() {
        assert_eq!(Wdl::Win.negate(), Wdl::Loss);
        assert_eq!(Wdl::Loss.negate(), Wdl::Win);
        assert_eq!(Wdl::CursedWin.negate(), Wdl::BlessedLoss);
        assert_eq!(Wdl::BlessedLoss.negate(), Wdl::CursedWin);
        assert_eq!(Wdl::Draw.negate(), Wdl::Draw);
    }

    #[test]
    fn the_verdicts_order_from_loss_to_win() {
        // The search compares them, so the ordering is load-bearing rather than cosmetic.
        assert!(Wdl::Loss < Wdl::BlessedLoss);
        assert!(Wdl::BlessedLoss < Wdl::Draw);
        assert!(Wdl::Draw < Wdl::CursedWin);
        assert!(Wdl::CursedWin < Wdl::Win);
    }

    #[test]
    fn an_unknown_stored_value_reads_as_a_draw() {
        assert_eq!(Wdl::from_i32(0), Wdl::Draw);
        assert_eq!(Wdl::from_i32(99), Wdl::Draw);
        assert_eq!(Wdl::from_i32(2), Wdl::Win);
    }
}
