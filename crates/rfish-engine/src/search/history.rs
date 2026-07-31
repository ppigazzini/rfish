//! The history tables the move picker orders by and the search reduces by.
//!
//! Every table here is a *gravity* table: an update moves the stored value toward the
//! bonus by an amount proportional to how far it still is from the clamp, so a value
//! saturates smoothly instead of pinning at the bound after a handful of updates. That
//! single rule is upstream's `StatsEntry::operator<<`, and it is what the whole ordering
//! rests on.
//!
//! # Shapes are upstream's, not convenient ones
//!
//! The butterfly and low-ply tables are indexed by the RAW 16-bit move, not by a packed
//! from/to pair. Those two differ: the raw move carries the move type and the promotion
//! piece in its high bits, so a promotion to a queen and a promotion to a knight are
//! separate entries upstream and would collide under a from/to index. The collision does
//! not crash anything — it changes the ordering, and therefore the node count.
//!
//! The tables are large and per-worker; they are allocated on the heap as boxed arrays so
//! a `SearchWorker` stays movable and a thread does not need a megabyte-deep stack.
//!
//! Golden: `Stockfish/src/history.h`, `Stockfish/src/search.cpp`.

use crate::board::types::{COLOR_NB, Color, PIECE_NB, Piece, PieceType, SQUARE_NB, Square};

/// A `Box<[T; N]>` filled with `fill`, built on the heap rather than through a stack
/// temporary.
///
/// `Box::new([[0; N]; M])` materialises the whole array in a stack frame first. For the
/// tables here that is tens of megabytes, and it overflows the thread stack in a debug
/// build. A `Vec` of the right length converted to a boxed array never touches the stack —
/// only the single `fill` element does.
fn boxed<T: Copy, const N: usize>(fill: T) -> Box<[T; N]> {
    match vec![fill; N].into_boxed_slice().try_into() {
        Ok(b) => b,
        // The vec was built with exactly N elements, so the conversion cannot fail. Written
        // as a match rather than `.expect` because the error type is the boxed slice
        // itself, which need not be `Debug`.
        Err(_) => unreachable!("the vec was built with exactly N items"),
    }
}

/// Rows in a table indexed by the raw 16-bit move.
pub const MOVE_HISTORY_SIZE: usize = 1 << 16;
/// Plies near the root that get their own ordering table.
pub const LOW_PLY_HISTORY_SIZE: usize = 5;
/// Rows in the pawn-structure history.
pub const PAWN_HISTORY_SIZE: usize = 8192;
/// Rows in the unified correction history.
pub const CORRECTION_HISTORY_SIZE: usize = 1 << 16;

/// Clamp of the butterfly and low-ply tables.
pub const MAIN_HISTORY_LIMIT: i32 = 7183;
/// Clamp of the capture table.
pub const CAPTURE_HISTORY_LIMIT: i32 = 10692;
/// Clamp of a continuation plane.
pub const CONTINUATION_LIMIT: i32 = 30000;
/// Clamp of the pawn-structure table.
pub const PAWN_HISTORY_LIMIT: i32 = 8192;
/// Clamp of every correction table.
pub const CORRECTION_LIMIT: i32 = 1024;
/// Clamp of the single transposition-move-quality counter.
pub const TT_MOVE_HISTORY_LIMIT: i32 = 8192;

/// Move a stored value toward `bonus`, by upstream's gravity rule.
///
/// The value approaches `limit` asymptotically: the closer it already is, the less an
/// update moves it. A table that simply added and clamped would lose the ordering
/// information between two moves that had both saturated.
#[inline(always)]
fn apply_gravity(entry: &mut i16, bonus: i32, limit: i32) {
    let bonus = bonus.clamp(-limit, limit);
    let v = i32::from(*entry);
    *entry = (v + bonus - v * bonus.abs() / limit) as i16;
}

/// Quiet-move history, indexed by side to move and the raw move.
///
/// "Butterfly" is the classic name: the index ignores which piece moved, so two different
/// pieces travelling the same path share a counter.
///
/// Stored FLAT rather than as nested arrays: a row is 128 KiB, and any construction that
/// names the row type puts one of those on the stack on its way to the heap.
#[derive(Debug)]
pub struct ButterflyHistory {
    table: Box<[i16]>,
}

