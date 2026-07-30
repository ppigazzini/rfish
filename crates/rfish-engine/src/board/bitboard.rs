//! The 64-bit board set and its geometry.
//!
//! A [`Bitboard`] is a set of squares, bit `i` standing for the square with index `i`.
//! Everything in this module is pure geometry: no position state, no attack tables that
//! depend on occupancy. The occupancy-dependent slider attacks live in
//! [`crate::board::attacks`].
//!
//! Golden: `Stockfish/src/bitboard.h`, `Stockfish/src/bitboard.cpp`.

use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Not};

use super::types::{Color, Direction, FILE_NB, RANK_NB, SQUARE_NB, Square};

/// A set of squares.
///
/// The operators are the set operations: `&` is intersection, `|` union, `^` symmetric
/// difference, `!` complement. That is upstream's spelling too, so a ported expression
/// reads the same on both sides.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Bitboard(pub u64);

impl Bitboard {
    /// The empty set.
    pub const EMPTY: Bitboard = Bitboard(0);
    /// Every square.
    pub const ALL: Bitboard = Bitboard(!0);
    /// The 32 light squares.
    pub const LIGHT_SQUARES: Bitboard = Bitboard(0x55AA_55AA_55AA_55AA);
    /// The 32 dark squares.
    pub const DARK_SQUARES: Bitboard = Bitboard(0xAA55_AA55_AA55_AA55);

    /// The singleton set holding `sq`.
    #[inline(always)]
    #[must_use]
    pub const fn from_square(sq: Square) -> Bitboard {
        debug_assert!(sq.is_ok());
        Bitboard(1u64 << sq.index())
    }

    /// The raw word.
    #[inline(always)]
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// True when the set is empty.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True when the set holds at least one square.
    #[inline(always)]
    #[must_use]
    pub const fn any(self) -> bool {
        self.0 != 0
    }

    /// True when `sq` is a member.
    #[inline(always)]
    #[must_use]
    pub const fn contains(self, sq: Square) -> bool {
        self.0 & (1u64 << sq.index()) != 0
    }

    /// Add `sq` to the set.
    #[inline(always)]
    pub const fn set(&mut self, sq: Square) {
        self.0 |= 1u64 << sq.index();
    }

    /// Remove `sq` from the set.
    #[inline(always)]
    pub const fn clear(&mut self, sq: Square) {
        self.0 &= !(1u64 << sq.index());
    }

    /// Flip `sq`'s membership.
    #[inline(always)]
    pub const fn toggle(&mut self, sq: Square) {
        self.0 ^= 1u64 << sq.index();
    }

    /// The number of squares in the set.
    #[inline(always)]
    #[must_use]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// True when the set holds exactly one square — upstream's `more_than_one()` negated.
    #[inline(always)]
    #[must_use]
    pub const fn exactly_one(self) -> bool {
        self.0 != 0 && self.0.is_power_of_two()
    }

    /// True when the set holds two or more squares.
    #[inline(always)]
    #[must_use]
    pub const fn more_than_one(self) -> bool {
        self.0 & self.0.wrapping_sub(1) != 0
    }

    /// The least significant member.
    ///
    /// # Panics
    /// Panics on the empty set: upstream's `lsb()` asserts the same precondition, and a
    /// zero here would silently read square a1.
    #[inline(always)]
    #[must_use]
    pub const fn lsb(self) -> Square {
        assert!(self.0 != 0, "lsb of the empty set");
        Square::new(self.0.trailing_zeros() as usize)
    }

    /// The most significant member.
    ///
    /// # Panics
    /// Panics on the empty set, for the same reason as [`Bitboard::lsb`].
    #[inline(always)]
    #[must_use]
    pub const fn msb(self) -> Square {
        assert!(self.0 != 0, "msb of the empty set");
        Square::new(63 - self.0.leading_zeros() as usize)
    }

    /// Remove and return the least significant member.
    ///
    /// This is the loop body of every generator in the tree. Upstream spells it
    /// `pop_lsb(b)`; the `&mut self` receiver makes the mutation part of the type here
    /// rather than a convention.
    ///
    /// # Panics
    /// Panics on the empty set.
    #[inline(always)]
    pub const fn pop_lsb(&mut self) -> Square {
        let sq = self.lsb();
        self.0 &= self.0 - 1;
        sq
    }

    /// The member frontmost from `c`'s point of view: the most significant for White,
    /// the least for Black. Upstream's `frontmost_sq`.
    ///
    /// # Panics
    /// Panics on the empty set.
    #[inline(always)]
    #[must_use]
    pub const fn frontmost(self, c: Color) -> Square {
        match c {
            Color::White => self.msb(),
            Color::Black => self.lsb(),
        }
    }

