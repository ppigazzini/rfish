//! Move generation.
//!
//! Every generator here is PSEUDO-legal except [`generate_legal`]: a move it produces is
//! reachable by the piece's geometry and lands on a square its own side does not occupy,
//! but it may still leave its own king in check. [`crate::board::position::Position::legal`]
//! is the filter, and the search applies it lazily so the cost is paid only for moves it
//! actually searches.
//!
//! The generated ORDER is part of the port's contract. Two engines that generate the same
//! set in a different order search different trees once move ordering is only partially
//! deterministic, so a generator is faithful only when it emits moves in upstream's
//! sequence.
//!
//! Golden: `Stockfish/src/movegen.cpp`.

use core::ops::Deref;

use super::attacks::{between_bb, piece_attacks};
use super::bitboard::{Bitboard, KING_ATTACKS, pawn_attacks_from, relative_rank_bb};
use super::position::Position;
use super::types::{
    CastlingRights, Color, Direction, MAX_MOVES, Move, MoveType, PieceType, Square,
};

/// A fixed-capacity move buffer.
///
/// 256 is upstream's `MAX_MOVES`, and it is a proven bound rather than a guess: no
/// reachable chess position has more legal moves. Overflowing it panics instead of
/// writing past the end, which is the whole difference from the C++ array it replaces.
#[derive(Clone)]
pub struct MoveList {
    moves: [Move; MAX_MOVES],
    len: usize,
}

impl MoveList {
    /// An empty list.
    #[must_use]
    pub fn new() -> MoveList {
        MoveList { moves: [Move::NONE; MAX_MOVES], len: 0 }
    }

    /// Append `m`.
    ///
    /// # Panics
    /// Panics when the list is full. See the type note: that bound is a property of chess,
    /// so a panic here means the generator produced a move twice.
    #[inline(always)]
    pub fn push(&mut self, m: Move) {
        assert!(self.len < MAX_MOVES, "move list overflow");
        self.moves[self.len] = m;
        self.len += 1;
    }

    /// The moves generated so far.
    #[inline(always)]
    #[must_use]
    pub fn as_slice(&self) -> &[Move] {
        &self.moves[..self.len]
    }

    /// The number of moves.
    #[inline(always)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing has been generated.
    #[inline(always)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drop every move for which `keep` is false, preserving order.
    ///
    /// The legality filter: generation is pseudo-legal, and this is what makes it legal.
    pub fn retain(&mut self, mut keep: impl FnMut(Move) -> bool) {
        let mut out = 0;
        for i in 0..self.len {
            if keep(self.moves[i]) {
                self.moves[out] = self.moves[i];
                out += 1;
            }
        }
        self.len = out;
    }
}

impl Default for MoveList {
    fn default() -> MoveList {
        MoveList::new()
    }
}

impl Deref for MoveList {
    type Target = [Move];

    fn deref(&self) -> &[Move] {
        self.as_slice()
    }
}

impl core::fmt::Debug for MoveList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl IntoIterator for MoveList {
    type Item = Move;
    type IntoIter = core::iter::Take<core::array::IntoIter<Move, MAX_MOVES>>;

    fn into_iter(self) -> Self::IntoIter {
        self.moves.into_iter().take(self.len)
    }
}

/// Anything generation can append a move to.
///
/// Exists so the move picker can generate STRAIGHT into its own scored buffer. Before it,
/// generation always built a fresh [`MoveList`] first, and a `MoveList` is a 256-entry
/// array that safe Rust has to initialise: 512 bytes of `memset` per generation, 593k of
/// them on a depth-9 bench, against a Stockfish that leaves the same buffer undefined and
/// pays nothing. The trait is generic rather than dynamic, so each generator monomorphises
/// into the sink it is given and the indirection costs nothing.
pub trait MoveSink {
    /// Append one move.
    fn push_move(&mut self, m: Move);
}

