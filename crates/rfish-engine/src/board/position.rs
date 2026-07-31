//! The board state: piece placement, the incrementally maintained derived state, and the
//! make/unmake transition.
//!
//! The split matters. [`Position`] holds what a move rewrites in place (the boards, the
//! side to move); [`StateInfo`] holds what a move cannot recompute cheaply on the way
//! back (the captured piece, the castling rights, the Zobrist key). Undo restores by
//! popping a `StateInfo`, never by recomputing — so any field added to `StateInfo` must
//! be written by [`Position::do_move`] before the recursion, or unmake restores a stale
//! value.
//!
//! # The state chain is a `Vec`, not a linked list
//!
//! Upstream chains `StateInfo` through a `previous` pointer that the caller allocates on
//! its own stack frame, and a `Position` that outlives one of those frames is a dangling
//! read. Here the chain is a `Vec<StateInfo>` the position owns: `previous` is `len - 2`,
//! the repetition walk is a backwards slice scan, and the lifetime question does not
//! arise. That is the single largest structural difference from the C++, and it is what
//! lets the whole zone forbid `unsafe`.
//!
//! Golden: `Stockfish/src/position.h`, `Stockfish/src/position.cpp`.

use core::fmt;
use core::fmt::Write as _;

use super::attacks::{
    aligned, between_bb, bishop_attacks, piece_attacks, queen_attacks, rook_attacks,
};
use super::bitboard::{
    Bitboard, KING_ATTACKS, KNIGHT_ATTACKS, pawn_attacks_from, relative_rank_bb,
};
use super::types::{
    COLOR_NB, CastlingRights, Color, Direction, Key, Move, MoveType, PIECE_NB, PIECE_TYPE_NB,
    Piece, PieceType, SQUARE_NB, Square, Value, piece_value,
};
use super::zobrist;

/// The FEN of the standard starting position.
pub const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

/// Per-ply state that a move cannot cheaply undo.
///
/// The fields divide into two groups, exactly as upstream's do. The first is COPIED
/// forward when a move is made and updated incrementally; the second is RECOMPUTED from
/// scratch. Adding a field to the first group without updating it in `do_move` leaves a
/// stale value that no gate necessarily catches.
#[derive(Clone, Debug)]
pub struct StateInfo {
    // ---- copied forward and updated incrementally ----
    /// Hashes the MATERIAL only — every piece of each kind, counted, with no square
    /// information. Syzygy looks its tables up by this.
    pub material_key: Key,
    /// Pawns ONLY, seeded with [`zobrist::no_pawns`] so an empty pawn structure still has
    /// a distinct key.
    pub pawn_key: Key,
    /// Knights and bishops only, kings EXCLUDED.
    pub minor_piece_key: Key,
    /// Every non-pawn of that colour, kings INCLUDED.
    pub non_pawn_key: [Key; COLOR_NB],
    /// Sum of each colour's non-pawn, non-king piece values.
    pub non_pawn_material: [Value; COLOR_NB],
    /// Halfmove clock, in plies.
    pub rule50: i32,
    /// Plies since the last null move, bounding the repetition walk.
    pub plies_from_null: i32,
    /// The en-passant target, or [`Square::NONE`].
    pub ep_square: Square,
    /// The castling rights still available.
    pub castling_rights: CastlingRights,

    // ---- recomputed, never copied ----
    /// The position key.
    pub key: Key,
    /// Pieces of the side NOT to move that give check.
    pub checkers: Bitboard,
    /// Own pieces whose move could expose their own king, per colour.
    pub blockers: [Bitboard; COLOR_NB],
    /// The enemy sliders that would deliver that discovered check.
    pub pinners: [Bitboard; COLOR_NB],
    /// Squares from which a piece of each type would check the enemy king.
    pub check_squares: [Bitboard; PIECE_TYPE_NB],
    /// The piece the move captured, or [`Piece::NONE`].
    pub captured_piece: Piece,
    /// Distance in plies back to the previous occurrence of this position: positive for
    /// the first repetition, NEGATIVE when that earlier occurrence was itself a
    /// repetition, 0 when never repeated. The sign is the whole encoding.
    pub repetition: i32,
}

impl StateInfo {
    /// The zero state, used only as the base of a fresh chain before a FEN is parsed.
    fn empty() -> StateInfo {
        StateInfo {
            material_key: 0,
            pawn_key: 0,
            minor_piece_key: 0,
            non_pawn_key: [0; COLOR_NB],
            non_pawn_material: [0; COLOR_NB],
            rule50: 0,
            plies_from_null: 0,
            ep_square: Square::NONE,
            castling_rights: CastlingRights::NONE,
            key: 0,
            checkers: Bitboard::EMPTY,
            blockers: [Bitboard::EMPTY; COLOR_NB],
            pinners: [Bitboard::EMPTY; COLOR_NB],
            check_squares: [Bitboard::EMPTY; PIECE_TYPE_NB],
            captured_piece: Piece::NONE,
            repetition: 0,
        }
    }

    /// The prefix a move carries forward. Everything from `key` on is recomputed.
    fn carried_forward(&self) -> StateInfo {
        StateInfo {
            material_key: self.material_key,
            pawn_key: self.pawn_key,
            minor_piece_key: self.minor_piece_key,
            non_pawn_key: self.non_pawn_key,
            non_pawn_material: self.non_pawn_material,
            rule50: self.rule50,
            plies_from_null: self.plies_from_null,
            ep_square: self.ep_square,
            castling_rights: self.castling_rights,
            ..StateInfo::empty()
        }
    }
}

/// Why a FEN record was rejected, in upstream's words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FenError {
    /// The board field is missing, malformed, or does not describe 64 squares.
    Board,
    /// The side-to-move field is neither `w` nor `b`.
    SideToMove,
    /// A castling letter names no rook.
    Castling,
    /// The en-passant field is not a square or `-`.
    EnPassant,
    /// A colour has no king, or more than one.
    Kings,
    /// The side not to move is already in check.
    OppositeCheck,
}

impl fmt::Display for FenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FenError::Board => "malformed board field",
            FenError::SideToMove => "side to move must be 'w' or 'b'",
            FenError::Castling => "castling field names no rook",
            FenError::EnPassant => "malformed en passant field",
            FenError::Kings => "each side must have exactly one king",
            FenError::OppositeCheck => "the side not to move is in check",
        })
    }
}

/// A chess position, plus the state chain reaching it.
#[derive(Clone, Debug)]
pub struct Position {
    board: [Piece; SQUARE_NB],
    /// Index 0 ([`PieceType::ALL_PIECES`]) is the total occupancy, not an empty set.
    by_type: [Bitboard; PIECE_TYPE_NB],
    by_color: [Bitboard; COLOR_NB],
    piece_count: [i32; PIECE_NB],
    side_to_move: Color,
    game_ply: i32,
    /// The rook origin per castling right, so Chess960 castling is a data lookup rather
    /// than a special case in the generator.
    castling_rook_square: [Square; 16],
    /// The squares the king and rook must find EMPTY for each right, minus the two movers
    /// themselves. Precomputed so the test is one AND against the occupancy.
    castling_path: [Bitboard; 16],
    /// For each square, the rights that moving from or to it destroys.
    castling_rights_mask: [CastlingRights; SQUARE_NB],
    chess960: bool,
    /// The state chain. NEVER empty: index 0 is the position as set up, and the last entry
    /// is the current state.
    states: Vec<StateInfo>,
}

impl Default for Position {
    fn default() -> Position {
        let mut pos = Position::blank();
        pos.set(START_FEN, false).expect("the start FEN is valid");
        pos
    }
}

