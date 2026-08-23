//! The staged move picker.
//!
//! Moves are yielded in the order the search wants to try them, and each stage is
//! generated only when the previous one runs out. That laziness is the point: most nodes
//! cut off on the transposition move or the first good capture, and generating the quiet
//! moves for those nodes would be most of the generator's total cost for nothing.
//!
//! # The order within a stage is a partial sort, not a selection sort
//!
//! Upstream sorts each generated list ONCE with an insertion sort that only orders the
//! entries at or above a threshold, and leaves the rest in generation order. That is not
//! the same permutation a repeated pick-the-maximum scan produces: ties break differently,
//! and so does the order of everything below the threshold. Since equal-scored moves are
//! common — whole blocks of quiet moves share a history value early in a search — the
//! difference is visible in the node count, so the sort is ported rather than replaced.
//!
//! Golden: `Stockfish/src/movepick.cpp`.

use crate::board::movegen::{GenType, MoveSink, generate_into};
use crate::board::position::Position;
use crate::board::types::{
    File, MAX_MOVES, Move, MoveType, Piece, PieceType, Ply, Square, VALUE_ZERO, Value, piece_value,
};

use super::history::{ContKey, Histories, PawnHistory, PawnRow};

/// The six continuation planes a node reads, one to six plies back.
///
/// Plain data, not a borrow. The picker is called with `&Position` and `&Histories` at
/// every step rather than holding them, so the search can make and unmake a move between
/// two calls — which is exactly what a picker that held a `&Position` would forbid. That
/// is the one structural difference from upstream's `MovePicker`, and it is forced by the
/// borrow checker rather than chosen.
///
/// [`ContKey`] itself, and the plane a sequence never reads, are owned by
/// [`super::history`] alongside the table they index.
pub type ContKeys = [ContKey; 6];

/// Where the picker is in its sequence. The numbering is upstream's, and the fallthrough
/// order below depends on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    MainTt,
    CaptureInit,
    GoodCapture,
    QuietInit,
    GoodQuiet,
    BadCapture,
    BadQuiet,

    EvasionTt,
    EvasionInit,
    Evasion,

    ProbCutTt,
    ProbCutInit,
    ProbCut,

    QSearchTt,
    QCaptureInit,
    QCapture,

    /// Not upstream's: the sequence has ended. Upstream falls off the end of `select`
    /// instead, which needs a pointer one past the last move; an explicit terminal state
    /// is the same behaviour without the pointer.
    Done,
}

/// A move with the score the picker sorts it by.
#[derive(Clone, Copy, Debug)]
pub struct ScoredMove {
    mv: Move,
    score: i32,
}

/// One node's move buffer, owned by the worker and lent to the picker per call.
///
/// A `Vec` whose allocation is REUSED, never a fresh one. Upstream's picker carries
/// `ExtMove moves[MAX_MOVES]` inline; the direct translation of that -- an inline array in
/// the picker -- is measurably WORSE here, because safe Rust must initialise 2 KB per node
/// where C++ leaves it undefined. Measured on an identical tree: the inline array removed
/// 97M instructions of allocator traffic and added 473M of initialisation.
///
/// The workhorse-collection pattern from the Rust Performance Book gets both: `clear()`
/// keeps the capacity, so after the first visit to a slot there is neither an allocation
/// nor an initialisation.
pub struct MoveBuf {
    /// Boxed so a `SearchWorker` stays movable: the pool holds one of these per ply slot.
    moves: Box<[ScoredMove; MAX_MOVES]>,
    len: usize,
}

impl MoveBuf {
    /// An empty buffer over a fully initialised array.
    ///
    /// The array is written once, here, and never again -- which is the whole difference
    /// from the `Vec` this replaced. A `Vec` carries its capacity in memory, so every
    /// `push` LOADS it, compares, and keeps a call to `RawVec::grow_one` on the cold side:
    /// 21 such calls sat in `generate_append` alone, for a buffer that was created at
    /// `MAX_MOVES` and can never grow. Here the bound is an immediate.
    #[must_use]
    pub fn new() -> MoveBuf {
        MoveBuf { moves: Box::new([ScoredMove { mv: Move::NONE, score: 0 }; MAX_MOVES]), len: 0 }
    }

