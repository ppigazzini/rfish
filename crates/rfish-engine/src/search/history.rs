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

use crate::board::types::{COLOR_NB, Color, PIECE_NB, Piece, PieceType, Ply, SQUARE_NB, Square};

/// One `(piece, square)` plane, as the continuation and pawn tables store it.
///
/// Named so a caller can hold a plane across a whole move list. Resolving the plane is the
/// part of a read that does not vary with the move: the colour, the pawn key and the parent
/// moves are fixed for the list, and upstream's `ss->continuationHistory` is a POINTER
/// resolved once at the node for exactly that reason. Reading through `get` instead pays the
/// outer index — and its bounds check — once per move.
pub type PieceToPlane = [[i16; SQUARE_NB]; PIECE_NB];

/// One raw-move-indexed row, as the butterfly and low-ply tables store it.
pub type MoveRow = [i16; MOVE_HISTORY_SIZE];

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

/// A `Box<[Line<[i16; N]>; R]>` with every entry `v`, for the two tables a raw 16-bit move
/// indexes.
///
/// A row here is 128 KiB, and `clippy::large_stack_arrays` flags the literal on sight. It
/// does not survive to the stack in an optimised build, and in an unoptimised one it is one
/// frame on its way into the heap vector — the same trade [`boxed`] already documents.
/// Written once so the suppression is stated once rather than at each table.
#[allow(clippy::large_stack_arrays)]
fn boxed_rows<const R: usize, const N: usize>(v: i16) -> Box<[Line<[i16; N]>; R]> {
    boxed(Line([v; N]))
}

/// A table row forced onto a cache-line boundary.
///
/// The history tables are the largest randomly indexed structures the search touches, and
/// their natural alignment is the alignment of an `i16` -- two bytes. The allocator hands
/// back sixteen, so every table began sixteen bytes INTO a line and every row inherited
/// that offset for the life of the process: a row that should occupy one line occupied
/// two, and a lookup that should be one fetch was two.
///
/// Nothing reads these tables at a coarser granularity than a row, so the alignment was an
/// omission rather than a decision. `Deref` keeps the indexing at the call sites unchanged.
#[repr(align(64))]
#[derive(Clone, Copy, Debug)]
pub struct Line<T>(T);

impl<T> core::ops::Deref for Line<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> core::ops::DerefMut for Line<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
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

/// A history-table update, in the tables' own units.
///
/// Not a [`Value`](crate::board::types::Value) and not a plain number: a bonus is clamped to
/// a table's `LIMIT`, is produced by depth-scaled formulas, and means nothing outside the
/// gravity rule. It shared `i32` with the score domain, the reduction, the move-picker
/// ordering number and the stat score.
///
/// The operators are the ones the formulas use — scale, divide, negate, clamp, compare — and
/// there is no `Add<Bonus>`: two bonuses are never summed, they are applied one after the
/// other by [`apply_gravity`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct Bonus(i32);

impl Bonus {
    /// Build a bonus from the number a depth-scaled formula produced.
    #[inline(always)]
    #[must_use]
    pub const fn new(v: i32) -> Bonus {
        Bonus(v)
    }

    /// The number, for the one formula that mixes a bonus with a stat score.
    #[inline(always)]
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl core::ops::Neg for Bonus {
    type Output = Bonus;
    #[inline(always)]
    fn neg(self) -> Bonus {
        Bonus(-self.0)
    }
}

impl core::ops::Mul<i32> for Bonus {
    type Output = Bonus;
    #[inline(always)]
    fn mul(self, k: i32) -> Bonus {
        Bonus(self.0 * k)
    }
}

impl core::ops::Div<i32> for Bonus {
    type Output = Bonus;
    #[inline(always)]
    fn div(self, k: i32) -> Bonus {
        Bonus(self.0 / k)
    }
}

impl core::ops::Add<i32> for Bonus {
    type Output = Bonus;
    #[inline(always)]
    fn add(self, m: i32) -> Bonus {
        Bonus(self.0 + m)
    }
}

impl PartialOrd<i32> for Bonus {
    #[inline(always)]
    fn partial_cmp(&self, m: &i32) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(m)
    }
}

