//! Define the engine-wide value domain: colours, pieces, squares, moves, scores.
//!
//! Every type here is a newtype over a fixed-width integer with a named total range.
//! The width is load-bearing: [`Piece`] packs into `Position::board[64]` and [`Square`]
//! indexes every attack table, so widening a type without widening its `*_NB` bound
//! turns a bounds check into a silently wider table. The `const` block at the foot of this
//! file asserts every such relationship.
//!
//! # One constructor rule, in two tiers
//!
//! A constructor here is one of exactly two kinds, and which one it is decides what it does
//! with an argument outside the range:
//!
//! - **Checked construction** — the argument is a quantity the CALLER computed, so it can be
//!   computed wrong: [`Square::new`], [`Square::make`], [`Color::from_index`],
//!   [`PieceType::from_index`]. These panic, and the panic means a corrupt board rather than
//!   bad input.
//! - **Raw reconstruction** — the argument is an encoding THIS module produced, read back out
//!   of a packed record: [`Move::from_raw`], [`Piece::from_raw`],
//!   [`CastlingRights::from_raw`], [`Bound::from_raw`]. Total where every bit pattern is a
//!   valid encoding, `debug_assert`-ed where it is not.
//!
//! **Neither tier MASKS.** A mask under a name that reads lossless turns a corrupt byte from
//! a transposition entry or a table file into a plausible piece, which is a wrong answer
//! rather than a detected fault — and every caller of these already narrows the value before
//! the call, so the mask was buying nothing at either end.
//!
//! [`PieceType::from_low3`] is the third shape and not an exception to the rule: it is a
//! total function over the three bits it names, table-backed so it has no panic arm to
//! branch on, and it exists because routing `piece_type()` through the partial
//! [`PieceType::from_index`] cost 11.8M instructions on a bench.
//!
//! Golden: `Stockfish/src/types.h`.

use core::fmt;
use core::ops::Not;

/// Number of colours.
pub const COLOR_NB: usize = 2;
/// `NO_PIECE_TYPE` plus the six real piece types.
pub const PIECE_TYPE_NB: usize = 7;
/// Eight per colour, sparse: `colour << 3 | type`.
pub const PIECE_NB: usize = 16;
/// Squares on the board.
pub const SQUARE_NB: usize = 64;
/// Files on the board.
pub const FILE_NB: usize = 8;
/// Ranks on the board.
pub const RANK_NB: usize = 8;
/// Upper bound on legal moves in any reachable position.
pub const MAX_MOVES: usize = 256;
/// Upper bound on search depth in plies.
pub const MAX_PLY: usize = 246;

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// The side to move, and the owner of a piece.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Color {
    White = 0,
    Black = 1,
}

impl Color {
    /// Every colour, in index order. Written as an array so a loop over colours is a
    /// slice iteration rather than a hand-rolled counter that can drift from `COLOR_NB`.
    pub const ALL: [Color; COLOR_NB] = [Color::White, Color::Black];

    /// Reconstruct a colour from its index.
    ///
    /// # Panics
    /// Panics when `i` is not 0 or 1. Every caller derives `i` from a colour bit, so a
    /// panic here is a corrupted board, not bad input.
    #[inline(always)]
    #[must_use]
    pub const fn from_index(i: usize) -> Color {
        match i {
            0 => Color::White,
            1 => Color::Black,
            _ => panic!("colour index out of range"),
        }
    }

    /// Index this colour into a `[T; COLOR_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl Not for Color {
    type Output = Color;

    /// The other side. Upstream spells this `~c`.
    #[inline(always)]
    fn not(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

// ---------------------------------------------------------------------------
// Piece type and piece
// ---------------------------------------------------------------------------

/// A piece kind with no colour attached.
///
/// `None` doubles as upstream's `ALL_PIECES`: both are 0, and `Position::by_type[0]`
/// is deliberately the occupancy of every piece rather than an empty set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum PieceType {
    None = 0,
    Pawn = 1,
    Knight = 2,
    Bishop = 3,
    Rook = 4,
    Queen = 5,
    King = 6,
}

impl PieceType {
    /// Upstream's `ALL_PIECES`: index 0 of the by-type occupancy table.
    pub const ALL_PIECES: PieceType = PieceType::None;

    /// The six real piece types in generation order.
    pub const REAL: [PieceType; 6] = [
        PieceType::Pawn,
        PieceType::Knight,
        PieceType::Bishop,
        PieceType::Rook,
        PieceType::Queen,
        PieceType::King,
    ];

    /// Reconstruct a piece type from its index.
    ///
    /// # Panics
    /// Panics above `King`. Callers derive the index from a piece's low three bits, so
    /// only 7 can reach this and it means the board holds a piece that does not exist.
    #[inline(always)]
    #[must_use]
    pub const fn from_index(i: usize) -> PieceType {
        match i {
            0 => PieceType::None,
            1 => PieceType::Pawn,
            2 => PieceType::Knight,
            3 => PieceType::Bishop,
            4 => PieceType::Rook,
            5 => PieceType::Queen,
            6 => PieceType::King,
            _ => panic!("piece type index out of range"),
        }
    }

    /// Index this piece type into a `[T; PIECE_TYPE_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The type named by the low three bits of a piece encoding.
    ///
    /// Total where [`PieceType::from_index`] is partial, and that is the point: the low three
    /// bits of a `Piece` are 0..=6 by construction, so the out-of-range arm is unreachable
    /// from here -- but it is a real branch, and routing every `piece_type()` call through it
    /// charged 11.8M instructions on a bench. A table indexed by a value the mask proves is
    /// in range compiles to the mask and one load, which is what zfish's `pc & 7` costs.
    ///
    /// Seven names no piece; it reads as [`PieceType::None`] rather than panicking, and no
    /// valid encoding produces it.
    #[inline(always)]
    #[must_use]
    pub const fn from_low3(bits: u8) -> PieceType {
        const BY_LOW3: [PieceType; 8] = [
            PieceType::None,
            PieceType::Pawn,
            PieceType::Knight,
            PieceType::Bishop,
            PieceType::Rook,
            PieceType::Queen,
            PieceType::King,
            PieceType::None,
        ];
        BY_LOW3[(bits & 7) as usize]
    }