    /// Drop every move, keeping the storage.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for MoveBuf {
    fn default() -> MoveBuf {
        MoveBuf::new()
    }
}

impl core::fmt::Debug for MoveBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl core::ops::Deref for MoveBuf {
    type Target = [ScoredMove];

    #[inline(always)]
    fn deref(&self) -> &[ScoredMove] {
        &self.moves[..self.len]
    }
}

impl core::ops::DerefMut for MoveBuf {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [ScoredMove] {
        &mut self.moves[..self.len]
    }
}

impl MoveSink for MoveBuf {
    /// Write the MOVE only, and leave the score field holding whatever the slot held before.
    ///
    /// Every entry gets its real score from the scoring pass that follows, which walks
    /// exactly the range this generation appended and ASSIGNS rather than accumulates, so
    /// the stale value is never read. Storing a zero here is a second store per generated
    /// move that nothing consumes.
    ///
    /// # Panics
    /// Panics when the buffer is full. `MAX_MOVES` is a property of chess, not a guess, so
    /// that means the generator emitted a move twice.
    #[inline(always)]
    fn push_move(&mut self, m: Move) {
        self.moves[self.len].mv = m;
        self.len += 1;
    }
}

/// Quiet moves scoring at or below this are deferred behind the bad captures.
const GOOD_QUIET_THRESHOLD: i32 = -14000;

/// The quiet sort's limit, for a list generated at `depth`.
///
/// Saturating because `depth` is a caller's value and the product is not otherwise bounded.
fn quiet_sort_limit(depth: i32) -> i32 {
    -3560i32.saturating_mul(depth)
}

/// True when the quiet sort's limit is at or below `threshold`, so the sorted prefix runs
/// out before the threshold does and a walk may stop at the first move that fails it.
///
/// See [`MovePicker::select_while_above`], which this decides.
fn sorted_past(depth: i32, threshold: i32) -> bool {
    quiet_sort_limit(depth) <= threshold
}

/// Sort every entry descending — [`partial_insertion_sort`] with a limit nothing can fail.
///
/// The capture and evasion stages pass `i32::MIN`, which no `i32` score is below, so the
/// limit test is always taken and `sorted_end` tracks `p` exactly. That collapses the moving
/// boundary: the store that hands `p`'s slot to the displaced entry becomes a
/// self-assignment, and the limit compare becomes dead. The permutation is unchanged by
/// construction, which is what lets the two share a signature.
fn full_insertion_sort(moves: &mut [ScoredMove]) {
    let mut p = 1usize;
    while p < moves.len() {
        let tmp = moves[p];
        let mut q = p;
        while q != 0 && moves[q - 1].score < tmp.score {
            moves[q] = moves[q - 1];
            q -= 1;
        }
        moves[q] = tmp;
        p += 1;
    }
}

/// Sort `[begin, end)` descending, but only the entries scoring at least `limit`.
///
/// The entries below `limit` end up in an unspecified order, which upstream relies on:
/// they are only ever reached after everything above the limit has been tried, and by then
/// the node has usually cut off. Written as upstream's insertion sort over a moving
/// boundary rather than as a stable sort, because the two produce different permutations
/// for equal scores.
fn partial_insertion_sort(moves: &mut [ScoredMove], limit: i32) {
    if moves.is_empty() {
        return;
    }
    // `sorted_end` is the index of the last entry in the sorted prefix.
    let mut sorted_end = 0usize;
    let mut p = 1usize;
    while p < moves.len() {
        if moves[p].score >= limit {
            let tmp = moves[p];
            sorted_end += 1;
            // The entry displaced from the front of the unsorted region takes p's slot.
            moves[p] = moves[sorted_end];
            let mut q = sorted_end;
            while q != 0 && moves[q - 1].score < tmp.score {
                moves[q] = moves[q - 1];
                q -= 1;
            }
            moves[q] = tmp;
        }
        p += 1;
    }
}

/// Yields the moves of one node in search order.
pub struct MovePicker {
    continuations: ContKeys,
    tt_move: Move,
    /// The static-exchange threshold `ProbCut` requires. Unused by the other constructors.
    threshold: Value,
    stage: Stage,
    depth: i32,
    ply: Ply,
    skip_quiets: bool,

