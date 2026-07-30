//! The history tables the move picker orders by.
//!
//! Every table here is a *gravity* table: an update moves the stored value toward the
//! bonus by an amount proportional to how far it still is from the clamp, so a value
//! saturates smoothly instead of pinning at the bound after a handful of updates. That
//! single rule is upstream's `StatsEntry::operator<<`, and it is what the whole ordering
//! rests on.
//!
//! The tables are large and per-worker; they are allocated on the heap as boxed arrays so
//! a `SearchWorker` stays movable and a thread does not need a megabyte-deep stack.
//!
//! Golden: `Stockfish/src/history.h`, `Stockfish/src/movepick.cpp`.

use crate::board::types::{COLOR_NB, Color, PIECE_NB, Piece, PieceType, SQUARE_NB, Square};

/// A `Box<[T; N]>` filled with `fill`, built on the heap rather than through a stack
/// temporary.
///
/// `Box::new([[0; N]; M])` materialises the whole array in a stack frame first. For the
/// tables here that is megabytes, and it overflows the thread stack in a debug build. A
/// `Vec` of the right length converted to a boxed array never touches the stack — only the
/// single `fill` element does, and that is at most a few kilobytes.
fn boxed<T: Copy, const N: usize>(fill: T) -> Box<[T; N]> {
    match vec![fill; N].into_boxed_slice().try_into() {
        Ok(b) => b,
        // The vec was built with exactly N elements, so the conversion cannot fail. Written
        // as a match rather than `.expect` because the error type is the boxed slice
        // itself, which need not be `Debug`.
        Err(_) => unreachable!("the vec was built with exactly N items"),
    }
}

/// Clamp of the butterfly (quiet-move) table.
pub const MAIN_HISTORY_LIMIT: i32 = 7183;
/// Clamp of the capture table.
pub const CAPTURE_HISTORY_LIMIT: i32 = 10692;
/// Clamp of a continuation table.
pub const CONTINUATION_LIMIT: i32 = 29952;
/// Clamp of the pawn-structure table.
pub const PAWN_HISTORY_LIMIT: i32 = 8192;
/// Clamp of every correction table.
pub const CORRECTION_LIMIT: i32 = 1024;

/// Rows in the pawn-structure history, indexed by the low bits of the pawn key.
pub const PAWN_HISTORY_SIZE: usize = 512;
/// Rows in each correction history.
pub const CORRECTION_HISTORY_SIZE: usize = 32768;

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

/// The bonus a move earns for causing a beta cutoff at `depth`.
#[inline]
#[must_use]
pub fn stat_bonus(depth: i32) -> i32 {
    (155 * depth - 96).min(1730)
}

/// The malus a move earns for failing to cause one.
#[inline]
#[must_use]
pub fn stat_malus(depth: i32) -> i32 {
    (802 * depth - 243).min(2850)
}

/// Quiet-move history, indexed by side to move and the move's from/to pair.
///
/// "Butterfly" is the classic name: the from/to index ignores which piece moved, so two
/// different pieces travelling the same path share a counter.
#[derive(Debug)]
pub struct ButterflyHistory {
    table: Box<[[i16; 4096]; COLOR_NB]>,
}

impl Default for ButterflyHistory {
    fn default() -> ButterflyHistory {
        ButterflyHistory { table: boxed([0; 4096]) }
    }
}

impl ButterflyHistory {
    /// The stored score for `c` playing `from_to`.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, c: Color, from_to: usize) -> i32 {
        i32::from(self.table[c.index()][from_to])
    }

    /// Move the stored score toward `bonus`.
    #[inline(always)]
    pub fn update(&mut self, c: Color, from_to: usize, bonus: i32) {
        apply_gravity(&mut self.table[c.index()][from_to], bonus, MAIN_HISTORY_LIMIT);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.table.iter_mut().flatten().for_each(|v| *v = 0);
    }
}

/// Capture history, indexed by the moving piece, its destination, and what it took.
#[derive(Debug)]
pub struct CaptureHistory {
    table: Box<[[[i16; 8]; SQUARE_NB]; PIECE_NB]>,
}

impl Default for CaptureHistory {
    fn default() -> CaptureHistory {
        // Built through a `Vec`, not `Box::new([..])`: the latter materialises the whole
        // array in a stack frame before moving it to the heap, and these tables are
        // megabytes. A debug build overflows the thread stack doing that.
        CaptureHistory { table: boxed([[0; 8]; SQUARE_NB]) }
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

    pub fn clear(&mut self) {
        self.table.iter_mut().flatten().flatten().for_each(|v| *v = 0);
    }
}

/// One (piece, destination) plane: the follow-up scores for moves played after a given
/// parent move.
#[derive(Debug)]
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
    table: Box<[[[[PieceToHistory; SQUARE_NB]; PIECE_NB]; 2]; 2]>,
}

impl Default for ContinuationHistory {
    fn default() -> ContinuationHistory {
        let mut h = ContinuationHistory {
            table: Box::new(std::array::from_fn(|_| {
                std::array::from_fn(|_| {
                    std::array::from_fn(|_| std::array::from_fn(|_| PieceToHistory::default()))
                })
            })),
        };
        h.clear();
        h
    }
}