    /// The character used for this type in FEN and in `d` output, upper case.
    #[must_use]
    pub const fn to_char(self) -> u8 {
        match self {
            PieceType::None => b' ',
            PieceType::Pawn => b'P',
            PieceType::Knight => b'N',
            PieceType::Bishop => b'B',
            PieceType::Rook => b'R',
            PieceType::Queen => b'Q',
            PieceType::King => b'K',
        }
    }

    /// Parse a FEN piece letter, case-insensitively. `None` for anything else.
    #[must_use]
    pub const fn from_char(c: u8) -> Option<PieceType> {
        match c.to_ascii_uppercase() {
            b'P' => Some(PieceType::Pawn),
            b'N' => Some(PieceType::Knight),
            b'B' => Some(PieceType::Bishop),
            b'R' => Some(PieceType::Rook),
            b'Q' => Some(PieceType::Queen),
            b'K' => Some(PieceType::King),
            _ => None,
        }
    }
}

/// A piece with its colour, encoded as `colour << 3 | type`.
///
/// 7 and 15 are unused. The gap keeps the colour bit at a fixed position, so
/// [`Piece::color`] is a shift rather than a table lookup.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Piece(u8);

impl Piece {
    pub const NONE: Piece = Piece(0);
    pub const W_PAWN: Piece = Piece(1);
    pub const W_KNIGHT: Piece = Piece(2);
    pub const W_BISHOP: Piece = Piece(3);
    pub const W_ROOK: Piece = Piece(4);
    pub const W_QUEEN: Piece = Piece(5);
    pub const W_KING: Piece = Piece(6);
    pub const B_PAWN: Piece = Piece(9);
    pub const B_KNIGHT: Piece = Piece(10);
    pub const B_BISHOP: Piece = Piece(11);
    pub const B_ROOK: Piece = Piece(12);
    pub const B_QUEEN: Piece = Piece(13);
    pub const B_KING: Piece = Piece(14);

    /// Build `colour << 3 | type`.
    #[inline(always)]
    #[must_use]
    pub const fn new(c: Color, pt: PieceType) -> Piece {
        Piece(((c as u8) << 3) | pt as u8)
    }

    /// Reconstruct from the raw encoding, as read back out of a packed record.
    ///
    /// Raw reconstruction, per the rule at the top of this file: `v` must already be a piece
    /// encoding. `DirtyThreat` masks its nibble out of a `u32` and the Syzygy header reads
    /// one out of a byte, so both arrive narrowed; masking again here would only convert a
    /// value that ISN'T a piece encoding into one that looks like one.
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> Piece {
        debug_assert!((v as usize) < PIECE_NB, "not a piece encoding");
        Piece(v)
    }

    /// The raw `colour << 3 | type` byte, for the packed NNUE records.
    #[inline(always)]
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// Index this piece into a `[T; PIECE_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The owning colour. Meaningless for [`Piece::NONE`], which reads as White; every
    /// caller checks for `NONE` first, exactly as upstream does.
    #[inline(always)]
    #[must_use]
    pub const fn color(self) -> Color {
        Color::from_index((self.0 >> 3) as usize)
    }

    /// The kind, with the colour bit masked off.
    #[inline(always)]
    #[must_use]
    pub const fn piece_type(self) -> PieceType {
        PieceType::from_low3(self.0)
    }

    /// True for [`Piece::NONE`].
    #[inline(always)]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The FEN letter: upper case for White, lower for Black.
    #[must_use]
    pub const fn to_char(self) -> u8 {
        if self.is_none() {
            return b' ';
        }
        let c = self.piece_type().to_char();
        match self.color() {
            Color::White => c,
            Color::Black => c.to_ascii_lowercase(),
        }
    }

    /// Parse a FEN piece letter. Case selects the colour.
    #[must_use]
    pub const fn from_char(c: u8) -> Option<Piece> {
        match PieceType::from_char(c) {
            Some(pt) => {
                let color = if c.is_ascii_uppercase() { Color::White } else { Color::Black };
                Some(Piece::new(color, pt))
            }
            None => None,
        }
    }
}

impl fmt::Debug for Piece {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_char() as char)
    }
}

// ---------------------------------------------------------------------------
// Square, file, rank
// ---------------------------------------------------------------------------

/// A board file, `A` through `H`.
///
/// Split from a bare `usize` because a file and a rank are the two halves of a square and
/// were the same type: [`Square::make`] takes them in one order, the Syzygy index arithmetic
/// multiplies them by different constants, and nothing said which was which. They also index
/// eight-element tables, so a caller that has one has a bound the compiler can use.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct File(u8);

impl File {
    pub const A: File = File(0);
    pub const B: File = File(1);
    pub const C: File = File(2);
    pub const D: File = File(3);
    pub const E: File = File(4);
    pub const F: File = File(5);
    pub const G: File = File(6);
    pub const H: File = File(7);

    /// Checked construction, per the rule at the top of this file.
    ///
    /// # Panics
    /// Panics at or above [`FILE_NB`].
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> File {
        assert!(i < FILE_NB, "file out of range");
        File(i as u8)
    }

    /// Index this file into a `[T; FILE_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl File {
    /// The lower-case letter naming this file, as FEN and algebraic notation spell it.
    #[inline(always)]
    #[must_use]
    pub const fn to_char(self) -> u8 {
        b'a' + self.0
    }
}

impl fmt::Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(core::str::from_utf8(&[self.to_char()]).unwrap_or("?"))
    }
}

/// A board rank, 0 for rank 1.
///
/// Numbered from zero because every consumer is an index or a relative distance, and because
/// [`Square::relative_rank`] returns one measured from the moving side's own back rank. See
/// [`File`] for why it is a type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Rank(u8);

impl Rank {
    pub const R1: Rank = Rank(0);
    pub const R2: Rank = Rank(1);
    pub const R3: Rank = Rank(2);
    pub const R4: Rank = Rank(3);
    pub const R5: Rank = Rank(4);
    pub const R6: Rank = Rank(5);
    pub const R7: Rank = Rank(6);
    pub const R8: Rank = Rank(7);