    /// Shift every member one square in `d`, dropping members that would wrap around a
    /// board edge.
    #[inline(always)]
    #[must_use]
    pub const fn shift(self, d: Direction) -> Bitboard {
        let b = self.0;
        Bitboard(match d {
            Direction::North => b << 8,
            Direction::South => b >> 8,
            Direction::East => (b & !FILE_H.0) << 1,
            Direction::West => (b & !FILE_A.0) >> 1,
            Direction::NorthEast => (b & !FILE_H.0) << 9,
            Direction::NorthWest => (b & !FILE_A.0) << 7,
            Direction::SouthEast => (b & !FILE_H.0) >> 7,
            Direction::SouthWest => (b & !FILE_A.0) >> 9,
        })
    }

    /// Push every pawn in the set one square forward for `c`.
    #[inline(always)]
    #[must_use]
    pub const fn pawn_push(self, c: Color) -> Bitboard {
        self.shift(Direction::pawn_push(c))
    }

    /// The squares attacked by the pawns in the set, for `c`.
    #[inline(always)]
    #[must_use]
    pub const fn pawn_attacks(self, c: Color) -> Bitboard {
        match c {
            Color::White => {
                Bitboard(self.shift(Direction::NorthWest).0 | self.shift(Direction::NorthEast).0)
            }
            Color::Black => {
                Bitboard(self.shift(Direction::SouthWest).0 | self.shift(Direction::SouthEast).0)
            }
        }
    }

    /// Iterate the members, least significant first.
    ///
    /// A `Bitboard` iterates by value: the iterator owns a copy, so a loop can freely
    /// mutate the board it came from. That is exactly upstream's `while (b) pop_lsb(b)`
    /// over a local, with the local made explicit.
    #[inline(always)]
    #[must_use]
    pub const fn iter(self) -> BitboardIter {
        BitboardIter(self)
    }
}

/// Iterator over a bitboard's members, least significant first.
///
/// Deliberately NOT `Copy`: a copied iterator that is then advanced leaves the original
/// where it was, which reads as a loop that silently repeats itself. The bitboard it is
/// built from IS `Copy`, which is what makes `for sq in some_bb` free.
#[derive(Clone, Debug)]
pub struct BitboardIter(Bitboard);

impl Iterator for BitboardIter {
    type Item = Square;

    #[inline(always)]
    fn next(&mut self) -> Option<Square> {
        if self.0.is_empty() { None } else { Some(self.0.pop_lsb()) }
    }

    #[inline(always)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.count() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for BitboardIter {}

impl IntoIterator for Bitboard {
    type Item = Square;
    type IntoIter = BitboardIter;

    #[inline(always)]
    fn into_iter(self) -> BitboardIter {
        self.iter()
    }
}

macro_rules! bitboard_binop {
    ($trait:ident, $method:ident, $assign_trait:ident, $assign_method:ident, $op:tt) => {
        impl $trait for Bitboard {
            type Output = Bitboard;
            #[inline(always)]
            fn $method(self, rhs: Bitboard) -> Bitboard {
                Bitboard(self.0 $op rhs.0)
            }
        }
        impl $trait<Square> for Bitboard {
            type Output = Bitboard;
            #[inline(always)]
            fn $method(self, rhs: Square) -> Bitboard {
                Bitboard(self.0 $op Bitboard::from_square(rhs).0)
            }
        }
        impl $assign_trait for Bitboard {
            #[inline(always)]
            fn $assign_method(&mut self, rhs: Bitboard) {
                self.0 = self.0 $op rhs.0;
            }
        }
        impl $assign_trait<Square> for Bitboard {
            #[inline(always)]
            fn $assign_method(&mut self, rhs: Square) {
                self.0 = self.0 $op Bitboard::from_square(rhs).0;
            }
        }
    };
}

bitboard_binop!(BitAnd, bitand, BitAndAssign, bitand_assign, &);
bitboard_binop!(BitOr, bitor, BitOrAssign, bitor_assign, |);
bitboard_binop!(BitXor, bitxor, BitXorAssign, bitxor_assign, ^);

impl Not for Bitboard {
    type Output = Bitboard;

    #[inline(always)]
    fn not(self) -> Bitboard {
        Bitboard(!self.0)
    }
}

impl fmt::Debug for Bitboard {
    /// Render as upstream's `Bitboards::pretty()` does: an 8x8 grid, rank 8 at the top.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "+---+---+---+---+---+---+---+---+")?;
        for rank in (0..8).rev() {
            for file in 0..8 {
                let occupied = self.contains(Square::make(file, rank));
                write!(f, "| {} ", if occupied { 'X' } else { ' ' })?;
            }
            writeln!(f, "| {}", rank + 1)?;
            writeln!(f, "+---+---+---+---+---+---+---+---+")?;
        }
        writeln!(f, "  a   b   c   d   e   f   g   h")
    }
}