impl Default for ButterflyHistory {
    fn default() -> ButterflyHistory {
        ButterflyHistory { table: vec![0; COLOR_NB * MOVE_HISTORY_SIZE].into_boxed_slice() }
    }
}

impl ButterflyHistory {
    /// The stored score for `c` playing the raw move `mv`.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, c: Color, mv: u16) -> i32 {
        i32::from(self.table[c.index() * MOVE_HISTORY_SIZE + mv as usize])
    }

    /// Move the stored score toward `bonus`.
    #[inline(always)]
    pub fn update(&mut self, c: Color, mv: u16, bonus: i32) {
        apply_gravity(
            &mut self.table[c.index() * MOVE_HISTORY_SIZE + mv as usize],
            bonus,
            MAIN_HISTORY_LIMIT,
        );
    }

    /// Reset every entry to `v`.
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().for_each(|x| *x = v);
    }

    /// Scale every entry by `num / den`.
    ///
    /// Upstream decays the butterfly table between moves rather than clearing it: the
    /// previous move's ordering is still evidence, just weaker than the current one's.
    pub fn decay(&mut self, num: i32, den: i32) {
        self.table.iter_mut().for_each(|x| *x = (i32::from(*x) * num / den) as i16);
    }
}

/// Quiet-move history for the plies nearest the root, where ordering matters most.
#[derive(Debug)]
pub struct LowPlyHistory {
    table: Box<[i16]>,
}

impl Default for LowPlyHistory {
    fn default() -> LowPlyHistory {
        LowPlyHistory {
            table: vec![0; LOW_PLY_HISTORY_SIZE * MOVE_HISTORY_SIZE].into_boxed_slice(),
        }
    }
}

impl LowPlyHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, ply: usize, mv: u16) -> i32 {
        i32::from(self.table[ply * MOVE_HISTORY_SIZE + mv as usize])
    }

    #[inline(always)]
    pub fn update(&mut self, ply: usize, mv: u16, bonus: i32) {
        apply_gravity(
            &mut self.table[ply * MOVE_HISTORY_SIZE + mv as usize],
            bonus,
            MAIN_HISTORY_LIMIT,
        );
    }

    /// Reset every entry to `v`. Upstream refills this at the start of every iteration,
    /// not on `ucinewgame`.
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().for_each(|x| *x = v);
    }
}

/// Capture history, indexed by the moving piece, its destination, and what it took.
#[derive(Debug)]
pub struct CaptureHistory {
    table: Box<[[[i16; PIECE_TYPE_SLOTS]; SQUARE_NB]; PIECE_NB]>,
}

/// Slots for the captured piece type. Upstream's `PIECE_TYPE_NB` is 8; the values used run
/// 0..=6, so seven would do, but the eighth keeps the index arithmetic identical.
const PIECE_TYPE_SLOTS: usize = 8;

impl Default for CaptureHistory {
    fn default() -> CaptureHistory {
        CaptureHistory { table: boxed([[0; PIECE_TYPE_SLOTS]; SQUARE_NB]) }
    }
}

impl CaptureHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, pc: Piece, to: Square, captured: PieceType) -> i32 {
        i32::from(self.table[pc.index()][to.index()][captured.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, pc: Piece, to: Square, captured: PieceType, bonus: i32) {
        apply_gravity(
            &mut self.table[pc.index()][to.index()][captured.index()],
            bonus,
            CAPTURE_HISTORY_LIMIT,
        );
    }

    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().flatten().for_each(|x| *x = v);
    }
}

/// One (piece, destination) plane: the follow-up scores for moves played after a given
/// parent move.
#[derive(Clone, Debug)]
pub struct PieceToHistory {
    table: [[i16; SQUARE_NB]; PIECE_NB],
}

impl Default for PieceToHistory {
    fn default() -> PieceToHistory {
        PieceToHistory { table: [[0; SQUARE_NB]; PIECE_NB] }
    }
}