impl MoveSink for MoveList {
    #[inline(always)]
    fn push_move(&mut self, m: Move) {
        self.push(m);
    }
}

/// Which subset of moves a generator produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenType {
    /// Captures and queen promotions — what qsearch searches.
    Captures,
    /// Everything else: quiet moves and under-promotions.
    Quiets,
    /// Every pseudo-legal move, for a position that is NOT in check.
    NonEvasions,
    /// Every pseudo-legal move that could answer a check.
    Evasions,
}

/// Generate the pseudo-legal moves of kind `gt`.
///
/// When the side to move is in check, `Captures`, `Quiets` and `NonEvasions` are still
/// answered from the evasion set, filtered — a move that does not answer the check is not
/// pseudo-legal in a checked position.
#[must_use]
pub fn generate(pos: &Position, gt: GenType) -> MoveList {
    let mut list = MoveList::new();
    generate_into(pos, gt, &mut list);
    list
}

/// Generate into an existing list, so the move picker can reuse one buffer per ply.
pub fn generate_into<S: MoveSink>(pos: &Position, gt: GenType, list: &mut S) {
    let us = pos.side_to_move();
    let checkers = pos.checkers();

    // The target mask is what makes one generator body serve every kind: the piece loops
    // are identical, only the set of acceptable destinations changes.
    let target = if checkers.any() {
        if checkers.more_than_one() {
            // A double check can only be answered by a king move, so skip the piece loops
            // entirely and go straight to the king.
            generate_king_moves(pos, us, !pos.colored(us), list);
            return;
        }
        // A single check: block on the line, or capture the checker.
        between_bb(pos.king_square(us), checkers.lsb())
    } else {
        match gt {
            GenType::Captures => pos.colored(!us),
            GenType::Quiets => !pos.occupied(),
            GenType::NonEvasions | GenType::Evasions => !pos.colored(us),
        }
    };

    // Under check the target is fixed by the checker, so a Captures/Quiets request has to
    // be narrowed afterwards rather than before.
    let target = if checkers.any() {
        match gt {
            GenType::Captures => target & pos.colored(!us),
            GenType::Quiets => target & !pos.occupied(),
            _ => target,
        }
    } else {
        target
    };

    generate_pawn_moves(pos, us, gt, target, list);

    // Written out per piece type rather than looped over one, so `piece_attacks` resolves to
    // that type's own kernel at each site. Through a RUNTIME piece type the match inside it
    // cannot fold, and what is left is an indirect branch taken once per attacker that the
    // predictor misses -- the same reason the NNUE threat scan is written out, where
    // ../zfish 24883582 measured 193K misses over its bench from one such call.
    //
    // The ORDER is the loop's and must stay: two generators that emit the same set in a
    // different sequence search different trees, because the move picker's partial sort
    // leaves equal-scored moves in generation order.
    macro_rules! generate_for {
        ($pt:expr) => {{
            for from in pos.pieces_of(us, $pt) {
                for to in piece_attacks($pt, from, pos.occupied()) & target {
                    list.push_move(Move::new(from, to));
                }
            }
        }};
    }
    generate_for!(PieceType::Knight);
    generate_for!(PieceType::Bishop);
    generate_for!(PieceType::Rook);
    generate_for!(PieceType::Queen);

    // The king is generated last, and never restricted to the check-answering target: it
    // escapes by leaving, which no target mask describes.
    let king_target = if checkers.any() {
        match gt {
            GenType::Captures => pos.colored(!us),
            GenType::Quiets => !pos.occupied(),
            _ => !pos.colored(us),
        }
    } else {
        target
    };
    generate_king_moves(pos, us, king_target, list);

    // Castling is a quiet move and impossible while in check.
    if checkers.is_empty() && matches!(gt, GenType::Quiets | GenType::NonEvasions) {
        generate_castling(pos, us, list);
    }
}