    /// Cursor into `moves`.
    cur: usize,
    /// One past the last move the current stage may yield.
    end_cur: usize,
    /// One past the last capture demoted to the bad-capture region, which grows from the
    /// front of the buffer as `GoodCapture` rejects entries.
    end_bad_captures: usize,
    /// One past the last generated capture.
    end_captures: usize,
    /// One past everything generated.
    end_generated: usize,

    pawn_row: PawnRow,
}

impl core::fmt::Debug for MovePicker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MovePicker")
            .field("stage", &self.stage)
            .field("tt_move", &self.tt_move)
            .finish_non_exhaustive()
    }
}

impl MovePicker {
    /// A picker for a main-search or quiescence node.
    ///
    /// `depth > 0` selects the main-search sequence, `depth <= 0` the quiescence one —
    /// upstream distinguishes them by the same test rather than by a separate constructor.
    #[must_use]
    pub fn new(
        pos: &Position,
        continuations: ContKeys,
        tt_move: Move,
        depth: i32,
        ply: Ply,
    ) -> MovePicker {
        let usable = tt_move.is_ok() && pos.pseudo_legal(tt_move);
        let stage = if pos.in_check() {
            if usable { Stage::EvasionTt } else { Stage::EvasionInit }
        } else if depth > 0 {
            if usable { Stage::MainTt } else { Stage::CaptureInit }
        } else if usable {
            Stage::QSearchTt
        } else {
            Stage::QCaptureInit
        };
        MovePicker {
            continuations,
            tt_move: if usable { tt_move } else { Move::NONE },
            threshold: VALUE_ZERO,
            stage,
            depth,
            ply,
            skip_quiets: false,
            cur: 0,
            end_cur: 0,
            end_bad_captures: 0,
            end_captures: 0,
            end_generated: 0,
            pawn_row: PawnHistory::row(pos.st().pawn_key),
        }
    }

    /// A picker for `ProbCut`: captures whose static exchange beats `threshold`.
    #[must_use]
    pub fn new_probcut(pos: &Position, tt_move: Move, threshold: Value) -> MovePicker {
        debug_assert!(!pos.in_check(), "ProbCut is never entered in check");
        let usable = tt_move.is_ok() && pos.is_capture_stage(tt_move) && pos.pseudo_legal(tt_move);
        MovePicker {
            continuations: [ContKey::UNREAD; 6],
            tt_move: if usable { tt_move } else { Move::NONE },
            threshold,
            stage: if usable { Stage::ProbCutTt } else { Stage::ProbCutInit },
            depth: 0,
            ply: Ply::ROOT,
            skip_quiets: false,
            cur: 0,
            end_cur: 0,
            end_bad_captures: 0,
            end_captures: 0,
            end_generated: 0,
            pawn_row: PawnHistory::row(pos.st().pawn_key),
        }
    }

    /// Stop yielding quiet moves from the next call on.
    pub fn skip_quiet_moves(&mut self) {
        self.skip_quiets = true;
    }

    /// Generate, score and sort the captures, and enter the stage that yields them.
    ///
    /// **Outlined deliberately, and the `inline(never)` is the point of the function.** This
    /// runs ONCE per picker; [`MovePicker::next_move`] runs once per move yielded, 1,268,056
    /// times on a `bench 16 1 8`. Inlined, the generator, the scoring loop and the whole
    /// insertion sort sit inside `next_move`'s body, and every one of those calls pays for
    /// them in frame size: the prologue alone measured 30 instructions a call. Hoisting the
    /// once-per-stage work out leaves the per-move path a short walk over a cursor.
    ///
    /// The three `*Init` stages share this body because they differ only in which stage they
    /// hand over to, which is upstream's structure (`movepick.cpp`'s fallthrough) and not a
    /// merge done here.
    #[inline(never)]
    fn init_captures(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        Self::generate(pos, GenType::Captures, buf);
        Self::score_captures(pos, h, buf);
        self.cur = 0;
        self.end_bad_captures = 0;
        self.end_cur = buf.len();
        self.end_captures = buf.len();
        self.end_generated = buf.len();
        full_insertion_sort(&mut buf[..self.end_cur]);
        self.stage = match self.stage {
            Stage::CaptureInit => Stage::GoodCapture,
            Stage::ProbCutInit => Stage::ProbCut,
            _ => Stage::QCapture,
        };
    }