impl PieceToHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[pc.index()][to.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, pc: Piece, to: Square, bonus: i32) {
        apply_gravity(&mut self.table[pc.index()][to.index()], bonus, CONTINUATION_LIMIT);
    }

    /// Update at the correction clamp rather than the continuation one.
    ///
    /// The same plane shape serves two tables with different limits, and the limit is part
    /// of the gravity arithmetic — using the wrong one changes every stored value.
    #[inline(always)]
    pub fn update_correction(&mut self, pc: Piece, to: Square, bonus: i32) {
        apply_gravity(&mut self.table[pc.index()][to.index()], bonus, CORRECTION_LIMIT);
    }

    /// Reset every entry to `v`.
    ///
    /// Upstream fills the continuation planes with a negative sentinel rather than zero,
    /// so an untouched follow-up sorts below a move that has merely never worked — a plane
    /// of zeros would make "unknown" look as good as "neutral".
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().for_each(|x| *x = v);
    }
}

/// The continuation tables, indexed by whether the parent was in check and whether it was
/// a capture, then by the parent move's (piece, destination).
#[derive(Debug)]
pub struct ContinuationHistory {
    table: Box<[[[i16; SQUARE_NB]; PIECE_NB]; 2 * 2 * PIECE_NB * SQUARE_NB]>,
}

/// The flat index of a continuation plane.
#[inline(always)]
#[must_use]
pub fn cont_plane_index(in_check: bool, capture: bool, pc: Piece, to: Square) -> usize {
    ((usize::from(in_check) * 2 + usize::from(capture)) * PIECE_NB + pc.index()) * SQUARE_NB
        + to.index()
}

impl Default for ContinuationHistory {
    fn default() -> ContinuationHistory {
        ContinuationHistory { table: boxed([[-586; SQUARE_NB]; PIECE_NB]) }
    }
}

impl ContinuationHistory {
    /// The score a follow-up `(pc, to)` has after the parent plane `idx`.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, idx: usize, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[idx][pc.index()][to.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, idx: usize, pc: Piece, to: Square, bonus: i32) {
        apply_gravity(&mut self.table[idx][pc.index()][to.index()], bonus, CONTINUATION_LIMIT);
    }

    /// Reset every plane to upstream's negative sentinel.
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().flatten().for_each(|x| *x = v);
    }
}

/// The continuation CORRECTION tables: how far the static evaluation has been off after a
/// given parent move, keyed the same way but clamped at the correction limit.
#[derive(Debug)]
pub struct ContinuationCorrectionHistory {
    table: Box<[[[i16; SQUARE_NB]; PIECE_NB]; PIECE_NB * SQUARE_NB]>,
}

/// The flat index of a continuation-correction plane. Unlike the continuation history this
/// one is not split by check or capture.
#[inline(always)]
#[must_use]
pub fn corr_plane_index(pc: Piece, to: Square) -> usize {
    pc.index() * SQUARE_NB + to.index()
}

impl Default for ContinuationCorrectionHistory {
    fn default() -> ContinuationCorrectionHistory {
        ContinuationCorrectionHistory { table: boxed([[5; SQUARE_NB]; PIECE_NB]) }
    }
}

impl ContinuationCorrectionHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, idx: usize, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[idx][pc.index()][to.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, idx: usize, pc: Piece, to: Square, bonus: i32) {
        apply_gravity(&mut self.table[idx][pc.index()][to.index()], bonus, CORRECTION_LIMIT);
    }

    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().flatten().for_each(|x| *x = v);
    }
}

/// Pawn-structure history: quiet-move scores conditioned on the pawn skeleton.
#[derive(Debug)]
pub struct PawnHistory {
    table: Box<[[[i16; SQUARE_NB]; PIECE_NB]; PAWN_HISTORY_SIZE]>,
}

impl Default for PawnHistory {
    fn default() -> PawnHistory {
        PawnHistory { table: boxed([[0; SQUARE_NB]; PIECE_NB]) }
    }
}

impl PawnHistory {
    /// The row a pawn key selects.
    #[inline(always)]
    #[must_use]
    pub fn row(pawn_key: u64) -> usize {
        (pawn_key as usize) & (PAWN_HISTORY_SIZE - 1)
    }