fn generate_king_moves<S: MoveSink>(pos: &Position, us: Color, target: Bitboard, list: &mut S) {
    let ksq = pos.king_square(us);
    for to in KING_ATTACKS[ksq.index()] & target {
        list.push_move(Move::new(ksq, to));
    }
}

fn generate_castling<S: MoveSink>(pos: &Position, us: Color, list: &mut S) {
    for cr in [CastlingRights::king_side(us), CastlingRights::queen_side(us)] {
        if pos.can_castle(cr) && !pos.castling_impeded(cr) {
            // The move encodes the ROOK's square as its destination -- upstream's
            // king-takes-rook convention, which is what makes Chess960 castling fit the
            // same 16 bits as every other move.
            list.push_move(Move::typed(
                MoveType::Castling,
                pos.king_square(us),
                pos.castling_rook_square(cr),
                PieceType::Knight,
            ));
        }
    }
}

fn generate_pawn_moves<S: MoveSink>(
    pos: &Position,
    us: Color,
    gt: GenType,
    target: Bitboard,
    list: &mut S,
) {
    let them = !us;
    let up = Direction::pawn_push(us);
    let up_right = if us == Color::White { Direction::NorthEast } else { Direction::SouthWest };
    let up_left = if us == Color::White { Direction::NorthWest } else { Direction::SouthEast };

    let seventh = relative_rank_bb(us, 6);
    let third = relative_rank_bb(us, 2);
    let pawns = pos.pieces_of(us, PieceType::Pawn);
    let on_seventh = pawns & seventh;
    let not_on_seventh = pawns & !seventh;
    let empty = !pos.occupied();
    let enemies = pos.colored(them);

    // Under check only the checker can be captured, so the enemy set collapses to it.
    let enemies = if gt == GenType::Evasions { pos.checkers() } else { enemies };
    let evasions = gt == GenType::Evasions;

    // Single and double pushes. A pawn on the seventh is handled by the promotion block,
    // never here, so a Normal move can never reach the last rank.
    if gt != GenType::Captures {
        let mut b1 = not_on_seventh.shift(up) & empty;
        let mut b2 = (b1 & third).shift(up) & empty;

        // Under check a push only counts when it blocks; the target says which squares do.
        if evasions {
            b1 &= target;
            b2 &= target;
        }

        let back = opposite(up);
        for to in b1 {
            list.push_move(Move::new(to.shift(back), to));
        }
        for to in b2 {
            list.push_move(Move::new(to.shift(back).shift(back), to));
        }
    }

    // Promotions and underpromotions.
    //
    // The split between the two move classes is not "queens are captures, the rest are
    // quiet". A CAPTURING promotion contributes all four pieces to `Captures`; a PUSHING
    // promotion contributes only the queen there and its three underpromotions to
    // `Quiets`. That asymmetry is upstream's, and it is what makes `capture_stage` agree
    // with the class a move was generated in.
    if on_seventh.any() {
        let b1 = on_seventh.shift(up_right) & enemies;
        let b2 = on_seventh.shift(up_left) & enemies;
        let mut b3 = on_seventh.shift(up) & empty;
        if evasions {
            b3 &= target;
        }

        // Captures to the right, then to the left, then pushes. The order is load-bearing:
        // the move picker's partial sort leaves equal-scored moves in generation order.
        for (set, dir, enemy) in [(b1, up_right, true), (b2, up_left, true), (b3, up, false)] {
            let back = opposite(dir);
            for to in set {
                let from = to.shift(back);
                if gt != GenType::Quiets {
                    list.push_move(Move::typed(MoveType::Promotion, from, to, PieceType::Queen));
                }
                let underpromote = match gt {
                    GenType::Captures => enemy,
                    GenType::Quiets => !enemy,
                    GenType::Evasions | GenType::NonEvasions => true,
                };
                if underpromote {
                    for promo in [PieceType::Rook, PieceType::Bishop, PieceType::Knight] {
                        list.push_move(Move::typed(MoveType::Promotion, from, to, promo));
                    }
                }
            }
        }
    }

    // Ordinary captures, and en passant.
    if gt != GenType::Quiets {
        for (set, dir) in
            [(not_on_seventh.shift(up_right), up_right), (not_on_seventh.shift(up_left), up_left)]
        {
            let back = opposite(dir);
            for to in set & enemies {
                list.push_move(Move::new(to.shift(back), to));
            }
        }

        let ep = pos.ep_square();
        if ep.is_ok() {
            // An en passant capture cannot resolve a discovered check: it removes the
            // pawn that blocked the line, so the check remains.
            if evasions && (target & Bitboard::from_square(ep.shift(up))).any() {
                return;
            }
            for from in pawn_attacks_from(them, ep) & not_on_seventh {
                list.push_move(Move::typed(MoveType::EnPassant, from, ep, PieceType::Knight));
            }
        }
    }
}