impl PartialEq<i32> for Bonus {
    #[inline(always)]
    fn eq(&self, m: &i32) -> bool {
        self.0 == *m
    }
}

/// Move a stored value toward `bonus`, by upstream's gravity rule.
///
/// The value approaches `LIMIT` asymptotically: the closer it already is, the less an update
/// moves it. A table that simply added and clamped would lose the ordering information
/// between two moves that had both saturated.
///
/// **The clamp is a const parameter, not an argument, because it belongs to the TABLE.**
/// Every caller passed its own table's constant, so the two `i32`s travelled adjacent and
/// were silently interchangeable — and a swap does not fail here, it produces a differently
/// shaped gravity curve and a different move ordering. `ContinuationHistory` is the one type
/// that updates at two different clamps, and that difference is now in two method names
/// rather than in what a caller remembered to pass.
#[inline(always)]
fn apply_gravity<const LIMIT: i32>(entry: &mut i16, bonus: Bonus) {
    let bonus = bonus.get().clamp(-LIMIT, LIMIT);
    let v = i32::from(*entry);
    *entry = (v + bonus - v * bonus.abs() / LIMIT) as i16;
}

/// Quiet-move history, indexed by side to move and the raw move.
///
/// "Butterfly" is the classic name: the index ignores which piece moved, so two different
/// pieces travelling the same path share a counter.
///
/// One row per colour, each row on a cache-line boundary. A flat `Box<[i16]>` carried the
/// alignment of an `i16`, so the allocator's sixteen-byte offset skewed the whole table for
/// the life of the process; [`Line`] states the requirement the hardware actually has.
#[derive(Debug)]
pub struct ButterflyHistory {
    table: Box<[Line<[i16; MOVE_HISTORY_SIZE]>; COLOR_NB]>,
}

impl Default for ButterflyHistory {
    fn default() -> ButterflyHistory {
        ButterflyHistory { table: boxed_rows(0) }
    }
}

impl ButterflyHistory {
    /// The stored score for `c` playing the raw move `mv`.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, c: Color, mv: u16) -> i32 {
        i32::from(self.table[c.index()][mv as usize])
    }

    /// The whole row for `c`, for a caller that reads many moves at one colour.
    #[inline(always)]
    #[must_use]
    pub fn row(&self, c: Color) -> &MoveRow {
        &self.table[c.index()]
    }

    /// Move the stored score toward `bonus`.
    #[inline(always)]
    pub fn update(&mut self, c: Color, mv: u16, bonus: Bonus) {
        apply_gravity::<MAIN_HISTORY_LIMIT>(&mut self.table[c.index()][mv as usize], bonus);
    }

    /// Reset every entry to `v`.
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().for_each(|row| row.fill(v));
    }

    /// Scale every entry by `num / den`.
    ///
    /// Upstream decays the butterfly table between moves rather than clearing it: the
    /// previous move's ordering is still evidence, just weaker than the current one's.
    pub fn decay(&mut self, num: i32, den: i32) {
        self.table
            .iter_mut()
            .for_each(|row| row.iter_mut().for_each(|x| *x = (i32::from(*x) * num / den) as i16));
    }
}

/// Quiet-move history for the plies nearest the root, where ordering matters most.
#[derive(Debug)]
pub struct LowPlyHistory {
    table: Box<[Line<[i16; MOVE_HISTORY_SIZE]>; LOW_PLY_HISTORY_SIZE]>,
}

impl Default for LowPlyHistory {
    fn default() -> LowPlyHistory {
        LowPlyHistory { table: boxed_rows(0) }
    }
}