    /// Checked construction, per the rule at the top of this file.
    ///
    /// # Panics
    /// Panics at or above [`RANK_NB`].
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> Rank {
        assert!(i < RANK_NB, "rank out of range");
        Rank(i as u8)
    }

    /// Index this rank into a `[T; RANK_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl Rank {
    /// The digit naming this rank, as FEN and algebraic notation spell it.
    #[inline(always)]
    #[must_use]
    pub const fn to_char(self) -> u8 {
        b'1' + self.0
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(core::str::from_utf8(&[self.to_char()]).unwrap_or("?"))
    }
}

/// A board square, ordered A1..H8 rank-major so `sq >> 3` is the rank and `sq & 7` the
/// file.
///
/// **A `Square` is always a real square, 0..=63.** "A square, or nothing" is
/// [`SquareOrNone`], which is a different type. They were one type with a 65th value, and
/// that made `is_ok()` a runtime test every consumer had to remember: the en-passant target,
/// the castling rook table and the search's previous-move square are all optional, and
/// nothing distinguished them from a square that had been checked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Square(u8);

impl Square {
    pub const A1: Square = Square(0);
    pub const H1: Square = Square(7);
    pub const A8: Square = Square(56);
    pub const H8: Square = Square(63);
    /// Reconstruct from an index in 0..64.
    ///
    /// # Panics
    /// Panics at or above [`SQUARE_NB`]. A square index is always derived from a bitboard
    /// bit or a file/rank pair, so out of range means the board is corrupt.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> Square {
        assert!(i < SQUARE_NB, "square index out of range");
        Square(i as u8)
    }

    /// Build from a file and a rank.
    ///
    /// Both are checked at construction, so this needs no assertion of its own -- which is
    /// the point of them being types: the bound is discharged once, where the value is made,
    /// rather than at every place one is used.
    #[inline(always)]
    #[must_use]
    pub const fn make(file: File, rank: Rank) -> Square {
        Square((rank.index() * 8 + file.index()) as u8)
    }

    /// Index this square into a `[T; SQUARE_NB]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The raw byte, including the 64 sentinel, for the packed NNUE records.
    #[inline(always)]
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// This square, in the type that can also be nothing.
    #[inline(always)]
    #[must_use]
    pub const fn some(self) -> SquareOrNone {
        SquareOrNone(self.0)
    }

    /// The file this square stands on.
    #[inline(always)]
    #[must_use]
    pub const fn file(self) -> File {
        File(self.0 & 7)
    }

    /// The rank this square stands on.
    #[inline(always)]
    #[must_use]
    pub const fn rank(self) -> Rank {
        Rank(self.0 >> 3)
    }

    /// Mirror across the horizontal axis: a1 becomes a8.
    #[inline(always)]
    #[must_use]
    pub const fn flip_rank(self) -> Square {
        Square(self.0 ^ 0b0011_1000)
    }

    /// Mirror across the vertical axis: a1 becomes h1.
    #[inline(always)]
    #[must_use]
    pub const fn flip_file(self) -> Square {
        Square(self.0 ^ 0b111)
    }

    /// The rank seen from `c`'s side: 0 is that colour's own back rank, so pawn logic
    /// can be written once for both colours.
    #[inline(always)]
    #[must_use]
    pub const fn relative_rank(self, c: Color) -> Rank {
        Rank(self.rank().0 ^ ((c as u8) * 7))
    }

    /// The square as `c` sees it: identity for White, vertically mirrored for Black.
    #[inline(always)]
    #[must_use]
    pub const fn relative(self, c: Color) -> Square {
        Square(self.0 ^ ((c as u8) * 0b11_1000))
    }

    /// Step one square in direction `d`, where the caller knows the step stays on the board.
    ///
    /// Wraps around the board edges exactly as upstream's pointer-free `Square + Direction`
    /// does; every caller of THIS one masks the result with a bitboard that excludes the
    /// wrap, or is a pawn context where the rank cannot be the last. Use [`Square::try_shift`]
    /// where the step may leave the board.
    #[inline(always)]
    #[must_use]
    pub const fn shift(self, d: Direction) -> Square {
        let stepped = (self.0 as i8).wrapping_add(d as i8) as u8;
        debug_assert!((stepped as usize) < SQUARE_NB, "the step left the board");
        Square(stepped)
    }

    /// Step one square in direction `d`, or `None` when the step leaves the board.
    ///
    /// A step that stays in range may still have WRAPPED around a file edge, which is a real
    /// square and the wrong one; the two callers here reject that with a Chebyshev distance
    /// test, exactly as they did when this was an `is_ok()` on the result.
    #[inline(always)]
    #[must_use]
    pub const fn try_shift(self, d: Direction) -> Option<Square> {
        let stepped = self.0 as i16 + d as i16;
        if stepped >= 0 && stepped < SQUARE_NB as i16 { Some(Square(stepped as u8)) } else { None }
    }

    /// The a1-relative distance in ranks and files, as upstream's `distance<Square>`:
    /// the Chebyshev distance between the two squares.
    #[inline(always)]
    #[must_use]
    pub const fn distance(self, other: Square) -> usize {
        let df = (self.file().index() as isize - other.file().index() as isize).unsigned_abs();
        let dr = (self.rank().index() as isize - other.rank().index() as isize).unsigned_abs();
        if df > dr { df } else { dr }
    }

    /// Parse algebraic coordinates such as `e4`. `None` for anything else.
    #[must_use]
    pub const fn from_coords(file: u8, rank: u8) -> Option<Square> {
        if file.is_ascii_lowercase() && file <= b'h' && rank >= b'1' && rank <= b'8' {
            Some(Square::make(File::new((file - b'a') as usize), Rank::new((rank - b'1') as usize)))
        } else {
            None
        }
    }

    /// Every real square, a1 first.
    pub fn all() -> impl Iterator<Item = Square> {
        (0..64u8).map(Square)
    }
}

