//! Define the engine-wide value domain: colours, pieces, squares, moves, scores.
//!
//! Every type here is a newtype over a fixed-width integer with a named total range.
//! The width is load-bearing: [`Piece`] packs into `Position::board[64]` and [`Square`]
//! indexes every attack table, so widening a type without widening its `*_NB` bound
//! turns a bounds check into a silently wider table.
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
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> Piece {
        Piece(v & 0xF)
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

/// A board square, ordered A1..H8 rank-major so `sq >> 3` is the rank and `sq & 7` the
/// file.
///
/// The valid range is 0..=63 plus [`Square::NONE`] at 64. That sentinel value is a
/// contract with the NNUE feature indexer, which tests raw square bytes against 64.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Square(u8);

impl Square {
    pub const A1: Square = Square(0);
    pub const H1: Square = Square(7);
    pub const A8: Square = Square(56);
    pub const H8: Square = Square(63);
    /// The no-square sentinel. Must stay 64: `DirtyPiece` stores raw square bytes and
    /// the accumulator tests them against 64.
    pub const NONE: Square = Square(64);

    /// Reconstruct from an index in 0..64.
    ///
    /// # Panics
    /// Panics above 64. A square index is always derived from a bitboard bit or a
    /// file/rank pair, so out of range means the board is corrupt.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> Square {
        assert!(i <= 64, "square index out of range");
        Square(i as u8)
    }

    /// Build from a file and a rank, each in 0..8.
    #[inline(always)]
    #[must_use]
    pub const fn make(file: usize, rank: usize) -> Square {
        debug_assert!(file < 8 && rank < 8);
        Square((rank * 8 + file) as u8)
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

    /// True for [`Square::NONE`].
    #[inline(always)]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 64
    }

    /// True for a real board square.
    #[inline(always)]
    #[must_use]
    pub const fn is_ok(self) -> bool {
        self.0 < 64
    }

    /// The file, 0 for the a-file.
    #[inline(always)]
    #[must_use]
    pub const fn file(self) -> usize {
        (self.0 & 7) as usize
    }

    /// The rank, 0 for rank 1.
    #[inline(always)]
    #[must_use]
    pub const fn rank(self) -> usize {
        (self.0 >> 3) as usize
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
    pub const fn relative_rank(self, c: Color) -> usize {
        self.rank() ^ ((c as usize) * 7)
    }

    /// The square as `c` sees it: identity for White, vertically mirrored for Black.
    #[inline(always)]
    #[must_use]
    pub const fn relative(self, c: Color) -> Square {
        Square(self.0 ^ ((c as u8) * 0b11_1000))
    }

    /// Step one square in direction `d`.
    ///
    /// Wraps around the board edges exactly as upstream's pointer-free `Square + Direction`
    /// does; every caller masks the result with a bitboard that excludes the wrap.
    #[inline(always)]
    #[must_use]
    pub const fn shift(self, d: Direction) -> Square {
        Square((self.0 as i8).wrapping_add(d as i8) as u8)
    }

    /// The a1-relative distance in ranks and files, as upstream's `distance<Square>`:
    /// the Chebyshev distance between the two squares.
    #[inline(always)]
    #[must_use]
    pub const fn distance(self, other: Square) -> usize {
        let df = (self.file() as isize - other.file() as isize).unsigned_abs();
        let dr = (self.rank() as isize - other.rank() as isize).unsigned_abs();
        if df > dr { df } else { dr }
    }

    /// Parse algebraic coordinates such as `e4`. `None` for anything else.
    #[must_use]
    pub const fn from_coords(file: u8, rank: u8) -> Option<Square> {
        if file.is_ascii_lowercase() && file <= b'h' && rank >= b'1' && rank <= b'8' {
            Some(Square::make((file - b'a') as usize, (rank - b'1') as usize))
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
        if self.is_none() {
            return f.write_str("-");
        }
        let file = (b'a' + self.file() as u8) as char;
        let rank = (b'1' + self.rank() as u8) as char;
        write!(f, "{file}{rank}")
    }
}

impl fmt::Debug for Square {
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
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> CastlingRights {
        CastlingRights(v & 0xF)
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
pub type Value = i32;

pub const VALUE_ZERO: Value = 0;
pub const VALUE_DRAW: Value = 0;
pub const VALUE_NONE: Value = 32002;
pub const VALUE_INFINITE: Value = 32001;
pub const VALUE_MATE: Value = 32000;
pub const VALUE_MATE_IN_MAX_PLY: Value = VALUE_MATE - MAX_PLY as Value;
pub const VALUE_MATED_IN_MAX_PLY: Value = -VALUE_MATE_IN_MAX_PLY;
pub const VALUE_TB: Value = VALUE_MATE_IN_MAX_PLY - 1;
pub const VALUE_TB_WIN_IN_MAX_PLY: Value = VALUE_TB - MAX_PLY as Value;
pub const VALUE_TB_LOSS_IN_MAX_PLY: Value = -VALUE_TB_WIN_IN_MAX_PLY;

pub const PAWN_VALUE: Value = 208;
pub const KNIGHT_VALUE: Value = 781;
pub const BISHOP_VALUE: Value = 825;
pub const ROOK_VALUE: Value = 1276;
pub const QUEEN_VALUE: Value = 2538;

/// Value of a raw [`Piece`], indexed without masking off the colour bit.
///
/// Upstream's `PieceValue[PIECE_NB]`, which `see_ge` indexes straight off `piece_on()`.
/// The colour bit selects a mirrored half, so the `& 7` a [`PieceType`] lookup would need
/// never happens on that hot path.
pub const PIECE_VALUE: [Value; PIECE_NB] = [
    0,
    PAWN_VALUE,
    KNIGHT_VALUE,
    BISHOP_VALUE,
    ROOK_VALUE,
    QUEEN_VALUE,
    0,
    0,
    0,
    PAWN_VALUE,
    KNIGHT_VALUE,
    BISHOP_VALUE,
    ROOK_VALUE,
    QUEEN_VALUE,
    0,
    0,
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
pub const fn mate_in(ply: i32) -> Value {
    VALUE_MATE - ply
}

/// The score for being mated in `ply` plies.
#[inline(always)]
#[must_use]
pub const fn mated_in(ply: i32) -> Value {
    ply - VALUE_MATE
}

/// True unless the value is the "no score" sentinel.
#[inline(always)]
#[must_use]
pub const fn is_valid(v: Value) -> bool {
    v != VALUE_NONE
}

/// True when the value is a proven win — a mate or a tablebase win.
#[inline(always)]
#[must_use]
pub const fn is_win(v: Value) -> bool {
    v >= VALUE_TB_WIN_IN_MAX_PLY
}

/// True when the value is a proven loss.
#[inline(always)]
#[must_use]
pub const fn is_loss(v: Value) -> bool {
    v <= VALUE_TB_LOSS_IN_MAX_PLY
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
    v >= VALUE_MATE_IN_MAX_PLY
}

/// True when the value is a being-mated score.
#[inline(always)]
#[must_use]
pub const fn is_mated(v: Value) -> bool {
    v <= VALUE_MATED_IN_MAX_PLY
}

/// True when the value is a mate score for either side.
#[inline(always)]
#[must_use]
pub const fn is_mate_or_mated(v: Value) -> bool {
    is_mate(v) || is_mated(v)
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
    #[inline(always)]
    #[must_use]
    pub const fn from_raw(v: u8) -> Bound {
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

    // `Square::NONE` must stay one past the last square: `DirtyPiece` stores raw square bytes
    // and the accumulator tests them against that value. The unit test below says the same
    // thing about the literal 64; this says it about the bound.
    assert!(Square::NONE.raw() as usize == SQUARE_NB);

    // The 16 bits of a `Move` are `type << 14 | (promo - Knight) << 12 | from << 6 | to`.
    // Six bits per square, two for the promotion piece, two for the type.
    assert!(SQUARE_NB <= 1 << 6);
    assert!((PieceType::Queen as usize) - (PieceType::Knight as usize) < (1 << 2));

    // The castling nibble is one Zobrist index, so the rights set must fit in four bits.
    assert!(CastlingRights::ANY.index() < 16);

    // `MAX_PLY` is compared against a ply held as `i32` throughout the search, and mate scores
    // are `VALUE_MATE - ply`. Both break silently if the bound outgrows the score domain.
    assert!(MAX_PLY < i32::MAX as usize);
    assert!(VALUE_MATE_IN_MAX_PLY > VALUE_TB);
    assert!(VALUE_TB_WIN_IN_MAX_PLY > 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// `DirtyPiece` stores raw square bytes and the NNUE accumulator tests them against
    /// 64. Renumbering [`Square`] without renumbering that constant would silently make
    /// every promotion and capture look like a real square.
    #[test]
    fn no_square_sentinel_is_64() {
        assert_eq!(Square::NONE.raw(), 64);
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
        assert_eq!(Square::A1.relative_rank(Color::White), 0);
        assert_eq!(Square::A1.relative_rank(Color::Black), 7);
        assert_eq!(Square::A8.relative_rank(Color::Black), 0);
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

    #[test]
    fn null_and_none_are_the_from_equals_to_encodings() {
        assert_eq!(Move::NONE.from(), Move::NONE.to());
        assert_eq!(Move::NULL.from(), Move::NULL.to());
    }
}