impl LowPlyHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, ply: Ply, mv: u16) -> i32 {
        i32::from(self.table[ply.index()][mv as usize])
    }

    /// The whole row for `ply`, for a caller that reads many moves at one ply.
    #[inline(always)]
    #[must_use]
    pub fn row(&self, ply: Ply) -> &MoveRow {
        &self.table[ply.index()]
    }

    #[inline(always)]
    pub fn update(&mut self, ply: Ply, mv: u16, bonus: Bonus) {
        apply_gravity::<MAIN_HISTORY_LIMIT>(&mut self.table[ply.index()][mv as usize], bonus);
    }

    /// Reset every entry to `v`. Upstream refills this at the start of every iteration,
    /// not on `ucinewgame`.
    pub fn fill(&mut self, v: i16) {
        self.table.iter_mut().for_each(|row| row.fill(v));
    }
}

/// Capture history, indexed by the moving piece, its destination, and what it took.
#[derive(Debug)]
pub struct CaptureHistory {
    table: Box<[Line<[[i16; PIECE_TYPE_SLOTS]; SQUARE_NB]>; PIECE_NB]>,
}

/// Slots for the captured piece type. Upstream's `PIECE_TYPE_NB` is 8; the values used run
/// 0..=6, so seven would do, but the eighth keeps the index arithmetic identical.
const PIECE_TYPE_SLOTS: usize = 8;

impl Default for CaptureHistory {
    fn default() -> CaptureHistory {
        CaptureHistory { table: boxed(Line([[0; PIECE_TYPE_SLOTS]; SQUARE_NB])) }
    }
}