impl fmt::Display for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Written as two chars rather than as `"{}{}"` over the file and the rank. Those
        // have their own `Display`, and delegating to them runs `core::fmt::write` twice more
        // per square: measured at +507K instructions on `bench 16 1 8`, at BOTH tiers.
        let file = self.file().to_char() as char;
        let rank = self.rank().to_char() as char;
        write!(f, "{file}{rank}")
    }
}

impl fmt::Debug for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A square, or nothing: the en-passant target, a castling rook that a position does not
/// have, the square a previous move arrived on when there was no previous move.
///
/// A distinct type from [`Square`] rather than a 65th `Square` value. The sentinel is one
/// past the last square, so the raw byte still orders and still fits a `u8` — but nothing
/// depends on the number 64 any more. It used to: the doc-comment here claimed a contract
/// with a `DirtyPiece` record that the NNUE accumulator tested against 64. **rfish has no
/// `DirtyPiece`.** Its accumulator diffs recomputed feature sets, so the only raw square
/// bytes in the tree — `DirtyThreat`'s and the feature indexer's — are real squares. The
/// contract had no holder and the assertion defending it proved nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SquareOrNone(u8);

impl SquareOrNone {
    /// No square.
    pub const NONE: SquareOrNone = SquareOrNone(SQUARE_NB as u8);

    /// True when there is no square.
    #[inline(always)]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == Self::NONE.0
    }

    /// True when there is one.
    #[inline(always)]
    #[must_use]
    pub const fn is_some(self) -> bool {
        self.0 != Self::NONE.0
    }

    /// The square, if there is one.
    #[inline(always)]
    #[must_use]
    pub const fn square(self) -> Option<Square> {
        if self.is_some() { Some(Square(self.0)) } else { None }
    }

    /// The square, where the caller has already established there is one.
    ///
    /// # Panics
    /// Panics on [`SquareOrNone::NONE`]. Every caller tests `is_some` first, so a panic here
    /// means the test and the use disagree.
    #[inline(always)]
    #[must_use]
    pub const fn unwrap(self) -> Square {
        assert!(self.is_some(), "no square");
        Square(self.0)
    }

    /// The raw byte, for a packed record.
    #[inline(always)]
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl fmt::Display for SquareOrNone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.square() {
            Some(sq) => fmt::Display::fmt(&sq, f),
            None => f.write_str("-"),
        }
    }
}

impl fmt::Debug for SquareOrNone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A one-square step, as an offset in the rank-major square numbering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i8)]
pub enum Direction {
    North = 8,
    East = 1,
    South = -8,
    West = -1,
    NorthEast = 9,
    SouthEast = -7,
    SouthWest = -9,
    NorthWest = 7,
}

impl Direction {
    /// The forward direction for `c`'s pawns.
    #[inline(always)]
    #[must_use]
    pub const fn pawn_push(c: Color) -> Direction {
        match c {
            Color::White => Direction::North,
            Color::Black => Direction::South,
        }
    }
}

// ---------------------------------------------------------------------------
// Castling
// ---------------------------------------------------------------------------

/// Castling rights, packed as one nibble so the whole right set is a single Zobrist
/// index rather than four.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct CastlingRights(u8);

impl CastlingRights {
    pub const NONE: CastlingRights = CastlingRights(0);
    pub const WHITE_OO: CastlingRights = CastlingRights(1);
    pub const WHITE_OOO: CastlingRights = CastlingRights(2);
    pub const BLACK_OO: CastlingRights = CastlingRights(4);
    pub const BLACK_OOO: CastlingRights = CastlingRights(8);
    pub const ANY: CastlingRights = CastlingRights(15);

    /// The two rights belonging to `c`.
    #[inline(always)]
    #[must_use]
    pub const fn for_color(c: Color) -> CastlingRights {
        CastlingRights(match c {
            Color::White => 3,
            Color::Black => 12,
        })
    }

    /// `c`'s king-side right.
    #[inline(always)]
    #[must_use]
    pub const fn king_side(c: Color) -> CastlingRights {
        CastlingRights(1 << ((c as u8) * 2))
    }

    /// `c`'s queen-side right.
    #[inline(always)]
    #[must_use]
    pub const fn queen_side(c: Color) -> CastlingRights {
        CastlingRights(2 << ((c as u8) * 2))
    }

    /// Index the rights set into a `[T; 16]`.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from the raw nibble.
    ///
    /// Raw reconstruction: `v` must already be a rights nibble. The one caller draws Zobrist
    /// keys over `0..16`, so it is one by construction.
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> CastlingRights {
        debug_assert!(v <= CastlingRights::ANY.0, "not a castling-rights nibble");
        CastlingRights(v)
    }

    /// True when every bit of `other` is present.
    #[inline(always)]
    #[must_use]
    pub const fn contains(self, other: CastlingRights) -> bool {
        self.0 & other.0 == other.0
    }

    /// True when no right is set.
    #[inline(always)]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The union of two right sets.
    #[inline(always)]
    #[must_use]
    pub const fn union(self, other: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 | other.0)
    }

    /// `self` with every bit of `other` cleared.
    #[inline(always)]
    #[must_use]
    pub const fn without(self, other: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 & !other.0)
    }

    /// The intersection of two right sets.
    #[inline(always)]
    #[must_use]
    pub const fn intersect(self, other: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 & other.0)
    }
}

// ---------------------------------------------------------------------------
// Moves
// ---------------------------------------------------------------------------

/// How a move changes the board beyond moving one piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MoveType {
    Normal = 0,
    Promotion = 1,
    EnPassant = 2,
    Castling = 3,
}

/// A move packed into 16 bits: `type << 14 | (promo - Knight) << 12 | from << 6 | to`.
///
/// [`Move::NONE`] and [`Move::NULL`] are the two encodings with `from == to`, which no
/// legal move can produce, so they never collide with a real move.
///
/// A castling move encodes the ROOK's square as `to`, not the king's destination —
/// upstream's king-takes-rook convention, which is what makes Chess960 castling fit the
/// same 16 bits as every other move.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Move(u16);

impl Move {
    pub const NONE: Move = Move(0);
    pub const NULL: Move = Move(65);