    /// Append, score and partially sort the quiet moves. Outlined for the reason above.
    ///
    /// The caller keeps the `skip_quiets` test and the stage handover: this must not run at
    /// all when quiets are skipped, and a call that returned immediately would still cost the
    /// call.
    #[inline(never)]
    fn init_quiets(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        Self::generate_append(pos, GenType::Quiets, buf);
        self.score_quiets(pos, h, buf);
        self.end_cur = buf.len();
        self.end_generated = buf.len();
        let from = self.cur;
        let to = self.end_cur;
        partial_insertion_sort(&mut buf[from..to], quiet_sort_limit(self.depth));
    }

    /// Generate, score and sort the evasions. Outlined for the reason above.
    #[inline(never)]
    fn init_evasions(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        Self::generate(pos, GenType::Evasions, buf);
        self.score_evasions(pos, h, buf);
        self.cur = 0;
        self.end_cur = buf.len();
        self.end_generated = buf.len();
        full_insertion_sort(&mut buf[..self.end_cur]);
    }

    /// The next move, or [`Move::NONE`] when the sequence is exhausted.
    pub fn next_move(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) -> Move {
        loop {
            match self.stage {
                Stage::MainTt | Stage::EvasionTt | Stage::QSearchTt | Stage::ProbCutTt => {
                    self.stage = match self.stage {
                        Stage::MainTt => Stage::CaptureInit,
                        Stage::EvasionTt => Stage::EvasionInit,
                        Stage::QSearchTt => Stage::QCaptureInit,
                        _ => Stage::ProbCutInit,
                    };
                    return self.tt_move;
                }

                Stage::CaptureInit | Stage::ProbCutInit | Stage::QCaptureInit => {
                    self.init_captures(pos, h, buf);
                }

                Stage::GoodCapture => {
                    // A capture whose exchange does not hold is demoted rather than
                    // dropped: it moves to the front region and is retried after the
                    // quiet moves. The threshold scales with the move's own score, so a
                    // capture the history likes is given more benefit of the doubt.
                    while self.cur < self.end_cur {
                        let sm = buf[self.cur];
                        if sm.mv == self.tt_move {
                            self.cur += 1;
                            continue;
                        }
                        if pos.see_ge(sm.mv, Value::new(-sm.score / 18)) {
                            self.cur += 1;
                            return sm.mv;
                        }
                        buf.swap(self.end_bad_captures, self.cur);
                        self.end_bad_captures += 1;
                        self.cur += 1;
                    }
                    self.stage = Stage::QuietInit;
                }

                Stage::QuietInit => {
                    if !self.skip_quiets {
                        self.init_quiets(pos, h, buf);
                    }
                    self.stage = Stage::GoodQuiet;
                }

                Stage::GoodQuiet => {
                    if !self.skip_quiets
                        && let Some(m) = self.select_while_above(pos, buf, GOOD_QUIET_THRESHOLD)
                    {
                        return m;
                    }
                    // Rewind to the bad captures, which sit at the front of the buffer.
                    self.cur = 0;
                    self.end_cur = self.end_bad_captures;
                    self.stage = Stage::BadCapture;
                }

                Stage::BadCapture => {
                    if let Some(m) = self.select(pos, buf, |_, _| true) {
                        return m;
                    }
                    // Then the quiet moves the good-quiet stage left behind.
                    self.cur = self.end_captures;
                    self.end_cur = self.end_generated;
                    self.stage = Stage::BadQuiet;
                }

                Stage::BadQuiet => {
                    if !self.skip_quiets
                        && let Some(m) =
                            self.select(pos, buf, |sm, _| sm.score <= GOOD_QUIET_THRESHOLD)
                    {
                        return m;
                    }
                    self.stage = Stage::Done;
                }

                Stage::EvasionInit => {
                    self.init_evasions(pos, h, buf);
                    self.stage = Stage::Evasion;
                }

                Stage::Evasion | Stage::QCapture => {
                    if let Some(m) = self.select(pos, buf, |_, _| true) {
                        return m;
                    }
                    self.stage = Stage::Done;
                }

                Stage::ProbCut => {
                    let threshold = self.threshold;
                    if let Some(m) = self.select(pos, buf, |sm, p| p.see_ge(sm.mv, threshold)) {
                        return m;
                    }
                    self.stage = Stage::Done;
                }

                Stage::Done => return Move::NONE,
            }
        }
    }