// ---------------------------------------------------------------------------
// File and rank masks
// ---------------------------------------------------------------------------

pub const FILE_A: Bitboard = Bitboard(0x0101_0101_0101_0101);
pub const FILE_B: Bitboard = Bitboard(FILE_A.0 << 1);
pub const FILE_C: Bitboard = Bitboard(FILE_A.0 << 2);
pub const FILE_D: Bitboard = Bitboard(FILE_A.0 << 3);
pub const FILE_E: Bitboard = Bitboard(FILE_A.0 << 4);
pub const FILE_F: Bitboard = Bitboard(FILE_A.0 << 5);
pub const FILE_G: Bitboard = Bitboard(FILE_A.0 << 6);
pub const FILE_H: Bitboard = Bitboard(FILE_A.0 << 7);

pub const RANK_1: Bitboard = Bitboard(0xFF);
pub const RANK_2: Bitboard = Bitboard(RANK_1.0 << 8);
pub const RANK_3: Bitboard = Bitboard(RANK_1.0 << 16);
pub const RANK_4: Bitboard = Bitboard(RANK_1.0 << 24);
pub const RANK_5: Bitboard = Bitboard(RANK_1.0 << 32);
pub const RANK_6: Bitboard = Bitboard(RANK_1.0 << 40);
pub const RANK_7: Bitboard = Bitboard(RANK_1.0 << 48);
pub const RANK_8: Bitboard = Bitboard(RANK_1.0 << 56);

/// The eight file masks, a-file first.
pub const FILE_BB: [Bitboard; FILE_NB] =
    [FILE_A, FILE_B, FILE_C, FILE_D, FILE_E, FILE_F, FILE_G, FILE_H];

/// The eight rank masks, rank 1 first.
pub const RANK_BB: [Bitboard; RANK_NB] =
    [RANK_1, RANK_2, RANK_3, RANK_4, RANK_5, RANK_6, RANK_7, RANK_8];

/// The file `sq` stands on.
#[inline(always)]
#[must_use]
pub const fn file_bb(sq: Square) -> Bitboard {
    FILE_BB[sq.file()]
}

/// The rank `sq` stands on.
#[inline(always)]
#[must_use]
pub const fn rank_bb(sq: Square) -> Bitboard {
    RANK_BB[sq.rank()]
}

/// The rank `r` counted from `c`'s own back rank.
#[inline(always)]
#[must_use]
pub const fn relative_rank_bb(c: Color, r: usize) -> Bitboard {
    RANK_BB[r ^ ((c as usize) * 7)]
}

// ---------------------------------------------------------------------------
// Compile-time geometry tables
// ---------------------------------------------------------------------------

/// Build a table in a `const` block. Rust's const evaluator runs the same loops the C++
/// runs in `Bitboards::init()`, so these tables cost nothing at startup and cannot be
/// read before they are filled — a class of bug the C++ has to order its init around.
const fn square_distance_table() -> [[u8; SQUARE_NB]; SQUARE_NB] {
    let mut t = [[0u8; SQUARE_NB]; SQUARE_NB];
    let mut a = 0;
    while a < SQUARE_NB {
        let mut b = 0;
        while b < SQUARE_NB {
            let df = (a % 8).abs_diff(b % 8);
            let dr = (a / 8).abs_diff(b / 8);
            t[a][b] = if df > dr { df as u8 } else { dr as u8 };
            b += 1;
        }
        a += 1;
    }
    t
}

/// Chebyshev distance between every pair of squares.
pub static SQUARE_DISTANCE: [[u8; SQUARE_NB]; SQUARE_NB] = square_distance_table();

/// Chebyshev distance between two squares.
#[inline(always)]
#[must_use]
pub fn distance(a: Square, b: Square) -> usize {
    SQUARE_DISTANCE[a.index()][b.index()] as usize
}

/// Step-piece attacks: knight, king, and the two pawn colours.
///
/// Sliders are not here — their attack set depends on occupancy and lives in
/// [`crate::board::attacks`].
const fn step_attacks(steps: &[i32]) -> [Bitboard; SQUARE_NB] {
    let mut t = [Bitboard::EMPTY; SQUARE_NB];
    let mut s = 0;
    while s < SQUARE_NB {
        let mut i = 0;
        while i < steps.len() {
            let to = s as i32 + steps[i];
            // A step is legal when it lands on the board AND stays within two files of
            // its origin. The file test is what rejects the wrap-around that pure index
            // arithmetic would let through.
            if to >= 0 && to < 64 {
                let du = to as usize;
                let df = (s % 8).abs_diff(du % 8);
                let dr = (s / 8).abs_diff(du / 8);
                let dist = if df > dr { df } else { dr };
                if dist <= 2 {
                    t[s] = Bitboard(t[s].0 | 1u64 << du);
                }
            }
            i += 1;
        }
        s += 1;
    }
    t
}