    /// A plain move from one square to another.
    #[inline(always)]
    #[must_use]
    pub const fn new(from: Square, to: Square) -> Move {
        Move(((from.0 as u16) << 6) | to.0 as u16)
    }

    /// A move with an explicit type, and a promotion piece when the type is
    /// [`MoveType::Promotion`].
    #[inline(always)]
    #[must_use]
    pub const fn typed(ty: MoveType, from: Square, to: Square, promo: PieceType) -> Move {
        Move(
            ((ty as u16) << 14)
                | ((promo as u16).wrapping_sub(PieceType::Knight as u16) << 12)
                | ((from.0 as u16) << 6)
                | to.0 as u16,
        )
    }

    /// Reconstruct from the raw 16-bit encoding, as stored in the transposition table.
    ///
    /// Raw reconstruction, and TOTAL: the encoding uses all sixteen bits — two for the type,
    /// two for the promotion piece and six for each square — so every `u16` names a move.
    /// That is what a `from_raw` should look like when it can, and it needs no assertion.
    ///
    /// Total is not the same as legal. A word out of a corrupt table decodes to some move,
    /// and the search checks it against the position before playing it.
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u16) -> Move {
        Move(v)
    }

    /// The raw 16-bit encoding.
    #[inline(always)]
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// The square the moving piece left.
    #[inline(always)]
    #[must_use]
    pub const fn from(self) -> Square {
        Square(((self.0 >> 6) & 0x3F) as u8)
    }

    /// The destination square — for castling, the rook's square.
    #[inline(always)]
    #[must_use]
    pub const fn to(self) -> Square {
        Square((self.0 & 0x3F) as u8)
    }

    /// `from` and `to` packed together, as upstream's `from_to()`: the 12-bit key the
    /// butterfly history tables are indexed by.
    #[inline(always)]
    #[must_use]
    pub const fn from_to(self) -> usize {
        (self.0 & 0x0FFF) as usize
    }

    /// What kind of move this is.
    #[inline(always)]
    #[must_use]
    pub const fn move_type(self) -> MoveType {
        match (self.0 >> 14) & 3 {
            0 => MoveType::Normal,
            1 => MoveType::Promotion,
            2 => MoveType::EnPassant,
            _ => MoveType::Castling,
        }
    }

    /// The promotion piece. Only meaningful when the type is [`MoveType::Promotion`].
    #[inline(always)]
    #[must_use]
    pub const fn promotion_type(self) -> PieceType {
        PieceType::from_low3((((self.0 >> 12) & 3) as u8) + PieceType::Knight as u8)
    }

    /// True unless this is [`Move::NONE`] or [`Move::NULL`].
    ///
    /// Upstream's `is_ok()`: the only two encodings with `from == to`.
    #[inline(always)]
    #[must_use]
    pub const fn is_ok(self) -> bool {
        Move::NONE.0 != self.0 && Move::NULL.0 != self.0
    }

    /// True for anything other than [`Move::NONE`] — upstream's `bool(move)`.
    ///
    /// Distinct from [`Move::is_ok`], which also rejects [`Move::NULL`]. A null move is a
    /// real move for the purposes of "is there a move here", and the search relies on that
    /// distinction when it looks back at what the previous ply played.
    #[inline(always)]
    #[must_use]
    pub const fn is_some(self) -> bool {
        self.0 != Move::NONE.0
    }

    /// True for [`Move::NONE`].
    #[inline(always)]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            return f.write_str("(none)");
        }
        if self.0 == Move::NULL.0 {
            return f.write_str("0000");
        }
        write!(f, "{}{}", self.from(), self.to())?;
        if self.move_type() == MoveType::Promotion {
            write!(f, "{}", self.promotion_type().to_char().to_ascii_lowercase() as char)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A centipawn-domain score. `i32` throughout, matching upstream's `Value`.
///
/// The operators form an AFFINE space over `i32`, which is what a score actually is: a
/// difference of two scores is a MARGIN and not a score, while a score offset by a margin is
/// a score again. So `Sub<Value>` yields `i32` and `Sub<i32>` yields `Value`, and the two
/// coexist because they differ in their right-hand side.
///
/// There is no `From<i32>` and no `Add<Value>`. Adding two scores is meaningless — upstream
/// never does it — and a conversion is a place a reader should be able to see.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Value(i32);

impl Value {
    /// Build a score from a number. Explicit, so a raw integer entering the score domain is
    /// visible at the line that does it.
    #[inline(always)]
    #[must_use]
    pub const fn new(v: i32) -> Value {
        Value(v)
    }

    /// The score as a number, for the reporting and quantisation paths that need one.
    #[inline(always)]
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// Offset by a margin, in a `const` context. `Sub<i32>` is not a `const fn`, and the
    /// tablebase score table is a `const`.
    #[inline(always)]
    #[must_use]
    pub const fn offset(self, m: i32) -> Value {
        Value(self.0 + m)
    }

    /// Negated, in a `const` context, for the same reason as [`Value::offset`].
    #[inline(always)]
    #[must_use]
    pub const fn negate(self) -> Value {
        Value(-self.0)
    }

    /// The magnitude, ignoring which side the score favours.
    #[inline(always)]
    #[must_use]
    pub const fn abs(self) -> Value {
        Value(self.0.abs())
    }
}

impl core::ops::Neg for Value {
    type Output = Value;
    #[inline(always)]
    fn neg(self) -> Value {
        Value(-self.0)
    }
}

impl core::ops::Add<i32> for Value {
    type Output = Value;
    #[inline(always)]
    fn add(self, m: i32) -> Value {
        Value(self.0 + m)
    }
}

impl core::ops::Sub<i32> for Value {
    type Output = Value;
    #[inline(always)]
    fn sub(self, m: i32) -> Value {
        Value(self.0 - m)
    }
}

/// The difference of two scores is a MARGIN, which is not a score.
impl core::ops::Sub<Value> for Value {
    type Output = i32;
    #[inline(always)]
    fn sub(self, other: Value) -> i32 {
        self.0 - other.0
    }
}

impl core::ops::Mul<i32> for Value {
    type Output = Value;
    #[inline(always)]
    fn mul(self, k: i32) -> Value {
        Value(self.0 * k)
    }
}

/// Scaling written the other way round, as upstream spells several of its margins.
impl core::ops::Mul<Value> for i32 {
    type Output = Value;
    #[inline(always)]
    fn mul(self, v: Value) -> Value {
        Value(self * v.0)
    }
}

impl core::ops::Div<i32> for Value {
    type Output = Value;
    #[inline(always)]
    fn div(self, k: i32) -> Value {
        Value(self.0 / k)
    }
}

impl core::ops::AddAssign<i32> for Value {
    #[inline(always)]
    fn add_assign(&mut self, m: i32) {
        self.0 += m;
    }
}

impl core::ops::SubAssign<i32> for Value {
    #[inline(always)]
    fn sub_assign(&mut self, m: i32) {
        self.0 -= m;
    }
}

/// Widen for the quantisation arithmetic, which must not overflow an `i32`.
impl From<Value> for i64 {
    #[inline(always)]
    fn from(v: Value) -> i64 {
        i64::from(v.0)
    }
}

/// Widen for the win-rate model, which is fitted in floating point.
impl From<Value> for f64 {
    #[inline(always)]
    fn from(v: Value) -> f64 {
        f64::from(v.0)
    }
}

/// Compare a score against a bare MARGIN, which is what upstream writes throughout the
/// pruning conditions: `beta >= -2000` is a test, not a conversion, so it does not put a raw
/// integer into the score domain and does not need to be spelled out.
impl PartialEq<i32> for Value {
    #[inline(always)]
    fn eq(&self, m: &i32) -> bool {
        self.0 == *m
    }
}

impl PartialOrd<i32> for Value {
    #[inline(always)]
    fn partial_cmp(&self, m: &i32) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(m)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub const VALUE_ZERO: Value = Value(0);
pub const VALUE_DRAW: Value = Value(0);
pub const VALUE_NONE: Value = Value(32002);
pub const VALUE_INFINITE: Value = Value(32001);
pub const VALUE_MATE: Value = Value(32000);
pub const VALUE_MATE_IN_MAX_PLY: Value = Value(VALUE_MATE.0 - MAX_PLY as i32);
pub const VALUE_MATED_IN_MAX_PLY: Value = Value(-VALUE_MATE_IN_MAX_PLY.0);
pub const VALUE_TB: Value = Value(VALUE_MATE_IN_MAX_PLY.0 - 1);
pub const VALUE_TB_WIN_IN_MAX_PLY: Value = Value(VALUE_TB.0 - MAX_PLY as i32);
pub const VALUE_TB_LOSS_IN_MAX_PLY: Value = Value(-VALUE_TB_WIN_IN_MAX_PLY.0);

pub const PAWN_VALUE: Value = Value(208);
pub const KNIGHT_VALUE: Value = Value(781);
pub const BISHOP_VALUE: Value = Value(825);
pub const ROOK_VALUE: Value = Value(1276);
pub const QUEEN_VALUE: Value = Value(2538);

/// Value of a raw [`Piece`], indexed without masking off the colour bit.
///
/// Upstream's `PieceValue[PIECE_NB]`, which `see_ge` indexes straight off `piece_on()`.
/// The colour bit selects a mirrored half, so the `& 7` a [`PieceType`] lookup would need
/// never happens on that hot path.
pub const PIECE_VALUE: [Value; PIECE_NB] = [
    VALUE_ZERO,
    PAWN_VALUE,
    KNIGHT_VALUE,
    BISHOP_VALUE,
    ROOK_VALUE,
    QUEEN_VALUE,
    VALUE_ZERO,
    VALUE_ZERO,
    VALUE_ZERO,
    PAWN_VALUE,
    KNIGHT_VALUE,
    BISHOP_VALUE,
    ROOK_VALUE,
    QUEEN_VALUE,
    VALUE_ZERO,
    VALUE_ZERO,
];

/// Value of a piece type.
#[inline(always)]
#[must_use]
pub const fn piece_type_value(pt: PieceType) -> Value {
    PIECE_VALUE[pt as usize]
}

/// Value of a piece, colour bit and all.
#[inline(always)]
#[must_use]
pub const fn piece_value(pc: Piece) -> Value {
    PIECE_VALUE[pc.index()]
}

/// The score for delivering mate in `ply` plies.
#[inline(always)]
#[must_use]
pub const fn mate_in(ply: Ply) -> Value {
    Value(VALUE_MATE.0 - ply.get())
}

/// The score for being mated in `ply` plies.
#[inline(always)]
#[must_use]
pub const fn mated_in(ply: Ply) -> Value {
    Value(ply.get() - VALUE_MATE.0)
}

/// True unless the value is the "no score" sentinel.
#[inline(always)]
#[must_use]
pub const fn is_valid(v: Value) -> bool {
    v.0 != VALUE_NONE.0
}

/// True when the value is a proven win — a mate or a tablebase win.
#[inline(always)]
#[must_use]
pub const fn is_win(v: Value) -> bool {
    v.0 >= VALUE_TB_WIN_IN_MAX_PLY.0
}

/// True when the value is a proven loss.
#[inline(always)]
#[must_use]
pub const fn is_loss(v: Value) -> bool {
    v.0 <= VALUE_TB_LOSS_IN_MAX_PLY.0
}

/// True when the value settles the game either way.
///
/// The distinction from an ordinary score matters throughout the search: a decisive score
/// must not be blended, widened or scaled, because it is a fact rather than an estimate.
#[inline(always)]
#[must_use]
pub const fn is_decisive(v: Value) -> bool {
    is_win(v) || is_loss(v)
}

/// True when the value is a mate score, as distinct from a tablebase win.
#[inline(always)]
#[must_use]
pub const fn is_mate(v: Value) -> bool {
    v.0 >= VALUE_MATE_IN_MAX_PLY.0
}

/// True when the value is a being-mated score.
#[inline(always)]
#[must_use]
pub const fn is_mated(v: Value) -> bool {
    v.0 <= VALUE_MATED_IN_MAX_PLY.0
}

/// True when the value is a mate score for either side.
#[inline(always)]
#[must_use]
pub const fn is_mate_or_mated(v: Value) -> bool {
    is_mate(v) || is_mated(v)
}

// ---------------------------------------------------------------------------
// Plies
// ---------------------------------------------------------------------------

/// Distance from the ROOT of the current search, in plies.
///
/// Split from [`GamePly`] because the two were both `i32` and both spelled `ply`, and they
/// are not the same quantity: this one is zero at the root of every search, that one counts
/// from the start of the game. `Limits.ply` is a [`GamePly`] and feeds the time manager;
/// `Position::is_draw(ply)` takes a [`Ply`] and decides a repetition. Neither function
/// rejects the other's argument today.
///
/// The operator set is deliberately closed and small — a step forward, a step back, an
/// ordering, and an index. Everything else a ply participates in is a mate distance or a
/// transposition adjustment, and those take a `Ply` directly rather than an `i32`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Ply(i32);

impl Ply {
    /// The root of a search.
    pub const ROOT: Ply = Ply(0);
    /// The deepest ply the search may reach, as [`MAX_PLY`] names it.
    pub const MAX: Ply = Ply(MAX_PLY as i32);

    /// Checked construction, per the rule at the top of this file.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: i32) -> Ply {
        debug_assert!(i >= 0, "a ply is a distance from the root");
        Ply(i)
    }

    /// The distance as a number, for the reporting and time paths that need one.
    ///
    /// This is the escape hatch, and it is narrow on purpose: `seldepth` is reported as an
    /// integer and the time manager scales by one. A search formula that wants a ply should
    /// take a [`Ply`].
    #[inline(always)]
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// Index a per-ply table, such as the stack or the low-ply history.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// One ply deeper — the child of this node.
    #[inline(always)]
    #[must_use]
    pub const fn next(self) -> Ply {
        Ply(self.0 + 1)
    }

    /// One ply shallower — this node's parent.
    #[inline(always)]
    #[must_use]
    pub const fn prev(self) -> Ply {
        Ply(self.0 - 1)
    }

    /// `n` plies deeper.
    #[inline(always)]
    #[must_use]
    pub const fn offset(self, n: i32) -> Ply {
        Ply(self.0 + n)
    }
}

/// Plies since the START OF THE GAME, as the FEN's move number implies.
///
/// Distinct from [`Ply`]: see that type. This one is what the time manager scales its budget
/// by and what `Position::fen` turns back into a move number.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct GamePly(i32);

impl GamePly {
    /// The start of a game.
    pub const START: GamePly = GamePly(0);

    /// Checked construction, per the rule at the top of this file.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: i32) -> GamePly {
        debug_assert!(i >= 0, "a game ply is a count from the start");
        GamePly(i)
    }

    /// The count as a number, for the move-number arithmetic and the time model.
    #[inline(always)]
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// One ply later.
    #[inline(always)]
    #[must_use]
    pub const fn next(self) -> GamePly {
        GamePly(self.0 + 1)
    }

    /// One ply earlier.
    #[inline(always)]
    #[must_use]
    pub const fn prev(self) -> GamePly {
        GamePly(self.0 - 1)
    }
}