    /// The next move in `[cur, end_cur)` scoring above `threshold`, stopping at the first
    /// move that is not -- where [`Self::select`] would walk the rest of the list to find
    /// nothing.
    ///
    /// [`partial_insertion_sort`] leaves the list in two pieces: a prefix that descends, and
    /// a tail every member of which scores below the sort's own limit. So once the walk has
    /// seen one move at or below the threshold, the rest of the prefix is at or below it BY
    /// THE ORDERING and the tail is below it BY THE LIMIT -- and there is nothing left to
    /// find.
    ///
    /// **The tail half of that argument holds only while the limit is at or below the
    /// threshold.** The limit is `-3560 * depth` and the threshold is
    /// [`GOOD_QUIET_THRESHOLD`], so the limit first reaches it at depth 4; below that a tail
    /// move can still outscore the threshold and the walk has to run to the end.
    /// [`Self::good_quiets_are_sorted_past_the_threshold`] is the test that decides which
    /// form the stage takes, and it is a test of the CONSTANTS rather than of a position.
    fn select_while_above(
        &mut self,
        pos: &Position,
        buf: &MoveBuf,
        threshold: i32,
    ) -> Option<Move> {
        if !sorted_past(self.depth, threshold) {
            return self.select(pos, buf, |sm, _| sm.score > threshold);
        }
        while self.cur < self.end_cur {
            let sm = buf[self.cur];
            if sm.score <= threshold {
                // Everything from here down is at or below the threshold, so the stage is
                // finished -- but `cur` stays where it is: the BAD_QUIET stage resumes from
                // `end_captures` and wants these moves.
                return None;
            }
            self.cur += 1;
            if sm.mv != self.tt_move {
                return Some(sm.mv);
            }
        }
        None
    }

    /// The next move in `[cur, end_cur)` that is not the transposition move and satisfies
    /// `filter`. Advances `cur` past everything it rejects.
    fn select<F>(&mut self, pos: &Position, buf: &MoveBuf, filter: F) -> Option<Move>
    where
        F: Fn(&ScoredMove, &Position) -> bool,
    {
        while self.cur < self.end_cur {
            let sm = buf[self.cur];
            self.cur += 1;
            if sm.mv != self.tt_move && filter(&sm, pos) {
                return Some(sm.mv);
            }
        }
        None
    }

    /// Generate into a fresh buffer.
    ///
    /// The transposition move is NOT filtered out here — upstream leaves it in the list and
    /// skips it in `select`, and the difference matters: the entry still occupies a slot,
    /// so it still shifts what the partial sort's threshold admits.
    fn generate(pos: &Position, gt: GenType, buf: &mut MoveBuf) {
        buf.clear();
        Self::generate_append(pos, gt, buf);
    }

    /// Generate onto the end of the existing buffer.
    fn generate_append(pos: &Position, gt: GenType, buf: &mut MoveBuf) {
        // Straight into the picker's own buffer. Going through a `MoveList` first cost a
        // 512-byte zero-fill and a second pass over every move, per generation.
        generate_into(pos, gt, buf);
    }

    fn score_captures(pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        for sm in buf.iter_mut() {
            let to = sm.mv.to();
            let pc = pos.moved_piece(sm.mv);
            let captured = pos.piece_on(to);
            // Most-valuable-victim first, refined by how well this capture has worked
            // before. The victim term dominates, so the history only breaks ties.
            //
            // `PieceValue[capturedPiece]` is indexed by the PIECE, not the piece type, and
            // is zero for an empty square — which is what an en-passant capture's
            // destination holds. Upstream scores it as a zero-value victim, and so does
            // this: correcting it to a pawn would be an improvement, and improvements move
            // the node count.
            sm.score =
                h.captures.get(pc, to, captured.piece_type()) + 7 * piece_value(captured).get();
        }
    }