    #[inline(always)]
    #[must_use]
    pub fn get(&self, row: usize, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[row][pc.index()][to.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, row: usize, pc: Piece, to: Square, bonus: i32) {
        apply_gravity(&mut self.table[row][pc.index()][to.index()], bonus, PAWN_HISTORY_LIMIT);
    }

    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().flatten().for_each(|x| *x = v);
    }
}

/// The four correction counters one key selects, for one side to move.
///
/// Upstream stores them as one bundle in a single table rather than as four tables, so a
/// pawn key and a minor-piece key that happen to collide land on the SAME bundle and
/// interfere. Splitting them into four tables would remove that interference — and change
/// the numbers. The bundle is the port.
#[derive(Clone, Copy, Debug)]
pub struct CorrectionBundle {
    pub pawn: i16,
    pub minor: i16,
    pub non_pawn_white: i16,
    pub non_pawn_black: i16,
}

impl CorrectionBundle {
    const fn filled(v: i16) -> CorrectionBundle {
        CorrectionBundle { pawn: v, minor: v, non_pawn_white: v, non_pawn_black: v }
    }
}

/// The unified correction history: one table, four counters per (key, colour).
#[derive(Debug)]
pub struct CorrectionHistory {
    table: Box<[[CorrectionBundle; COLOR_NB]; CORRECTION_HISTORY_SIZE]>,
}

impl Default for CorrectionHistory {
    fn default() -> CorrectionHistory {
        CorrectionHistory { table: boxed([CorrectionBundle::filled(-5); COLOR_NB]) }
    }
}

impl CorrectionHistory {
    /// The row a key selects.
    #[inline(always)]
    #[must_use]
    pub fn row(key: u64) -> usize {
        (key as usize) & (CORRECTION_HISTORY_SIZE - 1)
    }

    #[inline(always)]
    #[must_use]
    pub fn entry(&self, key: u64, c: Color) -> &CorrectionBundle {
        &self.table[Self::row(key)][c.index()]
    }

    #[inline(always)]
    pub fn entry_mut(&mut self, key: u64, c: Color) -> &mut CorrectionBundle {
        &mut self.table[Self::row(key)][c.index()]
    }

    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().flatten().for_each(|b| *b = CorrectionBundle::filled(v));
    }
}

/// Move one field of a correction bundle toward `bonus`.
#[inline(always)]
pub fn update_correction_entry(entry: &mut i16, bonus: i32) {
    apply_gravity(entry, bonus, CORRECTION_LIMIT);
}

/// Every history table one worker owns.
///
/// Boxed as a unit so a worker can be moved between threads without copying tens of
/// megabytes, and reset as a unit so `ucinewgame` cannot forget one of them.
#[derive(Debug, Default)]
pub struct Histories {
    pub main: ButterflyHistory,
    pub low_ply: LowPlyHistory,
    pub captures: CaptureHistory,
    pub continuation: ContinuationHistory,
    pub continuation_correction: ContinuationCorrectionHistory,
    pub pawn: PawnHistory,
    pub correction: CorrectionHistory,
    /// How often the transposition move turned out to be the best one. A single counter,
    /// not a table: it measures the table's move quality overall, not any one move's.
    pub tt_move: i16,
}

impl Histories {
    /// Reset to upstream's starting values. Called on `ucinewgame`, never mid-search.
    ///
    /// The values are not zero and they are not uniform. Each one is where upstream starts
    /// that table, and a table that started at zero instead would order differently from
    /// the first node of the first search.
    pub fn clear(&mut self) {
        self.main.fill(-5);
        self.captures.fill(-742);
        self.correction.fill(-5);
        self.pawn.fill(-1338);
        self.tt_move = 0;
        self.continuation_correction.fill(5);
        self.continuation.fill(-586);
    }