/// A Zobrist hash key.
pub type Key = u64;

/// Node kinds, as the search's generic parameter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeType {
    PV,
    NonPV,
    Root,
}

/// The transposition table's bound kind for a stored score.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Bound {
    None = 0,
    Upper = 1,
    Lower = 2,
    Exact = 3,
}

impl Bound {
    /// Reconstruct from the two bits stored in a TT entry.
    ///
    /// Raw reconstruction. NOT table-backed, unlike [`PieceType::from_low3`], and the
    /// difference is the whole reason that one is: this mapping is the IDENTITY — the
    /// discriminants are 0..=3 and the arms are in that order — so the match compiles to the
    /// mask alone, and a lookup table would put a load where there is currently no memory
    /// access at all. `from_low3` is table-backed because 7 maps to `None`, which no mask
    /// produces.
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> Bound {
        debug_assert!(v < 4, "not a two-bit bound");
        match v & 3 {
            0 => Bound::None,
            1 => Bound::Upper,
            2 => Bound::Lower,
            _ => Bound::Exact,
        }
    }
}

// ---------------------------------------------------------------------------
// Representation, asserted
// ---------------------------------------------------------------------------

/// Assert every representation the module doc calls load-bearing.
///
/// The doc at the top of this file says widening a type without widening its `*_NB` bound
/// turns a bounds check into a silently wider table. Nothing enforced that. These are `const`
/// blocks rather than `#[test]`s because a test can be skipped and a gate can be excused,
/// while a `const` assertion is a build failure on every profile and every target.
///
/// Each one states the RELATIONSHIP, not the number: `size_of::<Piece>() == 1` alone goes
/// stale the day `PIECE_NB` changes, so the bound that implies the width is asserted beside
/// it.
const _: () = {
    // Every domain type is exactly its declared width, with natural alignment. A wider one
    // silently widens `Position::board`, the attack tables and the packed NNUE records.
    assert!(size_of::<Color>() == 1 && align_of::<Color>() == 1);
    assert!(size_of::<PieceType>() == 1 && align_of::<PieceType>() == 1);
    assert!(size_of::<Piece>() == 1 && align_of::<Piece>() == 1);
    assert!(size_of::<Square>() == 1 && align_of::<Square>() == 1);
    assert!(size_of::<Direction>() == 1 && align_of::<Direction>() == 1);
    assert!(size_of::<CastlingRights>() == 1 && align_of::<CastlingRights>() == 1);
    assert!(size_of::<MoveType>() == 1 && align_of::<MoveType>() == 1);
    assert!(size_of::<Move>() == 2 && align_of::<Move>() == 2);
    assert!(size_of::<Bound>() == 1 && align_of::<Bound>() == 1);

    // The width can hold the bound. Each `*_NB` is a count, so the largest index is one less.
    assert!(COLOR_NB - 1 <= u8::MAX as usize);
    assert!(PIECE_TYPE_NB - 1 <= u8::MAX as usize);
    assert!(PIECE_NB - 1 <= u8::MAX as usize);
    // `Square` must hold the sentinel too, which sits one past the last real square.
    assert!(SQUARE_NB <= u8::MAX as usize);

    // `Piece` is `colour << 3 | type`, so the piece space is eight slots per colour and the
    // low three bits must hold every piece type. `from_low3` masks with 7 on that basis.
    assert!(PIECE_NB == COLOR_NB * 8);
    assert!(PIECE_TYPE_NB <= 8);

    // A bitboard is one bit per square. Widening `SQUARE_NB` without widening the word would
    // drop squares off the top of every set with no diagnostic.
    assert!(SQUARE_NB == u64::BITS as usize);

    // The square numbering is rank-major over an 8x8 board: `sq >> 3` is the rank and `sq & 7`
    // the file, which `Square::file`, `rank` and `make` all depend on.
    assert!(FILE_NB * RANK_NB == SQUARE_NB);
    assert!(FILE_NB == 8 && RANK_NB == 8);

    // `SquareOrNone` packs its sentinel one past the last square, so the raw byte still fits a
    // `u8` and still orders after every square. NOTHING in this tree depends on the number
    // being 64 -- see the type's own doc for the contract that used to be claimed here and
    // had no holder.
    assert!(SquareOrNone::NONE.raw() as usize == SQUARE_NB);
    assert!(size_of::<SquareOrNone>() == 1 && align_of::<SquareOrNone>() == 1);

    // The 16 bits of a `Move` are `type << 14 | (promo - Knight) << 12 | from << 6 | to`.
    // Six bits per square, two for the promotion piece, two for the type.
    assert!(SQUARE_NB <= 1 << 6);
    assert!((PieceType::Queen as usize) - (PieceType::Knight as usize) < (1 << 2));

    // The castling nibble is one Zobrist index, so the rights set must fit in four bits.
    assert!(CastlingRights::ANY.index() < 16);

    // `MAX_PLY` is compared against a ply held as `i32` throughout the search, and mate scores
    // are `VALUE_MATE - ply`. Both break silently if the bound outgrows the score domain.
    assert!(MAX_PLY < i32::MAX as usize);
    assert!(VALUE_MATE_IN_MAX_PLY.get() > VALUE_TB.get());
    assert!(VALUE_TB_WIN_IN_MAX_PLY.get() > 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Square` is a real square and a [`SquareOrNone`] may be neither; the two must not
    /// collide on the raw byte, because the Syzygy prober and the NNUE feature indexer both
    /// read `Square::raw()` and both would accept the sentinel as a square.
    #[test]
    fn the_sentinel_is_not_a_square() {
        assert!(SquareOrNone::NONE.is_none());
        assert_eq!(SquareOrNone::NONE.square(), None);
        for sq in Square::all() {
            assert_ne!(sq.raw(), SquareOrNone::NONE.raw());
            assert_eq!(sq.some().square(), Some(sq));
        }
    }

    #[test]
    fn square_coordinates_round_trip() {
        for sq in Square::all() {
            assert_eq!(Square::make(sq.file(), sq.rank()), sq);
            assert_eq!(sq.flip_rank().flip_rank(), sq);
            assert_eq!(sq.flip_file().flip_file(), sq);
        }
        assert_eq!(Square::A1.to_string(), "a1");
        assert_eq!(Square::H8.to_string(), "h8");
    }

    #[test]
    fn relative_rank_is_own_side_first() {
        assert_eq!(Square::A1.relative_rank(Color::White), Rank::R1);
        assert_eq!(Square::A1.relative_rank(Color::Black), Rank::R8);
        assert_eq!(Square::A8.relative_rank(Color::Black), Rank::R1);
    }

    #[test]
    fn piece_encoding_packs_colour_above_type() {
        for c in Color::ALL {
            for pt in PieceType::REAL {
                let pc = Piece::new(c, pt);
                assert_eq!(pc.color(), c);
                assert_eq!(pc.piece_type(), pt);
                assert_eq!(Piece::from_char(pc.to_char()), Some(pc));
            }
        }
        assert!(Piece::NONE.is_none());
    }

    #[test]
    fn move_encoding_round_trips() {
        let m = Move::typed(MoveType::Promotion, Square::A8, Square::H8, PieceType::Queen);
        assert_eq!(m.from(), Square::A8);
        assert_eq!(m.to(), Square::H8);
        assert_eq!(m.move_type(), MoveType::Promotion);
        assert_eq!(m.promotion_type(), PieceType::Queen);
        assert!(m.is_ok());
        assert!(!Move::NONE.is_ok());
        assert!(!Move::NULL.is_ok());
        assert_eq!(format!("{m:?}"), "a8h8q");
    }

    /// The raw reconstructors used to mask, so a corrupt byte became a plausible value with
    /// no diagnostic anywhere. Each now refuses one in an unoptimised build. These run under
    /// the `gate` profile, which has `overflow-checks` and `debug_assertions` on.
    #[test]
    #[should_panic(expected = "not a piece encoding")]
    fn piece_from_raw_refuses_a_non_encoding() {
        let _ = Piece::from_raw(0x1F);
    }

    #[test]
    #[should_panic(expected = "not a castling-rights nibble")]
    fn castling_from_raw_refuses_a_non_nibble() {
        let _ = CastlingRights::from_raw(0x10);
    }

    #[test]
    #[should_panic(expected = "not a two-bit bound")]
    fn bound_from_raw_refuses_more_than_two_bits() {
        let _ = Bound::from_raw(4);
    }

    /// `Move::from_raw` is the one raw reconstructor that is honestly total: the encoding
    /// fills all sixteen bits, so it must accept every word rather than assert.
    #[test]
    fn move_from_raw_is_total() {
        for v in 0..=u16::MAX {
            assert_eq!(Move::from_raw(v).raw(), v);
        }
    }

    #[test]
    fn null_and_none_are_the_from_equals_to_encodings() {
        assert_eq!(Move::NONE.from(), Move::NONE.to());
        assert_eq!(Move::NULL.from(), Move::NULL.to());
    }
}
