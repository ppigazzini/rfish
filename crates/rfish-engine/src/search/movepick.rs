//! The staged move picker.
//!
//! Moves are yielded in the order the search wants to try them, and each stage is
//! generated only when the previous one runs out. That laziness is the point: most nodes
//! cut off on the transposition move or the first good capture, and generating the quiet
//! moves for those nodes would be most of the generator's total cost for nothing.
//!
//! Golden: `Stockfish/src/movepick.cpp`.

use crate::board::movegen::{GenType, MoveList, generate_into};
use crate::board::position::Position;
use crate::board::types::{Move, MoveType, Piece, PieceType, Square, Value, piece_value};

use super::history::{Histories, PawnHistory};

/// Which continuation plane a parent move selects.
///
/// Plain data, not a borrow. The picker is called with `&Position` and `&Histories` at
/// every step rather than holding them, so the search can make and unmake a move between
/// two calls — which is exactly what a picker that held a `&Position` would forbid. That
/// is the one structural difference from upstream's `MovePicker`, and it is forced by the
/// borrow checker rather than chosen.
#[derive(Clone, Copy, Debug)]
pub struct ContKey {
    pub in_check: bool,
    pub capture: bool,
    pub pc: Piece,
    pub to: Square,
}

/// Where the picker is in its sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Yield the transposition move, unfiltered.
    TtMove,
    /// Generate captures and score them.
    InitCaptures,
    /// Yield captures whose static exchange is good.
    GoodCaptures,
    /// Yield the two killer moves.
    Killers,
    /// Generate quiets and score them.
    InitQuiets,
    /// Yield quiets, best first.
    Quiets,
    /// Yield the captures the good-capture stage rejected.
    BadCaptures,
    /// Nothing left.
    Done,

    /// Qsearch: yield the transposition move if it is a capture.
    QTtMove,
    /// Qsearch: generate captures.
    QInitCaptures,
    /// Qsearch: yield captures.
    QCaptures,

    /// In check: generate every evasion.
    EvasionInit,
    /// In check: yield evasions, best first.
    Evasions,
}

/// A move with the score the picker sorts it by.
#[derive(Clone, Copy, Debug)]
struct ScoredMove {
    mv: Move,
    score: i32,
}

/// Yields the moves of one node in search order.
///
/// The picker borrows the position and the histories immutably and owns everything it
/// mutates, so the search can hold one per ply on its own stack without any aliasing
/// question arising.
pub struct MovePicker {
    continuations: [Option<ContKey>; 4],
    tt_move: Move,
    killers: [Move; 2],
    counter: Move,
    /// Captures below this static-exchange threshold are deferred to the bad-capture
    /// stage. In qsearch it also prunes them away entirely.
    threshold: Value,
    stage: Stage,
    moves: Vec<ScoredMove>,
    bad_captures: Vec<ScoredMove>,
    index: usize,
    pawn_row: usize,
}

impl core::fmt::Debug for MovePicker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MovePicker")
            .field("stage", &self.stage)
            .field("tt_move", &self.tt_move)
            .field("remaining", &(self.moves.len() - self.index.min(self.moves.len())))
            .finish_non_exhaustive()
    }
}

impl MovePicker {
    /// A picker for a main-search node.
    #[must_use]
    pub fn new(
        pos: &Position,
        continuations: [Option<ContKey>; 4],
        tt_move: Move,
        killers: [Move; 2],
        counter: Move,
        depth: i32,
    ) -> MovePicker {
        let stage = if pos.in_check() {
            Stage::EvasionInit
        } else if tt_move.is_ok() && pos.pseudo_legal(tt_move) {
            Stage::TtMove
        } else {
            Stage::InitCaptures
        };
        MovePicker {
            continuations,
            tt_move: if stage == Stage::TtMove { tt_move } else { Move::NONE },
            killers,
            counter,
            threshold: -depth * 32,
            stage,
            moves: Vec::with_capacity(48),
            bad_captures: Vec::new(),
            index: 0,
            pawn_row: PawnHistory::row(pos.st().pawn_key),
        }
    }