    /// Move the transposition-move-quality counter toward `bonus`.
    #[inline]
    pub fn update_tt_move(&mut self, bonus: i32) {
        apply_gravity(&mut self.tt_move, bonus, TT_MOVE_HISTORY_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gravity_saturates_without_pinning() {
        let mut v: i16 = 0;
        // Repeated maximum bonuses approach the limit but never exceed it.
        for _ in 0..1000 {
            apply_gravity(&mut v, MAIN_HISTORY_LIMIT, MAIN_HISTORY_LIMIT);
        }
        assert!(i32::from(v) <= MAIN_HISTORY_LIMIT);
        assert!(i32::from(v) > MAIN_HISTORY_LIMIT - 2);

        // And symmetric on the way down.
        for _ in 0..1000 {
            apply_gravity(&mut v, -MAIN_HISTORY_LIMIT, MAIN_HISTORY_LIMIT);
        }
        assert!(i32::from(v) >= -MAIN_HISTORY_LIMIT);
        assert!(i32::from(v) < -MAIN_HISTORY_LIMIT + 2);
    }

    #[test]
    fn a_small_bonus_moves_a_saturated_entry_less_than_a_fresh_one() {
        // The property gravity exists for: ordering information survives saturation.
        let mut fresh: i16 = 0;
        let mut saturated: i16 = (MAIN_HISTORY_LIMIT - 10) as i16;
        apply_gravity(&mut fresh, 500, MAIN_HISTORY_LIMIT);
        let before = i32::from(saturated);
        apply_gravity(&mut saturated, 500, MAIN_HISTORY_LIMIT);
        assert!(i32::from(fresh) > i32::from(saturated) - before);
    }

    /// The butterfly table is indexed by the RAW move. Two promotions of the same pawn to
    /// different pieces must therefore be separate entries — under a from/to index they
    /// would collide, and the ordering would differ from upstream's.
    #[test]
    fn promotions_to_different_pieces_do_not_share_an_entry() {
        use crate::board::types::{Move, MoveType};
        let (from, to) = (Square::make(0, 6), Square::make(0, 7));
        let q = Move::typed(MoveType::Promotion, from, to, PieceType::Queen);
        let n = Move::typed(MoveType::Promotion, from, to, PieceType::Knight);
        assert_ne!(q.raw(), n.raw());

        let mut h = ButterflyHistory::default();
        h.update(Color::White, q.raw(), 1000);
        assert!(h.get(Color::White, q.raw()) > 0);
        assert_eq!(h.get(Color::White, n.raw()), 0);
    }

    /// `clear` must restore upstream's starting values, which are not zero.
    #[test]
    fn clear_restores_upstreams_starting_values() {
        let mut h = Histories::default();
        h.main.update(Color::White, 100, 1000);
        h.clear();
        assert_eq!(h.main.get(Color::White, 100), -5);
        assert_eq!(h.captures.get(Piece::W_PAWN, Square::make(0, 0), PieceType::Queen), -742);
        assert_eq!(h.pawn.get(0, Piece::W_PAWN, Square::make(0, 0)), -1338);
        assert_eq!(h.correction.entry(0, Color::White).pawn, -5);
        assert_eq!(h.continuation.get(0, Piece::W_PAWN, Square::make(0, 0)), -586);
        assert_eq!(h.continuation_correction.get(0, Piece::W_PAWN, Square::make(0, 0)), 5);
    }

    /// A pawn key and a minor key that collide must land on the same bundle but on
    /// different FIELDS of it: the interference is upstream's, the field split is too.
    #[test]
    fn the_correction_bundle_keeps_four_independent_counters() {
        let mut h = CorrectionHistory::default();
        let e = h.entry_mut(7, Color::White);
        update_correction_entry(&mut e.pawn, 500);
        assert!(h.entry(7, Color::White).pawn > -5);
        assert_eq!(h.entry(7, Color::White).minor, -5);
        assert_eq!(h.entry(7, Color::Black).pawn, -5);
    }

    /// The continuation planes are addressed by a flat index; two different parents must
    /// never share one.
    #[test]
    fn continuation_planes_are_distinct_per_parent() {
        let a = cont_plane_index(false, false, Piece::W_KNIGHT, Square::make(0, 0));
        let b = cont_plane_index(false, true, Piece::W_KNIGHT, Square::make(0, 0));
        let c = cont_plane_index(true, false, Piece::W_KNIGHT, Square::make(0, 0));
        let d = cont_plane_index(false, false, Piece::W_KNIGHT, Square::make(0, 1));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(a < 2 * 2 * PIECE_NB * SQUARE_NB);
        assert!(c < 2 * 2 * PIECE_NB * SQUARE_NB);
    }
}