/// The reverse of a one-square step.
#[inline(always)]
const fn opposite(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::South,
        Direction::South => Direction::North,
        Direction::East => Direction::West,
        Direction::West => Direction::East,
        Direction::NorthEast => Direction::SouthWest,
        Direction::SouthWest => Direction::NorthEast,
        Direction::NorthWest => Direction::SouthEast,
        Direction::SouthEast => Direction::NorthWest,
    }
}

/// Every legal move in the position.
#[must_use]
pub fn generate_legal(pos: &Position) -> MoveList {
    let gt = if pos.in_check() { GenType::Evasions } else { GenType::NonEvasions };
    let mut list = generate(pos, gt);
    // Upstream filters IN PLACE by moving the LAST element over the rejected one, which does
    // not preserve order, and the order is observable: `syzygy_extend_pv` picks the first of
    // several moves the tables rank equally, so a stable compaction here shows a different
    // winning line from upstream's for the same position. Reproduce the swap, not the intent.
    let mut cur = 0;
    while cur < list.len {
        let m = list.moves[cur];
        if needs_legality_test(pos, m) && !pos.legal(m) {
            list.len -= 1;
            list.moves[cur] = list.moves[list.len];
        } else {
            cur += 1;
        }
    }
    list
}

/// Whether `m` could possibly be illegal, so the expensive test is worth running.
///
/// Only three classes can be: a piece that is pinned against its own king, a king move
/// (which must not walk into an attack), and an en-passant capture (which removes two
/// pieces from one rank and can expose the king along it). Everything else was already
/// proven legal by generation, and upstream's `generate<LEGAL>` filters on exactly this
/// predicate rather than testing every move.
///
/// Testing everything is not a small overcharge: `Position::legal` is one of the hottest
/// functions in the engine, and a full pass over ~35 pseudo-legal moves runs it about
/// seven times more often than it is needed.
#[inline]
fn needs_legality_test(pos: &Position, m: Move) -> bool {
    let us = pos.side_to_move();
    (pos.blockers_for_king(us) & pos.colored(us)).contains(m.from())
        || m.from() == pos.king_square(us)
        || m.move_type() == MoveType::EnPassant
}

/// True when the side to move has at least one legal move.
///
/// Cheaper than generating them all: the stalemate and checkmate tests only need to know
/// whether the set is empty, and the first legal move answers that.
#[must_use]
pub fn has_legal_move(pos: &Position) -> bool {
    let gt = if pos.in_check() { GenType::Evasions } else { GenType::NonEvasions };
    generate(pos, gt).iter().any(|&m| !needs_legality_test(pos, m) || pos.legal(m))
}

/// Count the leaf nodes of the game tree to `depth`.
///
/// The reference test for the whole board zone: perft counts are facts about chess,
/// identical for every correct engine, so a mismatch is always a bug here and never a
/// candidate for a golden update.
#[must_use]
pub fn perft(pos: &mut Position, depth: u32) -> u64 {
    if depth == 0 {
        return 1;
    }
    let moves = generate_legal(pos);
    if depth == 1 {
        return moves.len() as u64;
    }
    let mut nodes = 0;
    for m in moves.as_slice() {
        pos.do_move(*m);
        nodes += perft(pos, depth - 1);
        pos.undo_move(*m);
    }
    nodes
}