impl ContinuationHistory {
    /// The plane for a parent move.
    #[inline(always)]
    #[must_use]
    pub fn plane(&self, in_check: bool, capture: bool, pc: Piece, to: Square) -> &PieceToHistory {
        &self.table[usize::from(in_check)][usize::from(capture)][pc.index()][to.index()]
    }

    #[inline(always)]
    pub fn plane_mut(
        &mut self,
        in_check: bool,
        capture: bool,
        pc: Piece,
        to: Square,
    ) -> &mut PieceToHistory {
        &mut self.table[usize::from(in_check)][usize::from(capture)][pc.index()][to.index()]
    }

    /// Reset every plane to upstream's negative sentinel.
    pub fn clear(&mut self) {
        for a in self.table.iter_mut() {
            for b in a.iter_mut() {
                for c in b.iter_mut() {
                    for p in c.iter_mut() {
                        p.fill(-40);
                    }
                }
            }
        }
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

    pub fn clear(&mut self) {
        self.table.iter_mut().flatten().flatten().for_each(|v| *v = 0);
    }
}

/// A correction table: how far the static evaluation of positions sharing some key has
/// historically been from the searched score.
#[derive(Debug)]
pub struct CorrectionHistory {
    table: Box<[[i16; COLOR_NB]; CORRECTION_HISTORY_SIZE]>,
}

impl Default for CorrectionHistory {
    fn default() -> CorrectionHistory {
        CorrectionHistory { table: boxed([0; COLOR_NB]) }
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
    pub fn get(&self, key: u64, c: Color) -> i32 {
        i32::from(self.table[Self::row(key)][c.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, key: u64, c: Color, bonus: i32) {
        apply_gravity(&mut self.table[Self::row(key)][c.index()], bonus, CORRECTION_LIMIT);
    }

    pub fn clear(&mut self) {
        self.table.iter_mut().flatten().for_each(|v| *v = 0);
    }
}

/// Every history table one worker owns.
///
/// Boxed as a unit so a worker can be moved between threads without copying a megabyte,
/// and cleared as a unit so `ucinewgame` cannot forget one of them.
#[derive(Debug, Default)]
pub struct Histories {
    pub main: ButterflyHistory,
    pub captures: CaptureHistory,
    pub continuation: ContinuationHistory,
    pub pawn: PawnHistory,
    /// Correction by pawn structure.
    pub pawn_corr: CorrectionHistory,
    /// Correction by the non-pawn material of each side.
    pub non_pawn_corr: [CorrectionHistory; COLOR_NB],
    /// Correction by the minor-piece configuration.
    pub minor_corr: CorrectionHistory,
}

impl Histories {
    /// Forget everything. Called on `ucinewgame`, never mid-search.
    pub fn clear(&mut self) {
        self.main.clear();
        self.captures.clear();
        self.continuation.clear();
        self.pawn.clear();
        self.pawn_corr.clear();
        for c in &mut self.non_pawn_corr {
            c.clear();
        }
        self.minor_corr.clear();
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

    #[test]
    fn tables_round_trip_and_clear() {
        let mut h = Histories::default();
        h.main.update(Color::White, 100, 1000);
        assert!(h.main.get(Color::White, 100) > 0);
        assert_eq!(h.main.get(Color::Black, 100), 0);

        h.captures.update(Piece::W_PAWN, Square::A1, PieceType::Queen, 500);
        assert!(h.captures.get(Piece::W_PAWN, Square::A1, PieceType::Queen) > 0);

        let row = PawnHistory::row(0xDEAD_BEEF);
        h.pawn.update(row, Piece::B_KNIGHT, Square::H8, 400);
        assert!(h.pawn.get(row, Piece::B_KNIGHT, Square::H8) > 0);

        h.pawn_corr.update(7, Color::Black, 300);
        assert!(h.pawn_corr.get(7, Color::Black) > 0);

        h.clear();
        assert_eq!(h.main.get(Color::White, 100), 0);
        assert_eq!(h.captures.get(Piece::W_PAWN, Square::A1, PieceType::Queen), 0);
        assert_eq!(h.pawn.get(row, Piece::B_KNIGHT, Square::H8), 0);
        assert_eq!(h.pawn_corr.get(7, Color::Black), 0);
    }

    /// The continuation planes start NEGATIVE, not at zero: an untouched follow-up must
    /// sort below a move that has merely never worked.
    #[test]
    fn continuation_planes_start_below_zero() {
        let h = ContinuationHistory::default();
        let plane = h.plane(false, false, Piece::W_KNIGHT, Square::A1);
        assert_eq!(plane.get(Piece::W_PAWN, Square::H8), -40);
    }

    #[test]
    fn bonus_and_malus_grow_with_depth_and_saturate() {
        assert!(stat_bonus(1) < stat_bonus(5));
        assert_eq!(stat_bonus(100), 1730);
        assert!(stat_malus(1) < stat_malus(4));
        assert_eq!(stat_malus(100), 2850);
    }
}