    fn score_quiets(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        let us = pos.side_to_move();
        let them = !us;

        // Squares attacked by a piece cheaper than the one being moved. Moving ONTO one is
        // a threat against the mover; moving OFF one escapes a threat. The table is built
        // once per node and indexed by the mover's type.
        let pawn_att = pos.attacks_by(them, PieceType::Pawn);
        let knight_att = pos.attacks_by(them, PieceType::Knight);
        let bishop_att = pos.attacks_by(them, PieceType::Bishop);
        let rook_att = pos.attacks_by(them, PieceType::Rook);
        let mut threat_by_lesser = [crate::board::bitboard::Bitboard::EMPTY; 7];
        threat_by_lesser[PieceType::Knight.index()] = pawn_att;
        threat_by_lesser[PieceType::Bishop.index()] = pawn_att;
        threat_by_lesser[PieceType::Rook.index()] =
            knight_att | bishop_att | threat_by_lesser[PieceType::Knight.index()];
        threat_by_lesser[PieceType::Queen.index()] =
            rook_att | threat_by_lesser[PieceType::Rook.index()];

        // Resolve every row this list reads ONCE. The colour, the pawn key, the ply and the
        // parent moves are fixed for the whole list, so re-deriving them per move is work
        // upstream never does: its `ss->continuationHistory` is a pointer settled at the
        // node. Planes one, two, three, four and SIX — five is deliberately absent upstream.
        let main_row = h.main.row(us);
        // Every one of these six is read at the same `[pc][to]`, so each is held as the one
        // contiguous run it is and indexed once. See `history::piece_to_index`.
        let pawn_plane = super::history::flatten_plane(h.pawn.plane(self.pawn_row));
        let cont = [0usize, 1, 2, 3, 5]
            .map(|s| super::history::flatten_plane(h.continuation.plane(self.continuations[s])));
        let low_ply = (self.ply.index() < super::history::LOW_PLY_HISTORY_SIZE)
            .then(|| h.low_ply.row(self.ply));

        // The quiet list starts where the captures ended; only the new entries are scored.
        let start = self.end_captures;
        for slot in &mut buf[start..] {
            let mv = slot.mv;
            let from = mv.from();
            let to = mv.to();
            let pc = pos.moved_piece(mv);
            let pt = pc.piece_type();

            let hi = super::history::piece_to_index(pc, to);
            let mut score = 2 * i32::from(main_row[mv.raw() as usize]);
            score += 2 * i32::from(pawn_plane[hi]);
            for plane in cont {
                score += i32::from(plane[hi]);
            }

            // A quiet move that gives check is worth trying early: it is forcing, so the
            // subtree under it is small. Only when it does not simply hang the piece.
            if pos.check_squares(pt).contains(to) && pos.see_ge(mv, Value::new(-75)) {
                score += 16384;
            }

            let lesser = threat_by_lesser[pt.index()];
            let v = 20 * (i32::from(lesser.contains(from)) - i32::from(lesser.contains(to)));
            score += piece_value(Piece::new(us, pt)).get() * v;

            if let Some(row) = low_ply {
                score += 8 * i32::from(row[mv.raw() as usize]) / (1 + self.ply.get());
            }

            slot.score = score;
        }
    }

    fn score_evasions(&mut self, pos: &Position, h: &Histories, buf: &mut MoveBuf) {
        let us = pos.side_to_move();
        // Both rows are fixed for the list; see `score_quiets`.
        let main_row = h.main.row(us);
        let cont0 = h.continuation.plane(self.continuations[0]);
        for sm in buf.iter_mut() {
            let to = sm.mv.to();
            let pc = pos.moved_piece(sm.mv);
            if pos.is_capture_stage(sm.mv) {
                // Captures come first among evasions, ordered by what they win. The large
                // offset keeps every capture above every quiet evasion.
                sm.score = piece_value(pos.piece_on(to)).get() + (1 << 28);
            } else {
                sm.score = i32::from(main_row[sm.mv.raw() as usize])
                    + i32::from(cont0[pc.index()][to.index()]);
            }
        }
    }
}