/// Knight attacks from each square.
pub static KNIGHT_ATTACKS: [Bitboard; SQUARE_NB] =
    step_attacks(&[-17, -15, -10, -6, 6, 10, 15, 17]);

/// King attacks from each square.
pub static KING_ATTACKS: [Bitboard; SQUARE_NB] = step_attacks(&[-9, -8, -7, -1, 1, 7, 8, 9]);

/// Pawn attacks from each square, per colour.
pub static PAWN_ATTACKS: [[Bitboard; SQUARE_NB]; 2] =
    [step_attacks(&[7, 9]), step_attacks(&[-7, -9])];

/// The squares a pawn of colour `c` on `sq` attacks.
#[inline(always)]
#[must_use]
pub fn pawn_attacks_from(c: Color, sq: Square) -> Bitboard {
    PAWN_ATTACKS[c.index()][sq.index()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::PieceType;

    #[test]
    fn set_operations_agree_with_membership() {
        let mut b = Bitboard::EMPTY;
        b.set(Square::A1);
        b.set(Square::H8);
        assert_eq!(b.count(), 2);
        assert!(b.contains(Square::A1) && b.contains(Square::H8));
        assert_eq!(b.lsb(), Square::A1);
        assert_eq!(b.msb(), Square::H8);
        assert!(b.more_than_one());
        b.clear(Square::A1);
        assert!(b.exactly_one());
    }

    #[test]
    fn iteration_visits_every_member_once_ascending() {
        let b = FILE_A | RANK_1;
        let seen: Vec<Square> = b.iter().collect();
        assert_eq!(seen.len(), b.count() as usize);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        for sq in &seen {
            assert!(b.contains(*sq));
        }
    }

    #[test]
    fn shifts_drop_wrapping_members() {
        assert_eq!(FILE_H.shift(Direction::East), Bitboard::EMPTY);
        assert_eq!(FILE_A.shift(Direction::West), Bitboard::EMPTY);
        assert_eq!(RANK_8.shift(Direction::North), Bitboard::EMPTY);
        assert_eq!(RANK_1.shift(Direction::South), Bitboard::EMPTY);
        assert_eq!(FILE_A.shift(Direction::East), FILE_B);
    }

    #[test]
    fn step_attack_counts_match_the_geometry() {
        // A knight on a corner reaches 2 squares, in the centre 8; a king 3 and 8.
        assert_eq!(KNIGHT_ATTACKS[Square::A1.index()].count(), 2);
        assert_eq!(KNIGHT_ATTACKS[Square::make(3, 3).index()].count(), 8);
        assert_eq!(KING_ATTACKS[Square::A1.index()].count(), 3);
        assert_eq!(KING_ATTACKS[Square::make(3, 3).index()].count(), 8);
        // 336 is the number of (square, knight-move) pairs on a chessboard.
        let total: u32 = KNIGHT_ATTACKS.iter().map(|b| b.count()).sum();
        assert_eq!(total, 336);
    }

    #[test]
    fn pawn_attacks_face_the_right_way() {
        assert_eq!(
            pawn_attacks_from(Color::White, Square::A1),
            Bitboard::from_square(Square::new(9))
        );
        assert_eq!(PAWN_ATTACKS[Color::White.index()][Square::A8.index()], Bitboard::EMPTY);
        assert_eq!(PAWN_ATTACKS[Color::Black.index()][Square::A1.index()], Bitboard::EMPTY);
        // The set-wise form must agree with the per-square table.
        for sq in Square::all() {
            for c in Color::ALL {
                assert_eq!(Bitboard::from_square(sq).pawn_attacks(c), pawn_attacks_from(c, sq));
            }
        }
    }

    #[test]
    fn distance_is_chebyshev() {
        assert_eq!(distance(Square::A1, Square::A1), 0);
        assert_eq!(distance(Square::A1, Square::H8), 7);
        assert_eq!(distance(Square::A1, Square::new(9)), 1);
        for a in Square::all() {
            for b in Square::all() {
                assert_eq!(distance(a, b), a.distance(b));
            }
        }
    }

    #[test]
    fn piece_types_are_distinct() {
        assert_eq!(PieceType::REAL.len(), 6);
    }
}