impl Position {
    /// An empty board with a one-entry state chain. Not a legal position: call
    /// [`Position::set`] before using it.
    #[must_use]
    pub fn blank() -> Position {
        Position {
            board: [Piece::NONE; SQUARE_NB],
            by_type: [Bitboard::EMPTY; PIECE_TYPE_NB],
            by_color: [Bitboard::EMPTY; COLOR_NB],
            piece_count: [0; PIECE_NB],
            side_to_move: Color::White,
            game_ply: 0,
            castling_rook_square: [Square::NONE; 16],
            castling_path: [Bitboard::EMPTY; 16],
            castling_rights_mask: [CastlingRights::NONE; SQUARE_NB],
            chess960: false,
            states: vec![StateInfo::empty()],
        }
    }

    /// The position described by `fen`, in the standard or Chess960 castling dialect.
    pub fn from_fen(fen: &str, chess960: bool) -> Result<Position, FenError> {
        let mut pos = Position::blank();
        pos.set(fen, chess960)?;
        Ok(pos)
    }

    /// The standard starting position.
    #[must_use]
    pub fn startpos() -> Position {
        Position::default()
    }

    // -- accessors ---------------------------------------------------------

    /// The current state.
    #[inline(always)]
    #[must_use]
    pub fn st(&self) -> &StateInfo {
        self.states.last().expect("the state chain is never empty")
    }

    #[inline(always)]
    fn st_mut(&mut self) -> &mut StateInfo {
        self.states.last_mut().expect("the state chain is never empty")
    }

    /// The piece standing on `sq`, or [`Piece::NONE`].
    #[inline(always)]
    #[must_use]
    pub fn piece_on(&self, sq: Square) -> Piece {
        self.board[sq.index()]
    }

    /// True when `sq` holds no piece.
    #[inline(always)]
    #[must_use]
    pub fn is_empty_square(&self, sq: Square) -> bool {
        self.board[sq.index()].is_none()
    }

    /// Every occupied square.
    #[inline(always)]
    #[must_use]
    pub fn occupied(&self) -> Bitboard {
        self.by_type[0]
    }

    /// Every piece of type `pt`, both colours.
    #[inline(always)]
    #[must_use]
    pub fn pieces(&self, pt: PieceType) -> Bitboard {
        self.by_type[pt.index()]
    }

    /// Every piece of `c`.
    #[inline(always)]
    #[must_use]
    pub fn colored(&self, c: Color) -> Bitboard {
        self.by_color[c.index()]
    }

    /// Every piece of `c` and type `pt`.
    #[inline(always)]
    #[must_use]
    pub fn pieces_of(&self, c: Color, pt: PieceType) -> Bitboard {
        self.by_color[c.index()] & self.by_type[pt.index()]
    }

    /// Every piece of `c` and either of two types — upstream's two-type `pieces()`.
    #[inline(always)]
    #[must_use]
    pub fn pieces_of2(&self, c: Color, a: PieceType, b: PieceType) -> Bitboard {
        self.by_color[c.index()] & (self.by_type[a.index()] | self.by_type[b.index()])
    }

    /// How many pieces of `c` and type `pt` are on the board.
    #[inline(always)]
    #[must_use]
    pub fn count(&self, c: Color, pt: PieceType) -> i32 {
        self.piece_count[Piece::new(c, pt).index()]
    }

    /// How many pieces of type `pt`, both colours.
    #[inline(always)]
    #[must_use]
    pub fn count_both(&self, pt: PieceType) -> i32 {
        self.count(Color::White, pt) + self.count(Color::Black, pt)
    }

    /// The total number of pieces on the board.
    #[inline(always)]
    #[must_use]
    pub fn piece_total(&self) -> u32 {
        self.occupied().count()
    }

    /// `c`'s king square.
    ///
    /// # Panics
    /// Panics when `c` has no king. Every position that reaches the engine has been
    /// validated by [`Position::set`], which rejects that case.
    #[inline(always)]
    #[must_use]
    pub fn king_square(&self, c: Color) -> Square {
        self.pieces_of(c, PieceType::King).lsb()
    }

    /// The side to move.
    #[inline(always)]
    #[must_use]
    pub fn side_to_move(&self) -> Color {
        self.side_to_move
    }

    /// The position key.
    #[inline(always)]
    #[must_use]
    pub fn key(&self) -> Key {
        self.st().key
    }

    /// The pieces giving check to the side to move.
    #[inline(always)]
    #[must_use]
    pub fn checkers(&self) -> Bitboard {
        self.st().checkers
    }

    /// True when the side to move is in check.
    #[inline(always)]
    #[must_use]
    pub fn in_check(&self) -> bool {
        self.st().checkers.any()
    }

    /// The en-passant target square, or [`Square::NONE`].
    #[inline(always)]
    #[must_use]
    pub fn ep_square(&self) -> Square {
        self.st().ep_square
    }

    /// The halfmove clock.
    #[inline(always)]
    #[must_use]
    pub fn rule50_count(&self) -> i32 {
        self.st().rule50
    }

    /// The number of plies played from the position's own start.
    #[inline(always)]
    #[must_use]
    pub fn game_ply(&self) -> i32 {
        self.game_ply
    }

    /// True when castling is being played under Chess960 rules.
    #[inline(always)]
    #[must_use]
    pub fn is_chess960(&self) -> bool {
        self.chess960
    }

    /// `c`'s non-pawn, non-king material in centipawn-equivalent units.
    #[inline(always)]
    #[must_use]
    pub fn non_pawn_material(&self, c: Color) -> Value {
        self.st().non_pawn_material[c.index()]
    }

    /// Both sides' non-pawn material.
    #[inline(always)]
    #[must_use]
    pub fn non_pawn_material_total(&self) -> Value {
        self.st().non_pawn_material[0] + self.st().non_pawn_material[1]
    }

    /// True when `cr` is still available.
    #[inline(always)]
    #[must_use]
    pub fn can_castle(&self, cr: CastlingRights) -> bool {
        !self.st().castling_rights.intersect(cr).is_empty()
    }

    /// The rook that castling right `cr` moves.
    #[inline(always)]
    #[must_use]
    pub fn castling_rook_square(&self, cr: CastlingRights) -> Square {
        self.castling_rook_square[cr.index()]
    }

    /// True when a piece between the king and the rook blocks castling right `cr`.
    #[inline(always)]
    #[must_use]
    pub fn castling_impeded(&self, cr: CastlingRights) -> bool {
        (self.occupied() & self.castling_path[cr.index()]).any()
    }

    /// True when `m` captures a piece — including en passant, excluding castling, which
    /// encodes the rook's square as its destination.
    #[inline(always)]
    #[must_use]
    pub fn is_capture(&self, m: Move) -> bool {
        (!self.is_empty_square(m.to()) && m.move_type() != MoveType::Castling)
            || m.move_type() == MoveType::EnPassant
    }

    /// True when `m` is a capture or a promotion — the move classes qsearch generates.
    #[inline(always)]
    #[must_use]
    pub fn is_capture_stage(&self, m: Move) -> bool {
        self.is_capture(m) || m.move_type() == MoveType::Promotion
    }

    /// The piece the last move captured.
    #[inline(always)]
    #[must_use]
    pub fn captured_piece(&self) -> Piece {
        self.st().captured_piece
    }