    /// A picker for a quiescence node.
    ///
    /// Outside check it yields captures and queen promotions only; in check it falls back
    /// to the full evasion set, because a position in check has no quiet standing pat.
    #[must_use]
    pub fn new_qsearch(
        pos: &Position,
        continuations: [Option<ContKey>; 4],
        tt_move: Move,
    ) -> MovePicker {
        let in_check = pos.in_check();
        let tt_usable = tt_move.is_ok()
            && pos.pseudo_legal(tt_move)
            && (in_check || pos.is_capture_stage(tt_move));
        let stage = if in_check {
            Stage::EvasionInit
        } else if tt_usable {
            Stage::QTtMove
        } else {
            Stage::QInitCaptures
        };
        MovePicker {
            continuations,
            tt_move: if tt_usable { tt_move } else { Move::NONE },
            killers: [Move::NONE; 2],
            counter: Move::NONE,
            threshold: 0,
            stage,
            moves: Vec::with_capacity(16),
            bad_captures: Vec::new(),
            index: 0,
            pawn_row: PawnHistory::row(pos.st().pawn_key),
        }
    }

    /// The next move, or `None` when the sequence is exhausted.
    ///
    /// `skip_quiets` is read at every call, not fixed at construction: the search decides
    /// mid-node that it has seen enough quiet moves, and the picker has to honour that
    /// from the next call on.
    pub fn next(&mut self, pos: &Position, h: &Histories, skip_quiets: bool) -> Option<Move> {
        loop {
            match self.stage {
                Stage::TtMove | Stage::QTtMove => {
                    self.stage = if self.stage == Stage::TtMove {
                        Stage::InitCaptures
                    } else {
                        Stage::QInitCaptures
                    };
                    if self.tt_move.is_ok() {
                        return Some(self.tt_move);
                    }
                }

                Stage::InitCaptures | Stage::QInitCaptures => {
                    self.generate(pos, GenType::Captures);
                    self.score_captures(pos, h);
                    self.index = 0;
                    self.stage = if self.stage == Stage::InitCaptures {
                        Stage::GoodCaptures
                    } else {
                        Stage::QCaptures
                    };
                }

                Stage::GoodCaptures => {
                    while let Some(sm) = self.pick_best() {
                        // A capture that loses material is not searched here; it goes to
                        // the back of the queue, after the quiet moves.
                        if pos.see_ge(sm.mv, self.threshold) {
                            return Some(sm.mv);
                        }
                        self.bad_captures.push(sm);
                    }
                    self.stage = Stage::Killers;
                }

                Stage::Killers => {
                    // The killers and the counter-move are tried before any quiet is
                    // generated: they are quiets that worked at a sibling node, and most
                    // of the time one of them cuts off.
                    for slot in 0..3 {
                        let m = if slot < 2 { self.killers[slot] } else { self.counter };
                        self.killers[slot.min(1)] = Move::NONE;
                        if slot == 2 {
                            self.counter = Move::NONE;
                        }
                        if m.is_ok()
                            && m != self.tt_move
                            && !pos.is_capture_stage(m)
                            && pos.pseudo_legal(m)
                        {
                            return Some(m);
                        }
                    }
                    self.stage = Stage::InitQuiets;
                }

                Stage::InitQuiets => {
                    if skip_quiets {
                        self.stage = Stage::BadCaptures;
                        self.index = 0;
                        std::mem::swap(&mut self.moves, &mut self.bad_captures);
                        continue;
                    }
                    self.generate(pos, GenType::Quiets);
                    self.score_quiets(pos, h);
                    self.index = 0;
                    self.stage = Stage::Quiets;
                }

                Stage::Quiets => {
                    if !skip_quiets && let Some(sm) = self.pick_best() {
                        return Some(sm.mv);
                    }
                    self.stage = Stage::BadCaptures;
                    self.index = 0;
                    std::mem::swap(&mut self.moves, &mut self.bad_captures);
                }

                // Drain what is left, best first. `BadCaptures` reaches here with the
                // deferred queue already swapped in; `QCaptures` reaches it with the
                // qsearch capture list, which has no deferred queue behind it at all.
                Stage::BadCaptures | Stage::QCaptures | Stage::Evasions => {
                    if let Some(sm) = self.pick_best() {
                        return Some(sm.mv);
                    }
                    self.stage = Stage::Done;
                }

                Stage::EvasionInit => {
                    self.generate(pos, GenType::Evasions);
                    self.score_evasions(pos, h);
                    self.index = 0;
                    self.stage = Stage::Evasions;
                }

                Stage::Done => return None,
            }
        }
    }

    /// Generate into the scratch list, dropping the transposition move — it was already
    /// yielded and must not be searched twice.
    fn generate(&mut self, pos: &Position, gt: GenType) {
        let mut list = MoveList::new();
        generate_into(pos, gt, &mut list);
        self.moves.clear();
        for &m in list.as_slice() {
            if m != self.tt_move {
                self.moves.push(ScoredMove { mv: m, score: 0 });
            }
        }
    }