/// Perft with the per-move breakdown UCI's `go perft` prints.
#[must_use]
pub fn perft_divide(pos: &mut Position, depth: u32) -> (Vec<(Move, u64)>, u64) {
    let mut out = Vec::new();
    let mut total = 0;
    for m in generate_legal(pos).as_slice() {
        pos.do_move(*m);
        let n = if depth <= 1 { 1 } else { perft(pos, depth - 1) };
        pos.undo_move(*m);
        out.push((*m, n));
        total += n;
    }
    (out, total)
}

/// Find the move `uci` names among the legal moves, in UCI long algebraic notation.
///
/// Castling is accepted in both spellings: `e1g1` (what a GUI sends in standard chess) and
/// `e1h1` (the king-takes-rook form Chess960 requires).
#[must_use]
pub fn parse_uci_move(pos: &Position, uci: &str) -> Option<Move> {
    let legal = generate_legal(pos);
    for &m in legal.as_slice() {
        if format!("{m:?}") == uci {
            return Some(m);
        }
        // A castling move renders as king-takes-rook; also accept the king's destination.
        if m.move_type() == MoveType::Castling && !pos.is_chess960() {
            let king_side = m.to() > m.from();
            let king_to = Square::make(if king_side { 6 } else { 2 }, m.from().rank());
            if format!("{}{}", m.from(), king_to) == uci {
                return Some(m);
            }
        }
    }
    None
}