    /// Hash the halfmove clock into `k` past move 14, so a position reached with a
    /// different rule50 count cannot reuse a transposition entry the rule invalidates.
    #[inline(always)]
    #[must_use]
    pub fn adjust_key50(k: Key, rule50: i32) -> Key {
        if rule50 < 14 {
            return k;
        }
        let seed = ((rule50 - 14) / 8) as Key;
        k ^ seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407)
    }

    // -- placement ---------------------------------------------------------

    fn put_piece(&mut self, pc: Piece, sq: Square) {
        self.board[sq.index()] = pc;
        self.by_type[0] |= sq;
        self.by_type[pc.piece_type().index()] |= sq;
        self.by_color[pc.color().index()] |= sq;
        self.piece_count[pc.index()] += 1;
    }

    fn remove_piece(&mut self, sq: Square) {
        let pc = self.board[sq.index()];
        self.by_type[0] ^= sq;
        self.by_type[pc.piece_type().index()] ^= sq;
        self.by_color[pc.color().index()] ^= sq;
        self.board[sq.index()] = Piece::NONE;
        self.piece_count[pc.index()] -= 1;
    }

    fn move_piece(&mut self, from: Square, to: Square) {
        let pc = self.board[from.index()];
        let delta = Bitboard::from_square(from) | Bitboard::from_square(to);
        self.by_type[0] ^= delta;
        self.by_type[pc.piece_type().index()] ^= delta;
        self.by_color[pc.color().index()] ^= delta;
        self.board[from.index()] = Piece::NONE;
        self.board[to.index()] = pc;
    }

    // -- setup -------------------------------------------------------------

    /// Replace the position with the one `fen` describes.
    ///
    /// The whole state chain is reset: after this call the position has no history, which
    /// is what a `position fen ...` command means.
    pub fn set(&mut self, fen: &str, chess960: bool) -> Result<(), FenError> {
        *self = Position::blank();
        self.chess960 = chess960;

        let mut fields = fen.split_ascii_whitespace();
        let board = fields.next().ok_or(FenError::Board)?;

        let mut file = 0usize;
        let mut rank = 7usize;
        for ch in board.bytes() {
            match ch {
                b'1'..=b'8' => file += (ch - b'0') as usize,
                b'/' => {
                    if file != 8 || rank == 0 {
                        return Err(FenError::Board);
                    }
                    file = 0;
                    rank -= 1;
                }
                _ => {
                    let pc = Piece::from_char(ch).ok_or(FenError::Board)?;
                    if file >= 8 {
                        return Err(FenError::Board);
                    }
                    self.put_piece(pc, Square::make(file, rank));
                    file += 1;
                }
            }
        }
        if file != 8 || rank != 0 {
            return Err(FenError::Board);
        }

        self.side_to_move = match fields.next() {
            Some("w") => Color::White,
            Some("b") => Color::Black,
            _ => return Err(FenError::SideToMove),
        };

        // Both kings must exist before castling rights can name a rook relative to one.
        for c in Color::ALL {
            if self.count(c, PieceType::King) != 1 {
                return Err(FenError::Kings);
            }
        }

        let castling = fields.next().unwrap_or("-");
        if castling != "-" {
            for ch in castling.bytes() {
                let color = if ch.is_ascii_uppercase() { Color::White } else { Color::Black };
                let back_rank = relative_rank_bb(color, 0);
                let rooks = self.pieces_of(color, PieceType::Rook) & back_rank;
                let ksq = self.king_square(color);

                // Standard notation names the side; Shredder-FEN names the rook's file
                // directly. Resolve both to a rook square before setting the right.
                let rook_sq = match ch.to_ascii_uppercase() {
                    b'K' => (rooks.bits() != 0)
                        .then(|| rooks.msb())
                        .filter(|&r| r > ksq)
                        .ok_or(FenError::Castling)?,
                    b'Q' => (rooks.bits() != 0)
                        .then(|| rooks.lsb())
                        .filter(|&r| r < ksq)
                        .ok_or(FenError::Castling)?,
                    f @ b'A'..=b'H' => {
                        let sq = Square::make((f - b'A') as usize, ksq.rank());
                        if self.piece_on(sq) != Piece::new(color, PieceType::Rook) {
                            return Err(FenError::Castling);
                        }
                        sq
                    }
                    _ => return Err(FenError::Castling),
                };
                self.set_castling_right(color, rook_sq);
            }
        }

        self.set_ep_square(fields.next().unwrap_or("-"))?;

        let rule50: i32 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let fullmove: i32 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        // Upstream converts to a ply count from move 1, then subtracts one for Black.
        self.game_ply = (fullmove.max(1) - 1) * 2 + i32::from(self.side_to_move == Color::Black);

        let st = self.st_mut();
        st.rule50 = rule50;
        st.plies_from_null = 0;

        // The side not to move must not be in check: that position could never arise.
        let them = !self.side_to_move;
        if (self.attackers_to(self.king_square(them)) & self.colored(self.side_to_move)).any() {
            return Err(FenError::OppositeCheck);
        }

        self.set_state();
        Ok(())
    }

    /// Parse the FEN en-passant field, keeping the square only when the capture is
    /// actually available — upstream's rule, and one the key depends on.
    fn set_ep_square(&mut self, field: &str) -> Result<(), FenError> {
        let bytes = field.as_bytes();
        let sq = match bytes {
            b"-" => Square::NONE,
            [f, r] => Square::from_coords(*f, *r).ok_or(FenError::EnPassant)?,
            _ => return Err(FenError::EnPassant),
        };
        if sq.is_none() {
            self.st_mut().ep_square = Square::NONE;
            return Ok(());
        }

        // The square must be on the right rank for the side to move, must be empty, must
        // have the captured pawn behind it, and some pawn must actually be able to take.
        let us = self.side_to_move;
        let them = !us;
        let captured = sq.shift(Direction::pawn_push(them));
        let available = sq.relative_rank(us) == 5
            && self.is_empty_square(sq)
            && self.piece_on(captured) == Piece::new(them, PieceType::Pawn)
            && (pawn_attacks_from(them, sq) & self.pieces_of(us, PieceType::Pawn)).any();

        self.st_mut().ep_square = if available { sq } else { Square::NONE };
        Ok(())
    }

    /// Record one castling right and the path it needs clear.
    fn set_castling_right(&mut self, c: Color, rook_from: Square) {
        let ksq = self.king_square(c);
        let king_side = rook_from > ksq;
        let cr =
            if king_side { CastlingRights::king_side(c) } else { CastlingRights::queen_side(c) };

        self.states[0].castling_rights = self.states[0].castling_rights.union(cr);
        self.castling_rights_mask[ksq.index()] = self.castling_rights_mask[ksq.index()].union(cr);
        self.castling_rights_mask[rook_from.index()] =
            self.castling_rights_mask[rook_from.index()].union(cr);
        self.castling_rook_square[cr.index()] = rook_from;

        // The destinations are fixed by the rules, in both dialects: g1/c1 for the king,
        // f1/d1 for the rook, mirrored for Black.
        let king_to = Square::make(if king_side { 6 } else { 2 }, ksq.rank());
        let rook_to = Square::make(if king_side { 5 } else { 3 }, ksq.rank());

        // The path is everything the king and rook travel through, minus the two movers.
        // In Chess960 either can already stand on a square the other needs.
        self.castling_path[cr.index()] = (between_bb(rook_from, rook_to)
            | between_bb(ksq, king_to)
            | Bitboard::from_square(king_to)
            | Bitboard::from_square(rook_to))
            & !(Bitboard::from_square(ksq) | Bitboard::from_square(rook_from));
    }

    /// Recompute every derived field of the current state from the board alone.
    ///
    /// Called once after a FEN is parsed. `do_move` maintains the same fields
    /// incrementally, and [`Position::assert_state_consistent`] checks the two agree.
    fn set_state(&mut self) {
        let mut key: Key = 0;
        let mut pawn_key: Key = zobrist::no_pawns();
        let mut minor_key: Key = 0;
        let mut non_pawn_key = [0 as Key; COLOR_NB];
        let mut material_key: Key = 0;
        let mut non_pawn_material = [0 as Value; COLOR_NB];

        for sq in self.occupied() {
            let pc = self.piece_on(sq);
            let pt = pc.piece_type();
            let c = pc.color();
            key ^= zobrist::psq(pc, sq);
            if pt == PieceType::Pawn {
                pawn_key ^= zobrist::psq(pc, sq);
            } else {
                non_pawn_key[c.index()] ^= zobrist::psq(pc, sq);
                if pt != PieceType::King {
                    non_pawn_material[c.index()] += piece_value(pc);
                }
                if matches!(pt, PieceType::Knight | PieceType::Bishop) {
                    minor_key ^= zobrist::psq(pc, sq);
                }
            }
        }

        // The material key hashes counts, not squares: `psq[pc][8 + n]` for the n-th piece
        // of that kind. Syzygy indexes its tables by this.
        for c in Color::ALL {
            for pt in PieceType::REAL {
                let pc = Piece::new(c, pt);
                for n in 0..self.count(c, pt) {
                    material_key ^= zobrist::psq(pc, Square::new(8 + n as usize));
                }
            }
        }

        let ep = self.st().ep_square;
        if ep.is_ok() {
            key ^= zobrist::en_passant(ep.file());
        }
        if self.side_to_move == Color::Black {
            key ^= zobrist::side();
        }
        key ^= zobrist::castling(self.st().castling_rights);

        let checkers = self.attackers_to(self.king_square(self.side_to_move))
            & self.colored(!self.side_to_move);

        let st = self.st_mut();
        st.key = key;
        st.pawn_key = pawn_key;
        st.minor_piece_key = minor_key;
        st.non_pawn_key = non_pawn_key;
        st.material_key = material_key;
        st.non_pawn_material = non_pawn_material;
        st.checkers = checkers;
        self.set_check_info();
    }

    /// Recompute the pin, blocker and check-square caches for the current state.
    fn set_check_info(&mut self) {
        for c in Color::ALL {
            let (blockers, pinners) = self.slider_blockers(!c, self.king_square(c));
            let st = self.st_mut();
            st.blockers[c.index()] = blockers;
            st.pinners[(!c).index()] = pinners;
        }

        let ksq = self.king_square(!self.side_to_move);
        let occ = self.occupied();
        let mut cs = [Bitboard::EMPTY; PIECE_TYPE_NB];
        cs[PieceType::Pawn.index()] = pawn_attacks_from(!self.side_to_move, ksq);
        cs[PieceType::Knight.index()] = KNIGHT_ATTACKS[ksq.index()];
        cs[PieceType::Bishop.index()] = bishop_attacks(ksq, occ);
        cs[PieceType::Rook.index()] = rook_attacks(ksq, occ);
        cs[PieceType::Queen.index()] = cs[PieceType::Bishop.index()] | cs[PieceType::Rook.index()];
        // A king cannot give check, so its entry stays empty; the generator relies on it.
        self.st_mut().check_squares = cs;
    }

    /// Pieces of either colour that attack `sq`, given the current occupancy.
    #[must_use]
    pub fn attackers_to(&self, sq: Square) -> Bitboard {
        self.attackers_to_occ(sq, self.occupied())
    }

    /// Pieces of either colour that attack `sq`, given `occ`.
    ///
    /// The `occ` argument is what lets the static exchange evaluation replay a capture
    /// sequence by removing pieces from a local occupancy rather than from the board.
    #[must_use]
    pub fn attackers_to_occ(&self, sq: Square, occ: Bitboard) -> Bitboard {
        (pawn_attacks_from(Color::Black, sq) & self.pieces_of(Color::White, PieceType::Pawn))
            | (pawn_attacks_from(Color::White, sq) & self.pieces_of(Color::Black, PieceType::Pawn))
            | (KNIGHT_ATTACKS[sq.index()] & self.pieces(PieceType::Knight))
            | (rook_attacks(sq, occ)
                & (self.pieces(PieceType::Rook) | self.pieces(PieceType::Queen)))
            | (bishop_attacks(sq, occ)
                & (self.pieces(PieceType::Bishop) | self.pieces(PieceType::Queen)))
            | (KING_ATTACKS[sq.index()] & self.pieces(PieceType::King))
    }

    /// Pieces that stand between `sq` and a slider of colour `c`, plus those sliders.
    ///
    /// Returns `(blockers, pinners)`. A blocker of either colour is returned: an own
    /// blocker is pinned, an enemy blocker could deliver a discovered check by moving.
    #[must_use]
    pub fn slider_blockers(&self, c: Color, sq: Square) -> (Bitboard, Bitboard) {
        let mut blockers = Bitboard::EMPTY;
        let mut pinners = Bitboard::EMPTY;

        // Sliders that would attack `sq` if every piece between were removed.
        let snipers = ((rook_attacks(sq, Bitboard::EMPTY)
            & (self.pieces(PieceType::Queen) | self.pieces(PieceType::Rook)))
            | (bishop_attacks(sq, Bitboard::EMPTY)
                & (self.pieces(PieceType::Queen) | self.pieces(PieceType::Bishop))))
            & self.colored(c);
        let occupancy = self.occupied() ^ snipers;

        for sniper in snipers {
            let between = between_bb(sq, sniper) & occupancy;
            // Exactly one piece in the way makes it a blocker; two or more block nothing.
            if between.any() && !between.more_than_one() {
                blockers |= between;
                if (between & self.colored(self.piece_on(sq).color())).any() {
                    pinners |= sniper;
                }
            }
        }
        (blockers, pinners)
    }

    /// Own pieces of `c` whose move could expose their own king.
    #[inline(always)]
    #[must_use]
    pub fn blockers_for_king(&self, c: Color) -> Bitboard {
        self.st().blockers[c.index()]
    }

    /// Squares from which a piece of type `pt` would check the enemy king.
    #[inline(always)]
    #[must_use]
    pub fn check_squares(&self, pt: PieceType) -> Bitboard {
        self.st().check_squares[pt.index()]
    }

    // -- legality ----------------------------------------------------------

    /// True when the pseudo-legal move `m` leaves its own king safe.
    ///
    /// Only the three cases that can go wrong are checked, exactly as upstream does: an
    /// en-passant capture that unmasks a rank attack, a king move onto an attacked square,
    /// and a pinned piece leaving the pin line. Everything else was already excluded by
    /// generation.
    #[must_use]
    pub fn legal(&self, m: Move) -> bool {
        debug_assert!(m.is_ok());
        let us = self.side_to_move;
        let from = m.from();
        let to = m.to();
        debug_assert_eq!(self.piece_on(from).color(), us);
        debug_assert_eq!(self.piece_on(self.king_square(us)), Piece::new(us, PieceType::King));

        if m.move_type() == MoveType::EnPassant {
            // Removing BOTH pawns at once can open a rank onto the king, which no pin
            // test sees because neither pawn is a blocker on its own.
            let ksq = self.king_square(us);
            let capsq = to.shift(Direction::pawn_push(!us));
            let occ =
                (self.occupied() ^ Bitboard::from_square(from) ^ Bitboard::from_square(capsq))
                    | Bitboard::from_square(to);
            return (rook_attacks(ksq, occ)
                & self.pieces_of2(!us, PieceType::Rook, PieceType::Queen))
            .is_empty()
                && (bishop_attacks(ksq, occ)
                    & self.pieces_of2(!us, PieceType::Bishop, PieceType::Queen))
                .is_empty();
        }

        if m.move_type() == MoveType::Castling {
            // `to` is the ROOK's square. Check every square the king crosses, including
            // its destination, and -- for Chess960 -- that the rook is not itself pinned
            // along the rank.
            let king_side = to > from;
            let rook_from = to;
            let king_to = Square::make(if king_side { 6 } else { 2 }, from.rank());
            let step = if king_side { Direction::East } else { Direction::West };

            let mut s = from;
            loop {
                if (self.attackers_to(s) & self.colored(!us)).any() {
                    return false;
                }
                if s == king_to {
                    break;
                }
                s = s.shift(step);
            }

            if self.chess960 {
                let occ = (self.occupied() ^ Bitboard::from_square(rook_from))
                    | Bitboard::from_square(king_to);
                return (rook_attacks(king_to, occ)
                    & self.pieces_of2(!us, PieceType::Rook, PieceType::Queen))
                .is_empty();
            }
            return true;
        }

        if self.piece_on(from).piece_type() == PieceType::King {
            // The king may not step onto a square the enemy attacks with the king itself
            // removed from the occupancy -- otherwise it "blocks" the ray it is fleeing.
            let occ = self.occupied() ^ Bitboard::from_square(from);
            return (self.attackers_to_occ(to, occ) & self.colored(!us)).is_empty();
        }

        // A non-blocker cannot expose the king; a blocker may only move along the line
        // through the king.
        !self.blockers_for_king(us).contains(from) || aligned(from, to, self.king_square(us))
    }

    /// True when `m` could have been produced by the generator for this position.
    ///
    /// The transposition table and the killer slots hand back moves from other positions,
    /// so every such move is filtered through this before it is played.
    #[must_use]
    pub fn pseudo_legal(&self, m: Move) -> bool {
        if !m.is_ok() {
            return false;
        }
        let us = self.side_to_move;
        let from = m.from();
        let to = m.to();
        let pc = self.piece_on(from);

        // The special encodings carry information the cheap tests below cannot check, so
        // fall back to full generation for them. They are rare enough for that to be free.
        if m.move_type() != MoveType::Normal {
            return super::movegen::generate_legal(self).contains(&m);
        }
        // A Normal move never carries a promotion piece, and its mover must be ours.
        if pc.is_none() || pc.color() != us {
            return false;
        }
        if self.colored(us).contains(to) {
            return false;
        }

        if pc.piece_type() == PieceType::Pawn {
            // A Normal pawn move can never reach the last rank -- that would have to be a
            // Promotion.
            if (relative_rank_bb(us, 7) & Bitboard::from_square(to)).any() {
                return false;
            }
            let push = Direction::pawn_push(us);
            let single = from.shift(push);
            let capture = (pawn_attacks_from(us, from) & self.colored(!us)).contains(to);
            let single_push = to == single && self.is_empty_square(to);
            let double_push = to == single.shift(push)
                && from.relative_rank(us) == 1
                && self.is_empty_square(to)
                && self.is_empty_square(single);
            if !(capture || single_push || double_push) {
                return false;
            }
        } else if !piece_attacks(pc.piece_type(), from, self.occupied()).contains(to) {
            return false;
        }

        // In check, only a move that resolves the check survives; the generator would have
        // produced evasions only.
        if self.checkers().any() {
            if pc.piece_type() != PieceType::King {
                if self.checkers().more_than_one() {
                    return false;
                }
                if !(between_bb(self.king_square(us), self.checkers().lsb())).contains(to) {
                    return false;
                }
            } else if (self.attackers_to_occ(to, self.occupied() ^ Bitboard::from_square(from))
                & self.colored(!us))
            .any()
            {
                return false;
            }
        }
        true
    }

    /// True when `m` gives check, decided before the move is made.
    ///
    /// The search needs this to select the child's checkers set without a second attacker
    /// scan, so it is derived from the cached check squares rather than by making the move.
    #[must_use]
    pub fn gives_check(&self, m: Move) -> bool {
        debug_assert!(m.is_ok());
        let from = m.from();
        let to = m.to();
        let us = self.side_to_move;
        let ksq = self.king_square(!us);

        // Direct check: the mover lands on a square that attacks the enemy king.
        if self.check_squares(self.piece_on(from).piece_type()).contains(to) {
            return true;
        }

        // Discovered check: the mover was a blocker and leaves the line.
        if self.blockers_for_king(!us).contains(from) && !aligned(from, to, ksq) {
            return true;
        }

        match m.move_type() {
            MoveType::Normal => false,
            MoveType::Promotion => {
                piece_attacks(m.promotion_type(), to, self.occupied() ^ Bitboard::from_square(from))
                    .contains(ksq)
            }
            MoveType::EnPassant => {
                // Two pawns leave the board and one appears; only a full recheck of the
                // sliders answers it.
                let capsq = Square::make(to.file(), from.rank());
                let occ =
                    (self.occupied() ^ Bitboard::from_square(from) ^ Bitboard::from_square(capsq))
                        | Bitboard::from_square(to);
                (rook_attacks(ksq, occ) & self.pieces_of2(us, PieceType::Rook, PieceType::Queen))
                    .any()
                    || (bishop_attacks(ksq, occ)
                        & self.pieces_of2(us, PieceType::Bishop, PieceType::Queen))
                    .any()
            }
            MoveType::Castling => {
                // Only the rook can give check after castling; the king cannot.
                let king_side = to > from;
                let rook_to = Square::make(if king_side { 5 } else { 3 }, from.rank());
                let king_to = Square::make(if king_side { 6 } else { 2 }, from.rank());
                let occ =
                    (self.occupied() ^ Bitboard::from_square(from) ^ Bitboard::from_square(to))
                        | Bitboard::from_square(rook_to)
                        | Bitboard::from_square(king_to);
                rook_attacks(rook_to, occ).contains(ksq)
            }
        }
    }

    // -- transitions -------------------------------------------------------

    /// Make `m`, pushing a new state.
    ///
    /// `gives_check` is TRUSTED, never re-derived: it must equal
    /// [`Position::gives_check`] on the pre-move position, and a wrong value corrupts the
    /// child's checkers and every generation decision below it. Callers that do not
    /// already know it should use [`Position::do_move`].
    pub fn do_move_checked(&mut self, m: Move, gives_check: bool) {
        debug_assert!(m.is_ok());
        let us = self.side_to_move;
        let them = !us;
        let from = m.from();
        let to = m.to();
        let mt = m.move_type();
        let pc = self.piece_on(from);
        debug_assert_eq!(pc.color(), us);

        let mut st = self.st().carried_forward();
        let mut key = self.st().key ^ zobrist::side();
        st.rule50 += 1;
        st.plies_from_null += 1;
        self.game_ply += 1;

        // Clear the parent's en-passant key before anything else can set a new one.
        if st.ep_square.is_ok() {
            key ^= zobrist::en_passant(st.ep_square.file());
            st.ep_square = Square::NONE;
        }

        // Castling rights die when the king or a rook leaves, or when a rook is captured.
        let lost = self.castling_rights_mask[from.index()]
            .union(self.castling_rights_mask[to.index()])
            .intersect(st.castling_rights);
        if !lost.is_empty() {
            key ^= zobrist::castling(st.castling_rights);
            st.castling_rights = st.castling_rights.without(lost);
            key ^= zobrist::castling(st.castling_rights);
        }

        if mt == MoveType::Castling {
            // King takes rook: both movers leave, both destinations are fixed.
            let king_side = to > from;
            let king_to = Square::make(if king_side { 6 } else { 2 }, from.rank());
            let rook_to = Square::make(if king_side { 5 } else { 3 }, from.rank());
            let rook = self.piece_on(to);

            self.remove_piece(from);
            self.remove_piece(to);
            self.put_piece(pc, king_to);
            self.put_piece(rook, rook_to);

            key ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, king_to);
            key ^= zobrist::psq(rook, to) ^ zobrist::psq(rook, rook_to);
            st.non_pawn_key[us.index()] ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, king_to);
            st.non_pawn_key[us.index()] ^= zobrist::psq(rook, to) ^ zobrist::psq(rook, rook_to);
            st.captured_piece = Piece::NONE;
        } else {
            // A capture removes the victim first, so the mover lands on an empty square.
            let capsq =
                if mt == MoveType::EnPassant { to.shift(Direction::pawn_push(them)) } else { to };
            let captured =
                if mt == MoveType::EnPassant { self.piece_on(capsq) } else { self.piece_on(to) };

            if !captured.is_none() {
                self.remove_piece(capsq);
                key ^= zobrist::psq(captured, capsq);
                if captured.piece_type() == PieceType::Pawn {
                    st.pawn_key ^= zobrist::psq(captured, capsq);
                } else {
                    st.non_pawn_key[them.index()] ^= zobrist::psq(captured, capsq);
                    st.non_pawn_material[them.index()] -= piece_value(captured);
                    if matches!(captured.piece_type(), PieceType::Knight | PieceType::Bishop) {
                        st.minor_piece_key ^= zobrist::psq(captured, capsq);
                    }
                }
                st.material_key ^= zobrist::psq(
                    captured,
                    Square::new(8 + self.piece_count[captured.index()] as usize),
                );
                st.rule50 = 0;
            }
            st.captured_piece = captured;

            self.move_piece(from, to);
            key ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, to);

            if pc.piece_type() == PieceType::Pawn {
                st.pawn_key ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, to);
                st.rule50 = 0;

                // A double push only sets an en-passant square when a capture is actually
                // available -- upstream's rule, and the key depends on it.
                if to.index().abs_diff(from.index()) == 16
                    && (pawn_attacks_from(us, to.shift(Direction::pawn_push(them)))
                        & self.pieces_of(them, PieceType::Pawn))
                    .any()
                {
                    st.ep_square = to.shift(Direction::pawn_push(them));
                    key ^= zobrist::en_passant(st.ep_square.file());
                } else if mt == MoveType::Promotion {
                    let promo = Piece::new(us, m.promotion_type());
                    self.remove_piece(to);
                    self.put_piece(promo, to);
                    key ^= zobrist::psq(pc, to) ^ zobrist::psq(promo, to);
                    st.pawn_key ^= zobrist::psq(pc, to);
                    st.non_pawn_key[us.index()] ^= zobrist::psq(promo, to);
                    st.non_pawn_material[us.index()] += piece_value(promo);
                    if matches!(m.promotion_type(), PieceType::Knight | PieceType::Bishop) {
                        st.minor_piece_key ^= zobrist::psq(promo, to);
                    }
                    st.material_key ^= zobrist::psq(
                        promo,
                        Square::new(8 + self.piece_count[promo.index()] as usize - 1),
                    );
                    st.material_key ^=
                        zobrist::psq(pc, Square::new(8 + self.piece_count[pc.index()] as usize));
                }
            } else {
                st.non_pawn_key[us.index()] ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, to);
                if matches!(pc.piece_type(), PieceType::Knight | PieceType::Bishop) {
                    st.minor_piece_key ^= zobrist::psq(pc, from) ^ zobrist::psq(pc, to);
                }
            }
        }

        st.key = key;
        self.side_to_move = them;
        st.checkers = if gives_check {
            self.attackers_to(self.king_square(them)) & self.colored(us)
        } else {
            Bitboard::EMPTY
        };

        self.states.push(st);
        self.set_check_info();
        self.set_repetition();
    }

    /// Make `m`, deriving the check flag from the position.
    pub fn do_move(&mut self, m: Move) {
        let gives_check = self.gives_check(m);
        self.do_move_checked(m, gives_check);
    }

    /// Fill in the current state's repetition distance.
    ///
    /// Positive for a first repetition, NEGATIVE when the earlier occurrence was itself a
    /// repetition (so this is the threefold), 0 when never repeated. The sign is the whole
    /// encoding.
    fn set_repetition(&mut self) {
        let st_index = self.states.len() - 1;
        let key = self.states[st_index].key;
        // Only positions with the same side to move can repeat, so step back two at a
        // time; nothing before the last irreversible move can match.
        let end = self.states[st_index].rule50.min(self.states[st_index].plies_from_null);
        let mut repetition = 0;
        let mut i = 4;
        while i <= end {
            let idx = st_index.checked_sub(i as usize);
            let Some(idx) = idx else { break };
            if self.states[idx].key == key {
                repetition = if self.states[idx].repetition != 0 { -i } else { i };
                break;
            }
            i += 2;
        }
        self.states[st_index].repetition = repetition;
    }

    /// Undo the last move.
    ///
    /// # Panics
    /// Panics when the chain holds only the position as set up — undoing past the root is
    /// always a bug in the caller.
    pub fn undo_move(&mut self, m: Move) {
        debug_assert!(m.is_ok());
        assert!(self.states.len() > 1, "undo past the root of the state chain");

        self.side_to_move = !self.side_to_move;
        let us = self.side_to_move;
        let from = m.from();
        let to = m.to();
        let mt = m.move_type();

        if mt == MoveType::Castling {
            let king_side = to > from;
            let king_to = Square::make(if king_side { 6 } else { 2 }, from.rank());
            let rook_to = Square::make(if king_side { 5 } else { 3 }, from.rank());
            let king = self.piece_on(king_to);
            let rook = self.piece_on(rook_to);
            self.remove_piece(king_to);
            self.remove_piece(rook_to);
            self.put_piece(king, from);
            self.put_piece(rook, to);
        } else {
            if mt == MoveType::Promotion {
                self.remove_piece(to);
                self.put_piece(Piece::new(us, PieceType::Pawn), to);
            }
            self.move_piece(to, from);

            let captured = self.st().captured_piece;
            if !captured.is_none() {
                let capsq = if mt == MoveType::EnPassant {
                    to.shift(Direction::pawn_push(!us))
                } else {
                    to
                };
                self.put_piece(captured, capsq);
            }
        }

        self.states.pop();
        self.game_ply -= 1;
    }

    /// Flip the side to move without touching a piece.
    pub fn do_null_move(&mut self) {
        debug_assert!(!self.in_check());
        let mut st = self.st().carried_forward();
        let mut key = self.st().key ^ zobrist::side();
        if st.ep_square.is_ok() {
            key ^= zobrist::en_passant(st.ep_square.file());
            st.ep_square = Square::NONE;
        }
        st.key = key;
        st.rule50 += 1;
        // A null move is irreversible for repetition purposes: nothing before it can
        // repeat with the same side to move.
        st.plies_from_null = 0;
        st.captured_piece = Piece::NONE;
        st.checkers = Bitboard::EMPTY;

        self.side_to_move = !self.side_to_move;
        self.states.push(st);
        self.set_check_info();
        self.st_mut().repetition = 0;
    }

    /// Undo the last null move.
    ///
    /// # Panics
    /// Panics when the chain holds only the root state.
    pub fn undo_null_move(&mut self) {
        assert!(self.states.len() > 1, "undo past the root of the state chain");
        self.states.pop();
        self.side_to_move = !self.side_to_move;
    }

    // -- draws -------------------------------------------------------------

    /// True when the position is drawn by the fifty-move rule or by repetition within the
    /// current search.
    ///
    /// `ply` is the distance from the search root: a repetition that happened at or after
    /// the root counts once, while one entirely in the game history needs the full
    /// threefold. That asymmetry is upstream's, and it is why the search can claim a draw
    /// on the second occurrence.
    #[must_use]
    pub fn is_draw(&self, ply: i32) -> bool {
        let st = self.st();
        if st.rule50 > 99 && (!self.in_check() || super::movegen::has_legal_move(self)) {
            return true;
        }
        self.is_repetition(ply)
    }

    /// Mirror the position vertically and swap the colours.
    ///
    /// A debugging command: the evaluation of a position and of its mirror should agree up
    /// to sign, so any asymmetry in the evaluation shows up immediately. Upstream does it
    /// by rewriting the FEN and re-parsing rather than by permuting the bitboards, and so
    /// does this — the FEN writer and the FEN parser are already tested against each other,
    /// which a hand-written permutation would not be.
    ///
    /// The state chain is REPLACED, not extended. The mirrored position never occurred in
    /// this game, so carrying the repetition history across would let it claim a draw
    /// against positions it has never stood in.
    ///
    /// # Errors
    ///
    /// Returns the parse error if the mirrored record is not a legal position, which for a
    /// legal input it cannot be.
    pub fn flip(&mut self) -> Result<(), FenError> {
        let fen = self.fen();
        let mut parts = fen.split(' ');
        let (board, stm, castling, ep) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or("w"),
            parts.next().unwrap_or("-"),
            parts.next().unwrap_or("-"),
        );
        let rest: Vec<&str> = parts.collect();

        // Reversing the ranks mirrors the board; swapping every letter's case swaps the
        // piece colours, and does the same for the castling letters in one pass.
        let flipped_board: Vec<&str> = board.split('/').rev().collect();
        let swap_case = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect()
        };

        // The en-passant square mirrors with the board: only ranks 3 and 6 can hold one.
        let flipped_ep = if ep == "-" {
            ep.to_string()
        } else {
            let mut c = ep.chars();
            let file = c.next().unwrap_or('a');
            let rank = if c.next() == Some('3') { '6' } else { '3' };
            format!("{file}{rank}")
        };

        let mut out = format!(
            "{} {} {} {}",
            swap_case(&flipped_board.join("/")),
            if stm == "w" { "b" } else { "w" },
            swap_case(castling),
            flipped_ep
        );
        for part in rest {
            out.push(' ');
            out.push_str(part);
        }

        *self = Position::from_fen(&out, self.chess960)?;
        Ok(())
    }

    /// True when a Syzygy DTZ table's distance is also the distance to mate.
    ///
    /// DTZ counts plies to the next clock-zeroing move, which is generally not mate. With
    /// no pawns and at most a bare piece on each side, the only zeroing move available IS
    /// the mate, so the two distances coincide and the root ranking can order wins by
    /// length instead of treating them all as equal.
    #[must_use]
    pub fn dtz_is_dtm(&self) -> bool {
        self.count_both(PieceType::Pawn) == 0
            && (self.piece_total() == 3
                || (self.piece_total() == 4
                    && (self.pieces(PieceType::Queen) | self.pieces(PieceType::Rook)).is_empty()))
    }

    /// True when this position has already occurred within `ply` of the search root.
    ///
    /// Split out from [`Position::is_draw`] because the root tablebase ranking needs the
    /// repetition alone: with `Syzygy50MoveRule` off, the fifty-move half of a draw does
    /// not apply but a repetition still does.
    #[must_use]
    pub fn is_repetition(&self, ply: i32) -> bool {
        let st = self.st();
        st.repetition != 0 && st.repetition < ply
    }

    /// True when the side to move can reach a repetition with one move — upstream's
    /// `has_game_cycle`, used to prune positions that are already drawable.
    #[must_use]
    pub fn has_repeated(&self) -> bool {
        let mut i = self.states.len() - 1;
        loop {
            let end = self.states[i].rule50.min(self.states[i].plies_from_null);
            if end < 4 {
                return false;
            }
            if self.states[i].repetition != 0 {
                return true;
            }
            if i == 0 {
                return false;
            }
            i -= 1;
        }
    }

    // -- static exchange evaluation ----------------------------------------

    /// True when the exchange sequence starting with `m` wins at least `threshold`.
    ///
    /// The swap-off algorithm: play the cheapest attacker each time, and stop as soon as
    /// the side to move can no longer beat the threshold. It answers a question about
    /// material only — no king safety, no tactics beyond the one square.
    #[must_use]
    pub fn see_ge(&self, m: Move, threshold: Value) -> bool {
        debug_assert!(m.is_ok());
        // The special move types are given upstream's fixed answers rather than being
        // replayed: their material effect is not a simple exchange on one square.
        if m.move_type() != MoveType::Normal {
            return threshold <= 0;
        }

        let from = m.from();
        let to = m.to();
        let mut swap = piece_value(self.piece_on(to)) - threshold;
        if swap < 0 {
            return false;
        }
        swap = piece_value(self.piece_on(from)) - swap;
        if swap <= 0 {
            return true;
        }

        let mut occupied =
            self.occupied() ^ Bitboard::from_square(from) ^ Bitboard::from_square(to);
        let mut stm = self.side_to_move;
        let mut attackers = self.attackers_to_occ(to, occupied);
        let mut result = true;

        loop {
            stm = !stm;
            attackers &= occupied;
            let mut stm_attackers = attackers & self.colored(stm);
            if stm_attackers.is_empty() {
                break;
            }

            // A pinned piece cannot join the exchange unless its pinner is already gone.
            if (self.st().pinners[(!stm).index()] & occupied).any() {
                stm_attackers &= !self.blockers_for_king(stm);
                if stm_attackers.is_empty() {
                    break;
                }
            }

            result = !result;

            // Take with the cheapest attacker, then let the x-ray behind it join in.
            let mut taken = false;
            for pt in PieceType::REAL {
                let candidates = stm_attackers & self.pieces(pt);
                if candidates.is_empty() {
                    continue;
                }
                swap = piece_value(Piece::new(Color::White, pt)) - swap;
                if swap < i32::from(result) {
                    // Taking loses the exchange, so this side stops here.
                    return result;
                }
                occupied ^= Bitboard::from_square(candidates.lsb());
                match pt {
                    PieceType::Pawn | PieceType::Bishop => {
                        attackers |= bishop_attacks(to, occupied)
                            & (self.pieces(PieceType::Bishop) | self.pieces(PieceType::Queen));
                    }
                    PieceType::Rook => {
                        attackers |= rook_attacks(to, occupied)
                            & (self.pieces(PieceType::Rook) | self.pieces(PieceType::Queen));
                    }
                    PieceType::Queen => {
                        attackers |= queen_attacks(to, occupied)
                            & (self.pieces(PieceType::Bishop)
                                | self.pieces(PieceType::Rook)
                                | self.pieces(PieceType::Queen));
                    }
                    PieceType::King => {
                        // The king may only take when the other side has run out: if it
                        // has an attacker left, taking would be illegal.
                        if (attackers & self.colored(!stm)).any() {
                            result = !result;
                        }
                        return result;
                    }
                    _ => {}
                }
                taken = true;
                break;
            }
            if !taken {
                break;
            }
        }
        result
    }

    // -- rendering ---------------------------------------------------------

    /// The position's FEN record.
    #[must_use]
    pub fn fen(&self) -> String {
        let mut s = String::with_capacity(96);
        for rank in (0..8).rev() {
            let mut empty = 0;
            for file in 0..8 {
                let pc = self.piece_on(Square::make(file, rank));
                if pc.is_none() {
                    empty += 1;
                } else {
                    if empty > 0 {
                        s.push(char::from_digit(empty, 10).expect("0..8 is a digit"));
                        empty = 0;
                    }
                    s.push(pc.to_char() as char);
                }
            }
            if empty > 0 {
                s.push(char::from_digit(empty, 10).expect("0..8 is a digit"));
            }
            if rank > 0 {
                s.push('/');
            }
        }

        s.push(' ');
        s.push(if self.side_to_move == Color::White { 'w' } else { 'b' });
        s.push(' ');

        let before = s.len();
        for (c, right, letter) in [
            (Color::White, CastlingRights::WHITE_OO, 'K'),
            (Color::White, CastlingRights::WHITE_OOO, 'Q'),
            (Color::Black, CastlingRights::BLACK_OO, 'k'),
            (Color::Black, CastlingRights::BLACK_OOO, 'q'),
        ] {
            if !self.can_castle(right) {
                continue;
            }
            if self.chess960 {
                // Shredder-FEN: name the rook's file, upper case for White.
                let file = (b'A' + self.castling_rook_square(right).file() as u8) as char;
                s.push(if c == Color::White { file } else { file.to_ascii_lowercase() });
            } else {
                s.push(letter);
            }
        }
        if s.len() == before {
            s.push('-');
        }

        s.push(' ');
        if self.st().ep_square.is_ok() {
            s.push_str(&self.st().ep_square.to_string());
        } else {
            s.push('-');
        }

        let fullmove = 1 + (self.game_ply - i32::from(self.side_to_move == Color::Black)) / 2;
        write!(s, " {} {fullmove}", self.st().rule50).expect("writing to a String cannot fail");
        s
    }

    /// Check that every incrementally maintained field agrees with a from-scratch
    /// recomputation.
    ///
    /// O(pieces) per call: a diagnostic for gates and tests, never something the search
    /// calls. It is not behind a `cfg` so a gate build can use it without a second profile.
    #[must_use]
    pub fn state_is_consistent(&self) -> bool {
        let mut fresh = self.clone();
        // Recompute from the board alone and compare the derived block.
        let saved = fresh.st().clone();
        fresh.set_state();
        let now = fresh.st();
        saved.key == now.key
            && saved.pawn_key == now.pawn_key
            && saved.minor_piece_key == now.minor_piece_key
            && saved.non_pawn_key == now.non_pawn_key
            && saved.material_key == now.material_key
            && saved.non_pawn_material == now.non_pawn_material
            && saved.checkers == now.checkers
    }
}