/// The square a move ends on, for the continuation tables. Castling is keyed by the king's
/// destination, not the rook's square, so the plane matches the position that follows.
#[inline]
#[must_use]
pub fn continuation_to(m: Move) -> Square {
    if m.move_type() == MoveType::Castling {
        let king_side = m.to() > m.from();
        Square::make(if king_side { File::G } else { File::C }, m.from().rank())
    } else {
        m.to()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::board::movegen::generate_legal;
    use crate::board::position::START_FEN;
    use crate::board::types::Rank;

    /// The depth at which the quiet sort's limit falls to the good-quiet threshold, which is
    /// what decides whether the stage may stop at the first move below the bar.
    ///
    /// A test of the CONSTANTS, not of a position: `select_while_above`'s tail argument is
    /// sound exactly while the sort's limit is at or below the threshold, and both numbers
    /// are tuned values a sync can move. If either moves, this says which way.
    #[test]
    fn good_quiets_are_sorted_past_the_threshold() {
        for depth in 1..=3 {
            assert!(
                !sorted_past(depth, GOOD_QUIET_THRESHOLD),
                "at depth {depth} the limit is {} and a TAIL move can still outscore {}",
                quiet_sort_limit(depth),
                GOOD_QUIET_THRESHOLD
            );
        }
        for depth in 4..=64 {
            assert!(
                sorted_past(depth, GOOD_QUIET_THRESHOLD),
                "at depth {depth} the limit is {}, so nothing below the threshold can follow \
                 something above it",
                quiet_sort_limit(depth)
            );
        }
        // The limit saturates rather than wrapping, so a caller's depth cannot invert the
        // test by overflowing the product.
        assert!(sorted_past(i32::MAX, GOOD_QUIET_THRESHOLD));
    }

    /// The stage yields exactly the moves `select` would, on a list the sort has arranged.
    ///
    /// The two forms have to agree move for move: the early stop is an optimisation of the
    /// walk and not a change to what the stage picks.
    #[test]
    fn the_early_stop_yields_what_the_full_walk_yields() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let h = Histories::default();
        for depth in [1, 3, 4, 8, 20] {
            let mut full = Vec::new();
            let mut early = Vec::new();
            for (out, stop_early) in [(&mut full, false), (&mut early, true)] {
                let mut buf = MoveBuf::new();
                let mut mp =
                    MovePicker::new(&pos, [ContKey::UNREAD; 6], Move::NONE, depth, Ply::ROOT);
                mp.init_quiets(&pos, &h, &mut buf);
                loop {
                    let m = if stop_early {
                        mp.select_while_above(&pos, &buf, GOOD_QUIET_THRESHOLD)
                    } else {
                        mp.select(&pos, &buf, |sm, _| sm.score > GOOD_QUIET_THRESHOLD)
                    };
                    match m {
                        Some(m) => out.push(m),
                        None => break,
                    }
                }
            }
            assert_eq!(full, early, "the two walks disagree at depth {depth}");
        }
    }

    fn collect(pos: &Position, h: &Histories, tt: Move) -> Vec<Move> {
        let mut mp = MovePicker::new(pos, [ContKey::UNREAD; 6], tt, 4, Ply::ROOT);
        let mut buf = MoveBuf::new();
        let mut out = Vec::new();
        loop {
            let m = mp.next_move(pos, h, &mut buf);
            if m.is_none() {
                break;
            }
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
        let bogus = Move::new(
            Square::make(File::new(4), Rank::new(4)),
            Square::make(File::new(4), Rank::new(5)),
        );
        let picked = collect(&pos, &h, bogus);
        assert!(!picked.contains(&bogus));
        assert_eq!(picked.len(), generate_legal(&pos).len());
    }

    /// In qsearch outside check, only captures and promotions may be yielded.
    #[test]
    fn qsearch_yields_only_forcing_moves() {
        let h = Histories::default();
        let pos = Position::from_fen("4k3/8/8/3q4/4P3/8/8/R3K3 w - - 0 1", false).expect("valid");
        let mut mp = MovePicker::new(&pos, [ContKey::UNREAD; 6], Move::NONE, 0, Ply::ROOT);
        let mut buf = MoveBuf::new();
        let mut any = false;
        loop {
            let m = mp.next_move(&pos, &h, &mut buf);
            if m.is_none() {
                break;
            }
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
        let mut mp = MovePicker::new(&pos, [ContKey::UNREAD; 6], Move::NONE, 0, Ply::ROOT);
        let mut buf = MoveBuf::new();
        let mut out = Vec::new();
        loop {
            let m = mp.next_move(&pos, &h, &mut buf);
            if m.is_none() {
                break;
            }
            out.push(m);
        }
        out.retain(|&m| pos.legal(m));
        out.sort_unstable();
        let mut legal: Vec<Move> = generate_legal(&pos).iter().copied().collect();
        legal.sort_unstable();
        assert_eq!(out, legal);
    }

    /// `skip_quiet_moves` must take effect from the next call, not from construction.
    #[test]
    fn skip_quiets_stops_the_quiet_stage_mid_node() {
        let h = Histories::default();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut mp = MovePicker::new(&pos, [ContKey::UNREAD; 6], Move::NONE, 4, Ply::ROOT);
        let mut buf = MoveBuf::new();
        mp.skip_quiet_moves();
        let mut seen = 0;
        loop {
            let m = mp.next_move(&pos, &h, &mut buf);
            if m.is_none() {
                break;
            }
            assert!(pos.is_capture_stage(m), "{m:?} is quiet but quiets were skipped");
            seen += 1;
        }
        // The start position has no captures, so skipping quiets leaves nothing.
        assert_eq!(seen, 0);
    }

    /// `ProbCut` yields only captures that beat its threshold.
    #[test]
    fn probcut_yields_only_captures_above_the_threshold() {
        let h = Histories::default();
        let pos = Position::from_fen("4k3/8/8/3q4/4P3/8/8/R3K3 w - - 0 1", false).expect("valid");
        let mut mp = MovePicker::new_probcut(&pos, Move::NONE, VALUE_ZERO);
        let mut buf = MoveBuf::new();
        loop {
            let m = mp.next_move(&pos, &h, &mut buf);
            if m.is_none() {
                break;
            }
            assert!(pos.is_capture_stage(m));
            assert!(pos.see_ge(m, VALUE_ZERO));
        }
    }

    /// The partial sort orders everything at or above the limit and leaves the rest alone.
    /// A stable sort of the whole list would be a DIFFERENT permutation, and the node count
    /// would follow it.
    #[test]
    fn the_partial_sort_orders_only_above_the_limit() {
        let mk = |score| ScoredMove {
            mv: Move::new(
                Square::make(File::new(0), Rank::new(0)),
                Square::make(File::new(0), Rank::new(1)),
            ),
            score,
        };
        let mut v = vec![mk(10), mk(-100), mk(50), mk(-200), mk(30)];
        partial_insertion_sort(&mut v, 0);
        // The three entries at or above zero come first, in descending order.
        assert_eq!([v[0].score, v[1].score, v[2].score], [50, 30, 10]);
        // The two below it are still present, order unspecified.
        let mut tail = [v[3].score, v[4].score];
        tail.sort_unstable();
        assert_eq!(tail, [-200, -100]);
    }

    /// Equal scores must keep their generation order, which is what makes the sort's
    /// permutation reproducible.
    #[test]
    fn equal_scores_keep_generation_order() {
        let a = Move::new(
            Square::make(File::new(0), Rank::new(0)),
            Square::make(File::new(0), Rank::new(1)),
        );
        let b = Move::new(
            Square::make(File::new(1), Rank::new(0)),
            Square::make(File::new(1), Rank::new(1)),
        );
        let c = Move::new(
            Square::make(File::new(2), Rank::new(0)),
            Square::make(File::new(2), Rank::new(1)),
        );
        let mut v = vec![
            ScoredMove { mv: a, score: 5 },
            ScoredMove { mv: b, score: 5 },
            ScoredMove { mv: c, score: 5 },
        ];
        partial_insertion_sort(&mut v, 0);
        assert_eq!([v[0].mv, v[1].mv, v[2].mv], [a, b, c]);
    }

    #[test]
    fn castling_is_keyed_by_the_kings_destination() {
        let pos = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", false).expect("valid");
        let castle = generate_legal(&pos)
            .iter()
            .copied()
            .find(|m| m.move_type() == MoveType::Castling && m.to() > m.from())
            .expect("O-O is legal");
        assert_eq!(continuation_to(castle), Square::make(File::new(6), Rank::new(0)));
    }
}