/// Render `m` in the notation the UCI protocol expects for this position.
#[must_use]
pub fn move_to_uci(pos: &Position, m: Move) -> String {
    if m.is_none() {
        return "(none)".to_string();
    }
    if m == Move::NULL {
        return "0000".to_string();
    }
    // Standard chess names the king's destination; Chess960 names the rook's square,
    // because g1 may already hold a piece and the move would be ambiguous.
    if m.move_type() == MoveType::Castling && !pos.is_chess960() {
        let king_side = m.to() > m.from();
        let king_to = Square::make(if king_side { 6 } else { 2 }, m.from().rank());
        return format!("{}{}", m.from(), king_to);
    }
    format!("{m:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::position::START_FEN;

    /// The standard perft battery. These counts are facts about chess: every one is
    /// published, reproduced by every correct engine, and a mismatch localises the bug to
    /// this zone without any reference to Stockfish's behaviour.
    #[test]
    fn perft_matches_the_reference_positions() {
        let cases: &[(&str, &[u64])] = &[
            (START_FEN, &[20, 400, 8902, 197_281, 4_865_609]),
            (
                "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
                &[48, 2039, 97_862, 4_085_603],
            ),
            ("8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1", &[14, 191, 2812, 43_238, 674_624]),
            (
                "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
                &[6, 264, 9467, 422_333],
            ),
            (
                "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
                &[44, 1486, 62_379, 2_103_487],
            ),
            (
                "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
                &[46, 2079, 89_890, 3_894_594],
            ),
        ];

        for (fen, expected) in cases {
            let mut pos = Position::from_fen(fen, false).expect("reference FEN is valid");
            for (i, &want) in expected.iter().enumerate() {
                let depth = i as u32 + 1;
                let got = perft(&mut pos, depth);
                assert_eq!(got, want, "{fen} at depth {depth}");
            }
        }
    }

    /// Chess960 castling is a data lookup, not a special case, so it needs its own perft.
    #[test]
    fn perft_matches_the_chess960_positions() {
        let cases: &[(&str, &[u64])] = &[
            (
                "bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9",
                &[21, 528, 12_189, 326_672],
            ),
            (
                "2nnrbkr/p1qppppp/8/1ppb4/6PP/3PP3/PPP2P2/BQNNRBKR w HEhe - 1 9",
                &[21, 807, 18_002, 667_366],
            ),
            (
                "b1q1rrkb/pppppppp/3nn3/8/P7/1PPP4/4PPPP/BQNNRKRB w GE - 1 9",
                &[20, 479, 10_471, 273_318],
            ),
        ];
        for (fen, expected) in cases {
            let mut pos = Position::from_fen(fen, true).expect("reference FEN is valid");
            for (i, &want) in expected.iter().enumerate() {
                let depth = i as u32 + 1;
                assert_eq!(perft(&mut pos, depth), want, "{fen} at depth {depth}");
            }
        }
    }

    #[test]
    fn generators_partition_the_move_set() {
        let fens = [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        ];
        for fen in fens {
            let pos = Position::from_fen(fen, false).expect("valid");
            if pos.in_check() {
                continue;
            }
            let mut all: Vec<Move> = generate(&pos, GenType::NonEvasions).to_vec();
            let mut split: Vec<Move> = generate(&pos, GenType::Captures).to_vec();
            split.extend(generate(&pos, GenType::Quiets).iter().copied());
            all.sort_unstable();
            split.sort_unstable();
            assert_eq!(all, split, "{fen}: captures + quiets must partition the move set");
        }
    }

    #[test]
    fn evasions_answer_every_check_and_nothing_else() {
        // A position in check: every generated move must be answered by the legality
        // filter agreeing, and the set must equal the filtered full generation.
        let pos = Position::from_fen("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1", false).expect("valid");
        assert!(pos.in_check());
        let legal = generate_legal(&pos);
        assert!(!legal.is_empty());
        for &m in legal.as_slice() {
            let mut child = pos.clone();
            child.do_move(m);
            // After a legal evasion the mover's king cannot be capturable.
            let ksq = child.king_square(Color::White);
            assert!((child.attackers_to(ksq) & child.colored(Color::Black)).is_empty());
        }
    }

    #[test]
    fn checkmate_and_stalemate_have_no_legal_moves() {
        // Fool's mate.
        let mate = Position::from_fen(
            "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3",
            false,
        )
        .expect("valid");
        assert!(mate.in_check());
        assert!(!has_legal_move(&mate));

        // Stalemate: Black to move, not in check, no legal move.
        let stale = Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1", false).expect("valid");
        assert!(!stale.in_check());
        assert!(!has_legal_move(&stale));
    }

    #[test]
    fn en_passant_is_generated_and_legal_only_when_it_is() {
        let pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1", false).expect("valid");
        assert_eq!(pos.ep_square().to_string(), "d6");
        assert!(generate_legal(&pos).iter().any(|m| m.move_type() == MoveType::EnPassant));

        // The classic pin: taking en passant would expose the king along the rank.
        let pinned = Position::from_fen("8/8/8/K2pP2r/8/8/8/7k w - d6 0 1", false).expect("valid");
        assert!(!generate_legal(&pinned).iter().any(|m| m.move_type() == MoveType::EnPassant));
    }

    #[test]
    fn uci_move_notation_round_trips() {
        let pos = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", false).expect("valid");
        for &m in generate_legal(&pos).as_slice() {
            let text = move_to_uci(&pos, m);
            assert_eq!(parse_uci_move(&pos, &text), Some(m), "{text}");
        }
        // Standard chess spells castling by the king's destination.
        assert!(generate_legal(&pos).iter().any(|&m| move_to_uci(&pos, m) == "e1g1"));
    }

    #[test]
    fn move_list_retains_in_order_and_bounds_its_capacity() {
        let mut list = MoveList::new();
        for i in 1..10u16 {
            list.push_move(Move::from_raw(i));
        }
        list.retain(|m| m.raw() % 2 == 0);
        assert_eq!(
            list.as_slice().iter().copied().map(Move::raw).collect::<Vec<_>>(),
            vec![2, 4, 6, 8]
        );
    }
}