impl fmt::Display for Position {
    /// The board as UCI `d` prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n +---+---+---+---+---+---+---+---+")?;
        for rank in (0..8).rev() {
            for file in 0..8 {
                write!(f, " | {}", self.piece_on(Square::make(file, rank)).to_char() as char)?;
            }
            writeln!(f, " | {}", rank + 1)?;
            writeln!(f, " +---+---+---+---+---+---+---+---+")?;
        }
        writeln!(f, "   a   b   c   d   e   f   g   h\n")?;
        writeln!(f, "Fen: {}", self.fen())?;
        write!(f, "Key: {:016X}", self.key())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn flipping_twice_is_the_identity() {
        for fen in [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            // En passant flips rank 3 to 6 and back, and castling letters swap case.
            "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        ] {
            let original = super::Position::from_fen(fen, false).expect("valid");
            let mut p = original.clone();
            p.flip().expect("a legal position mirrors to a legal position");
            assert_ne!(p.fen(), original.fen(), "{fen} flipped to itself");
            p.flip().expect("and back");
            assert_eq!(p.fen(), original.fen(), "{fen} did not survive two flips");
        }
    }

    #[test]
    fn a_flip_swaps_the_side_to_move_and_keeps_the_material() {
        let mut p = super::Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            false,
        )
        .expect("valid");
        let before = p.side_to_move();
        let pieces = p.piece_total();
        p.flip().expect("valid");
        assert_ne!(p.side_to_move(), before);
        assert_eq!(p.piece_total(), pieces, "a mirror must not create or destroy pieces");
    }

    use super::*;
    use crate::board::movegen::generate_legal;

    #[test]
    fn start_position_round_trips_through_fen() {
        let pos = Position::startpos();
        assert_eq!(pos.fen(), START_FEN);
        assert_eq!(pos.piece_total(), 32);
        assert_eq!(pos.count(Color::White, PieceType::Pawn), 8);
        assert_eq!(pos.side_to_move(), Color::White);
        assert!(pos.can_castle(CastlingRights::ANY));
        assert!(!pos.in_check());
    }

    #[test]
    fn malformed_fens_are_rejected_with_a_reason() {
        assert_eq!(Position::from_fen("", false).unwrap_err(), FenError::Board);
        assert_eq!(
            Position::from_fen("8/8/8/8/8/8/8/8 w - - 0 1", false).unwrap_err(),
            FenError::Kings
        );
        assert_eq!(
            Position::from_fen("4k3/8/8/8/8/8/8/4K2R x KQ - 0 1", false).unwrap_err(),
            FenError::SideToMove
        );
        // Black to move with White already attacked cannot arise.
        assert!(Position::from_fen("4k3/8/8/8/8/8/8/4K1R1 b - - 0 1", false).is_ok());
        assert_eq!(
            Position::from_fen("4k1R1/8/8/8/8/8/8/4K3 w - - 0 1", false).unwrap_err(),
            FenError::OppositeCheck
        );
    }

    #[test]
    fn make_unmake_restores_every_field() {
        let fens = [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
        ];
        for fen in fens {
            let pos = Position::from_fen(fen, false).expect("test FEN is valid");
            for m in generate_legal(&pos) {
                let mut child = pos.clone();
                child.do_move(m);
                assert!(child.state_is_consistent(), "{fen} after {m:?}");
                child.undo_move(m);
                assert_eq!(child.fen(), pos.fen(), "{fen} after {m:?}");
                assert_eq!(child.key(), pos.key(), "{fen} after {m:?}");
            }
        }
    }

    #[test]
    fn null_move_is_its_own_inverse() {
        let mut pos = Position::startpos();
        let before = pos.fen();
        let key = pos.key();
        pos.do_null_move();
        assert_ne!(pos.key(), key);
        assert_eq!(pos.side_to_move(), Color::Black);
        pos.undo_null_move();
        assert_eq!(pos.fen(), before);
        assert_eq!(pos.key(), key);
    }

    #[test]
    fn incremental_key_matches_recomputation_over_a_line() {
        let mut pos = Position::startpos();
        // 1. e4 e5 2. Nf3 Nc6 3. Bb5 -- ordinary moves plus a double push that does set an
        // en-passant square only when a capture exists.
        for uci in ["e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6"] {
            let m = generate_legal(&pos)
                .into_iter()
                .find(|m| format!("{m:?}") == uci)
                .unwrap_or_else(|| panic!("{uci} is legal here"));
            pos.do_move(m);
            assert!(pos.state_is_consistent(), "after {uci}");
        }
    }

    #[test]
    fn en_passant_square_is_only_set_when_the_capture_exists() {
        // A double push with no enemy pawn beside it records no en-passant square: the key
        // would otherwise differ from a position that is functionally identical.
        let mut pos = Position::startpos();
        let m = generate_legal(&pos)
            .into_iter()
            .find(|m| format!("{m:?}") == "e2e4")
            .expect("e2e4 is legal");
        pos.do_move(m);
        assert!(pos.ep_square().is_none());
        assert!(pos.fen().contains(" - 0 1"));
    }

    #[test]
    fn castling_rights_die_when_a_rook_is_captured() {
        let pos = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", false).expect("valid");
        // Rxa8 removes Black's queen-side right along with the rook.
        let m = generate_legal(&pos)
            .into_iter()
            .find(|m| format!("{m:?}") == "a1a8")
            .expect("Rxa8 is legal");
        let mut child = pos.clone();
        child.do_move(m);
        assert!(!child.can_castle(CastlingRights::BLACK_OOO));
        assert!(child.can_castle(CastlingRights::BLACK_OO));
        assert!(!child.can_castle(CastlingRights::WHITE_OOO));
        assert!(child.state_is_consistent());
    }

    #[test]
    fn see_ranks_an_exchange_by_material() {
        // A pawn takes a queen: winning by any threshold up to the queen's value.
        let pos = Position::from_fen("4k3/8/8/3q4/4P3/8/8/4K3 w - - 0 1", false).expect("valid");
        let m = generate_legal(&pos)
            .into_iter()
            .find(|m| format!("{m:?}") == "e4d5")
            .expect("exd5 is legal");
        assert!(pos.see_ge(m, 0));
        assert!(pos.see_ge(m, QUEEN_VALUE_MINUS_ONE));
    }

    const QUEEN_VALUE_MINUS_ONE: Value = 2537;

    #[test]
    fn repetition_is_detected_and_signed() {
        let mut pos = Position::startpos();
        // Shuffle the knights back and forth: the third occurrence is the threefold.
        for uci in ["g1f3", "g8f6", "f3g1", "f6g8", "g1f3", "g8f6", "f3g1", "f6g8"] {
            let m = generate_legal(&pos)
                .into_iter()
                .find(|m| format!("{m:?}") == uci)
                .unwrap_or_else(|| panic!("{uci} is legal"));
            pos.do_move(m);
        }
        assert!(pos.has_repeated());
        assert!(pos.st().repetition != 0);
    }
}