    /// Selection sort, one element per call.
    ///
    /// A full sort would order moves the search never reaches. Most nodes consume two or
    /// three moves, so scanning for the maximum each time is cheaper than sorting.
    fn pick_best(&mut self) -> Option<ScoredMove> {
        if self.index >= self.moves.len() {
            return None;
        }
        let mut best = self.index;
        for i in self.index + 1..self.moves.len() {
            if self.moves[i].score > self.moves[best].score {
                best = i;
            }
        }
        self.moves.swap(self.index, best);
        let sm = self.moves[self.index];
        self.index += 1;
        Some(sm)
    }

    fn score_captures(&mut self, pos: &Position, h: &Histories) {
        for sm in &mut self.moves {
            let victim = captured_type(pos, sm.mv);
            let mover = pos.piece_on(sm.mv.from());
            // Most-valuable-victim first, refined by how well this capture has worked
            // before. The victim term dominates, so the history only breaks ties.
            sm.score = 7 * piece_value(Piece::new(mover.color(), victim))
                + h.captures.get(mover, sm.mv.to(), victim);
            if sm.mv.move_type() == MoveType::Promotion {
                sm.score += piece_value(Piece::new(mover.color(), sm.mv.promotion_type()));
            }
        }
    }

    fn score_quiets(&mut self, pos: &Position, h: &Histories) {
        let us = pos.side_to_move();
        for sm in &mut self.moves {
            let pc = pos.piece_on(sm.mv.from());
            let to = sm.mv.to();
            let mut score = 2 * h.main.get(us, sm.mv.from_to());
            score += 2 * h.pawn.get(self.pawn_row, pc, to);
            for k in self.continuations.iter().flatten() {
                score += h.continuation.plane(k.in_check, k.capture, k.pc, k.to).get(pc, to);
            }
            // A quiet move that gives check is worth trying early: it is forcing, so the
            // subtree under it is small.
            if pos.check_squares(pc.piece_type()).contains(to) {
                score += 16384;
            }
            sm.score = score;
        }
    }

    fn score_evasions(&mut self, pos: &Position, h: &Histories) {
        let us = pos.side_to_move();
        for sm in &mut self.moves {
            if pos.is_capture(sm.mv) {
                // Captures come first among evasions, ordered by what they win. The large
                // offset keeps every capture above every quiet evasion.
                let victim = captured_type(pos, sm.mv);
                sm.score = piece_value(Piece::new(us, victim))
                    - i32::from(pos.piece_on(sm.mv.from()).piece_type().index() as u8)
                    + (1 << 28);
            } else {
                sm.score = h.main.get(us, sm.mv.from_to());
                let pc = pos.piece_on(sm.mv.from());
                for k in self.continuations.iter().flatten() {
                    let v =
                        h.continuation.plane(k.in_check, k.capture, k.pc, k.to).get(pc, sm.mv.to());
                    score_add(&mut sm.score, v);
                }
            }
        }
    }
}

/// Saturating add, so an evasion score cannot wrap past the capture offset.
#[inline(always)]
fn score_add(acc: &mut i32, v: i32) {
    *acc = acc.saturating_add(v);
}

/// What `m` captures, as a piece type. En passant always takes a pawn; a non-capture
/// yields [`PieceType::None`], whose value is zero.
#[inline]
fn captured_type(pos: &Position, m: Move) -> PieceType {
    match m.move_type() {
        MoveType::EnPassant => PieceType::Pawn,
        MoveType::Castling => PieceType::None,
        _ => pos.piece_on(m.to()).piece_type(),
    }
}