impl CaptureHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, pc: Piece, to: Square, captured: PieceType) -> i32 {
        i32::from(self.table[pc.index()][to.index()][captured.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, pc: Piece, to: Square, captured: PieceType, bonus: Bonus) {
        apply_gravity::<CAPTURE_HISTORY_LIMIT>(
            &mut self.table[pc.index()][to.index()][captured.index()],
            bonus,
        );
    }

    pub fn fill(&mut self, v: i16) {
        for row in &mut *self.table {
            // `fill` on the innermost slice, so this stays a wide store rather than the
            // element-at-a-time loop a nested `for_each` compiles to.
            for inner in row.iter_mut() {
                inner.fill(v);
            }
        }
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
    pub fn update(&mut self, pc: Piece, to: Square, bonus: Bonus) {
        apply_gravity::<CONTINUATION_LIMIT>(&mut self.table[pc.index()][to.index()], bonus);
    }

    /// Update at the correction clamp rather than the continuation one.
    ///
    /// The same plane shape serves two tables with different limits, and the limit is part
    /// of the gravity arithmetic — using the wrong one changes every stored value.
    #[inline(always)]
    pub fn update_correction(&mut self, pc: Piece, to: Square, bonus: Bonus) {
        apply_gravity::<CORRECTION_LIMIT>(&mut self.table[pc.index()][to.index()], bonus);
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
    table: Box<[Line<[[i16; SQUARE_NB]; PIECE_NB]>; 2 * 2 * PIECE_NB * SQUARE_NB]>,
}

/// The flat index of a continuation plane.
///
/// A distinct type from [`CorrKey`] and [`PawnRow`] because all three are flat indices into
/// differently shaped tables and all three used to be `usize`. A `StackEntry` carries a
/// continuation plane and a correction plane side by side; the correction space is a SUBRANGE
/// of the continuation space, so swapping them compiled, never panicked, and silently read
/// the wrong plane.
///
/// No arithmetic, no `From<usize>`: an index is produced by [`cont_plane_index`] and consumed
/// by [`ContinuationHistory`], and there is no third thing to do with one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ContKey(usize);

impl ContKey {
    /// The plane given to a slot a picker never reads.
    ///
    /// Not every move-picker sequence reads all six planes: the quiescence and `ProbCut`
    /// sequences reach `score_quiets` only from `QuietInit`, which they never enter. Those
    /// slots were `Option<usize>`, which put a branch per plane on the picker's hottest line
    /// -- 17.2M instructions on a bench, in a branch that could never be taken.
    ///
    /// A named constant says the same thing with no branch. Plane zero is also a REAL plane,
    /// the one a move by [`Piece::NONE`] to a1 selects, which no move can produce; that is
    /// what makes it safe to read as well as safe to name, and it is the same plane a stack
    /// entry with no previous move carries.
    pub const UNREAD: ContKey = ContKey(0);

    /// Index the named plane into the continuation table.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// The flat index of a continuation plane.
#[inline(always)]
#[must_use]
pub fn cont_plane_index(in_check: bool, capture: bool, pc: Piece, to: Square) -> ContKey {
    ContKey(
        ((usize::from(in_check) * 2 + usize::from(capture)) * PIECE_NB + pc.index()) * SQUARE_NB
            + to.index(),
    )
}

impl Default for ContinuationHistory {
    fn default() -> ContinuationHistory {
        ContinuationHistory { table: boxed(Line([[-586; SQUARE_NB]; PIECE_NB])) }
    }
}

impl ContinuationHistory {
    /// The score a follow-up `(pc, to)` has after the parent plane `idx`.
    #[inline(always)]
    #[must_use]
    pub fn get(&self, idx: ContKey, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[idx.index()][pc.index()][to.index()])
    }

    /// The whole plane `idx` selects, for a caller that reads many moves at one plane.
    #[inline(always)]
    #[must_use]
    pub fn plane(&self, idx: ContKey) -> &PieceToPlane {
        &self.table[idx.index()]
    }

    #[inline(always)]
    pub fn update(&mut self, idx: ContKey, pc: Piece, to: Square, bonus: Bonus) {
        apply_gravity::<CONTINUATION_LIMIT>(
            &mut self.table[idx.index()][pc.index()][to.index()],
            bonus,
        );
    }

    /// Reset every plane to upstream's negative sentinel.
    pub fn fill(&mut self, v: i16) {
        for row in &mut *self.table {
            // `fill` on the innermost slice, so this stays a wide store rather than the
            // element-at-a-time loop a nested `for_each` compiles to.
            for inner in row.iter_mut() {
                inner.fill(v);
            }
        }
    }
}

/// The continuation CORRECTION tables: how far the static evaluation has been off after a
/// given parent move, keyed the same way but clamped at the correction limit.
#[derive(Debug)]
pub struct ContinuationCorrectionHistory {
    table: Box<[Line<[[i16; SQUARE_NB]; PIECE_NB]>; PIECE_NB * SQUARE_NB]>,
}

/// The flat index of a continuation-correction plane.
///
/// Distinct from [`ContKey`] even though both are flat plane indices, because the correction
/// table is a quarter the size: every valid `CorrKey` is also a valid `ContKey`, so a swap
/// reads a real plane of the wrong table rather than panicking. See [`ContKey`].
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CorrKey(usize);

impl CorrKey {
    /// The plane a stack entry with no previous move carries, for the same reason
    /// [`ContKey::UNREAD`] does: it is the plane a [`Piece::NONE`] move to a1 selects.
    pub const UNREAD: CorrKey = CorrKey(0);

    /// Index the named plane into the correction table.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// The flat index of a continuation-correction plane. Unlike the continuation history this
/// one is not split by check or capture.
#[inline(always)]
#[must_use]
pub fn corr_plane_index(pc: Piece, to: Square) -> CorrKey {
    CorrKey(pc.index() * SQUARE_NB + to.index())
}

impl Default for ContinuationCorrectionHistory {
    fn default() -> ContinuationCorrectionHistory {
        ContinuationCorrectionHistory { table: boxed(Line([[5; SQUARE_NB]; PIECE_NB])) }
    }
}

impl ContinuationCorrectionHistory {
    #[inline(always)]
    #[must_use]
    pub fn get(&self, idx: CorrKey, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[idx.index()][pc.index()][to.index()])
    }

    #[inline(always)]
    pub fn update(&mut self, idx: CorrKey, pc: Piece, to: Square, bonus: Bonus) {
        apply_gravity::<CORRECTION_LIMIT>(
            &mut self.table[idx.index()][pc.index()][to.index()],
            bonus,
        );
    }

    pub fn fill(&mut self, v: i16) {
        for row in &mut *self.table {
            // `fill` on the innermost slice, so this stays a wide store rather than the
            // element-at-a-time loop a nested `for_each` compiles to.
            for inner in row.iter_mut() {
                inner.fill(v);
            }
        }
    }
}

/// Pawn-structure history: quiet-move scores conditioned on the pawn skeleton.
#[derive(Debug)]
pub struct PawnHistory {
    table: Box<[Line<[[i16; SQUARE_NB]; PIECE_NB]>; PAWN_HISTORY_SIZE]>,
}

impl Default for PawnHistory {
    fn default() -> PawnHistory {
        PawnHistory { table: boxed(Line([[0; SQUARE_NB]; PIECE_NB])) }
    }
}

/// The row of the pawn-history table a pawn key selects.
///
/// The third flat index into a `PieceToPlane`-shaped table, and the third that used to be
/// `usize`. It is masked into range by [`PawnHistory::row`], so unlike [`ContKey`] and
/// [`CorrKey`] it cannot be out of bounds -- but it is still not either of them, and the
/// three appear within a few lines of each other in `movepick::score_quiets`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PawnRow(usize);

impl PawnRow {
    /// Index the named row into the pawn table.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

impl PawnHistory {
    /// The row a pawn key selects.
    #[inline(always)]
    #[must_use]
    pub fn row(pawn_key: u64) -> PawnRow {
        PawnRow((pawn_key as usize) & (PAWN_HISTORY_SIZE - 1))
    }

    #[inline(always)]
    #[must_use]
    pub fn get(&self, row: PawnRow, pc: Piece, to: Square) -> i32 {
        i32::from(self.table[row.index()][pc.index()][to.index()])
    }

    /// The whole plane `row` selects, for a caller that reads many moves at one pawn key.
    #[inline(always)]
    #[must_use]
    pub fn plane(&self, row: PawnRow) -> &PieceToPlane {
        &self.table[row.index()]
    }

    #[inline(always)]
    pub fn update(&mut self, row: PawnRow, pc: Piece, to: Square, bonus: Bonus) {
        apply_gravity::<PAWN_HISTORY_LIMIT>(
            &mut self.table[row.index()][pc.index()][to.index()],
            bonus,
        );
    }

    pub fn fill(&mut self, v: i16) {
        for row in &mut *self.table {
            // `fill` on the innermost slice, so this stays a wide store rather than the
            // element-at-a-time loop a nested `for_each` compiles to.
            for inner in row.iter_mut() {
                inner.fill(v);
            }
        }
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
pub fn update_correction_entry(entry: &mut i16, bonus: Bonus) {
    apply_gravity::<CORRECTION_LIMIT>(entry, bonus);
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
    pub fn update_tt_move(&mut self, bonus: Bonus) {
        apply_gravity::<TT_MOVE_HISTORY_LIMIT>(&mut self.tt_move, bonus);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::{File, Rank};

    #[test]
    fn gravity_saturates_without_pinning() {
        let mut v: i16 = 0;
        // Repeated maximum bonuses approach the limit but never exceed it.
        for _ in 0..1000 {
            apply_gravity::<MAIN_HISTORY_LIMIT>(&mut v, Bonus::new(MAIN_HISTORY_LIMIT));
        }
        assert!(i32::from(v) <= MAIN_HISTORY_LIMIT);
        assert!(i32::from(v) > MAIN_HISTORY_LIMIT - 2);

        // And symmetric on the way down.
        for _ in 0..1000 {
            apply_gravity::<MAIN_HISTORY_LIMIT>(&mut v, Bonus::new(-MAIN_HISTORY_LIMIT));
        }
        assert!(i32::from(v) >= -MAIN_HISTORY_LIMIT);
        assert!(i32::from(v) < -MAIN_HISTORY_LIMIT + 2);
    }

    #[test]
    fn a_small_bonus_moves_a_saturated_entry_less_than_a_fresh_one() {
        // The property gravity exists for: ordering information survives saturation.
        let mut fresh: i16 = 0;
        let mut saturated: i16 = (MAIN_HISTORY_LIMIT - 10) as i16;
        apply_gravity::<MAIN_HISTORY_LIMIT>(&mut fresh, Bonus::new(500));
        let before = i32::from(saturated);
        apply_gravity::<MAIN_HISTORY_LIMIT>(&mut saturated, Bonus::new(500));
        assert!(i32::from(fresh) > i32::from(saturated) - before);
    }

    /// The butterfly table is indexed by the RAW move. Two promotions of the same pawn to
    /// different pieces must therefore be separate entries — under a from/to index they
    /// would collide, and the ordering would differ from upstream's.
    #[test]
    fn promotions_to_different_pieces_do_not_share_an_entry() {
        use crate::board::types::{Move, MoveType};
        let (from, to) =
            (Square::make(File::new(0), Rank::new(6)), Square::make(File::new(0), Rank::new(7)));
        let q = Move::typed(MoveType::Promotion, from, to, PieceType::Queen);
        let n = Move::typed(MoveType::Promotion, from, to, PieceType::Knight);
        assert_ne!(q.raw(), n.raw());

        let mut h = ButterflyHistory::default();
        h.update(Color::White, q.raw(), Bonus::new(1000));
        assert!(h.get(Color::White, q.raw()) > 0);
        assert_eq!(h.get(Color::White, n.raw()), 0);
    }

    /// `clear` must restore upstream's starting values, which are not zero.
    #[test]
    fn clear_restores_upstreams_starting_values() {
        let mut h = Histories::default();
        h.main.update(Color::White, 100, Bonus::new(1000));
        h.clear();
        assert_eq!(h.main.get(Color::White, 100), -5);
        assert_eq!(
            h.captures.get(
                Piece::W_PAWN,
                Square::make(File::new(0), Rank::new(0)),
                PieceType::Queen
            ),
            -742
        );
        assert_eq!(
            h.pawn.get(
                PawnHistory::row(0),
                Piece::W_PAWN,
                Square::make(File::new(0), Rank::new(0))
            ),
            -1338
        );
        assert_eq!(h.correction.entry(0, Color::White).pawn, -5);
        assert_eq!(
            h.continuation.get(
                ContKey::UNREAD,
                Piece::W_PAWN,
                Square::make(File::new(0), Rank::new(0))
            ),
            -586
        );
        assert_eq!(
            h.continuation_correction.get(
                CorrKey::UNREAD,
                Piece::W_PAWN,
                Square::make(File::new(0), Rank::new(0))
            ),
            5
        );
    }

    /// A pawn key and a minor key that collide must land on the same bundle but on
    /// different FIELDS of it: the interference is upstream's, the field split is too.
    #[test]
    fn the_correction_bundle_keeps_four_independent_counters() {
        let mut h = CorrectionHistory::default();
        let e = h.entry_mut(7, Color::White);
        update_correction_entry(&mut e.pawn, Bonus::new(500));
        assert!(h.entry(7, Color::White).pawn > -5);
        assert_eq!(h.entry(7, Color::White).minor, -5);
        assert_eq!(h.entry(7, Color::Black).pawn, -5);
    }

    /// The continuation planes are addressed by a flat index; two different parents must
    /// never share one.
    #[test]
    fn continuation_planes_are_distinct_per_parent() {
        let a = cont_plane_index(
            false,
            false,
            Piece::W_KNIGHT,
            Square::make(File::new(0), Rank::new(0)),
        );
        let b = cont_plane_index(
            false,
            true,
            Piece::W_KNIGHT,
            Square::make(File::new(0), Rank::new(0)),
        );
        let c = cont_plane_index(
            true,
            false,
            Piece::W_KNIGHT,
            Square::make(File::new(0), Rank::new(0)),
        );
        let d = cont_plane_index(
            false,
            false,
            Piece::W_KNIGHT,
            Square::make(File::new(0), Rank::new(1)),
        );
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(a.index() < 2 * 2 * PIECE_NB * SQUARE_NB);
        assert!(c.index() < 2 * 2 * PIECE_NB * SQUARE_NB);
    }
}