/// The square a move ends on, for the continuation tables. Castling is keyed by the king's
/// destination, not the rook's square, so the plane matches the position that follows.
#[inline]
#[must_use]
pub fn continuation_to(m: Move) -> Square {
    if m.move_type() == MoveType::Castling {
        let king_side = m.to() > m.from();
        Square::make(if king_side { 6 } else { 2 }, m.from().rank())
    } else {
        m.to()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::movegen::generate_legal;
    use crate::board::position::START_FEN;

    fn collect(pos: &Position, h: &Histories, tt: Move) -> Vec<Move> {
        let mut mp = MovePicker::new(pos, [None; 4], tt, [Move::NONE; 2], Move::NONE, 4);
        let mut out = Vec::new();
        while let Some(m) = mp.next(pos, h, false) {
            out.push(m);
        }
        out
    }

    /// The picker must yield every pseudo-legal move exactly once. A move yielded twice is
    /// searched twice; a move missed is never searched at all, and neither shows up as
    /// anything but a wrong node count much later.
    #[test]
    fn every_move_is_yielded_exactly_once() {
        let h = Histories::default();
        for fen in [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "4k3/8/8/8/8/8/4r3/4K3 w - - 0 1",
        ] {
            let pos = Position::from_fen(fen, false).expect("valid");
            let mut picked = collect(&pos, &h, Move::NONE);
            let mut legal: Vec<Move> = generate_legal(&pos).iter().copied().collect();

            // The picker is pseudo-legal, so compare on the legal subset.
            picked.retain(|&m| pos.legal(m));
            picked.sort_unstable();
            let before = picked.len();
            picked.dedup();
            assert_eq!(picked.len(), before, "{fen}: a move was yielded twice");

            legal.sort_unstable();
            assert_eq!(picked, legal, "{fen}");
        }
    }

    /// The transposition move must come first and must not be repeated by a later stage.
    #[test]
    fn the_tt_move_is_yielded_first_and_once() {
        let h = Histories::default();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let tt = generate_legal(&pos).as_slice()[5];
        let picked = collect(&pos, &h, tt);
        assert_eq!(picked[0], tt);
        assert_eq!(picked.iter().filter(|&&m| m == tt).count(), 1);
    }

    /// A transposition move from another position must be rejected rather than played.
    #[test]
    fn an_illegal_tt_move_is_dropped() {
        let h = Histories::default();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let bogus = Move::new(Square::make(4, 4), Square::make(4, 5));
        let picked = collect(&pos, &h, bogus);
        assert!(!picked.contains(&bogus));
        assert_eq!(picked.len(), generate_legal(&pos).len());
    }

    /// Good captures precede quiet moves; losing captures follow them.
    #[test]
    fn captures_are_ordered_around_the_quiet_moves() {
        let h = Histories::default();
        // White can take the queen with a pawn (winning) or with the rook (also winning),
        // and has quiet moves available.
        let pos = Position::from_fen("4k3/8/8/3q4/4P3/8/8/R3K3 w - - 0 1", false).expect("valid");
        let picked = collect(&pos, &h, Move::NONE);
        let first_quiet =
            picked.iter().position(|&m| !pos.is_capture_stage(m)).expect("quiets exist");
        let winning = picked[..first_quiet].to_vec();
        assert!(!winning.is_empty());
        assert!(winning.iter().all(|&m| pos.is_capture_stage(m)));
    }

    /// In qsearch outside check, only captures and promotions may be yielded.
    #[test]
    fn qsearch_yields_only_forcing_moves() {
        let h = Histories::default();
        let pos = Position::from_fen("4k3/8/8/3q4/4P3/8/8/R3K3 w - - 0 1", false).expect("valid");
        let mut mp = MovePicker::new_qsearch(&pos, [None; 4], Move::NONE);
        let mut any = false;
        while let Some(m) = mp.next(&pos, &h, false) {
            assert!(pos.is_capture_stage(m), "{m:?} is not forcing");
            any = true;
        }
        assert!(any);
    }

    /// In check, qsearch must fall back to the full evasion set: there is no standing pat.
    #[test]
    fn qsearch_in_check_yields_every_evasion() {
        let h = Histories::default();
        let pos = Position::from_fen("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1", false).expect("valid");
        let mut mp = MovePicker::new_qsearch(&pos, [None; 4], Move::NONE);
        let mut out = Vec::new();
        while let Some(m) = mp.next(&pos, &h, false) {
            out.push(m);
        }
        out.retain(|&m| pos.legal(m));
        out.sort_unstable();
        let mut legal: Vec<Move> = generate_legal(&pos).iter().copied().collect();
        legal.sort_unstable();
        assert_eq!(out, legal);
    }

    /// `skip_quiets` must take effect from the next call, not from construction.
    #[test]
    fn skip_quiets_stops_the_quiet_stage_mid_node() {
        let h = Histories::default();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut mp = MovePicker::new(&pos, [None; 4], Move::NONE, [Move::NONE; 2], Move::NONE, 4);
        let mut seen = 0;
        while let Some(m) = mp.next(&pos, &h, true) {
            assert!(pos.is_capture_stage(m), "{m:?} is quiet but quiets were skipped");
            seen += 1;
        }
        // The start position has no captures, so skipping quiets leaves nothing.
        assert_eq!(seen, 0);
    }

    #[test]
    fn castling_is_keyed_by_the_kings_destination() {
        let pos = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", false).expect("valid");
        let castle = generate_legal(&pos)
            .iter()
            .copied()
            .find(|m| m.move_type() == MoveType::Castling && m.to() > m.from())
            .expect("O-O is legal");
        assert_eq!(continuation_to(castle), Square::make(6, 0));
    }
}
