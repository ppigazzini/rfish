//! The state blocks the search and the shell share.
//!
//! Nothing here searches or evaluates. These are the types that cross the zone boundary:
//! what the caller asked for ([`Limits`]), what a root move is worth ([`RootMove`]), and
//! the per-thread block a worker owns ([`crate::search::worker::SearchWorker`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::board::types::{Color, MAX_PLY, Move, VALUE_DRAW, VALUE_INFINITE, Value};

/// What the caller asked the search to do.
///
/// Every field is what upstream's `Limits` holds, and the same rule applies: a zero or
/// empty field means "not constrained by this", not "constrained to zero".
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// Milliseconds left on each side's clock.
    pub time: [Option<u64>; 2],
    /// Increment per move, per side.
    pub inc: [u64; 2],
    /// Moves until the next time control, or `None` for sudden death.
    pub moves_to_go: Option<u32>,
    /// A fixed depth to search to.
    pub depth: Option<i32>,
    /// A fixed node count to search.
    pub nodes: Option<u64>,
    /// A fixed time per move, in milliseconds.
    pub move_time: Option<u64>,
    /// Search until told to stop.
    pub infinite: bool,
    /// Search for a mate in this many moves.
    pub mate: Option<i32>,
    /// Only these root moves, in this order.
    pub search_moves: Vec<Move>,
    /// When the `go` was received. Set by the shell, so the time spent parsing counts
    /// against the move rather than being invisible.
    pub start: Option<Instant>,
    /// Plies already played, for the time manager's move-number heuristics.
    pub ply: i32,
    /// Nodes per millisecond, when `nodestime` converts the clock into a node budget.
    ///
    /// Zero means the clock is a real clock. The time manager writes this from the option,
    /// so the search reads one field rather than being handed the whole option map.
    pub npmsec: u64,
}

impl Limits {
    /// True when nothing bounds the search but an explicit stop.
    #[must_use]
    pub fn is_unbounded(&self) -> bool {
        self.infinite
            || (self.depth.is_none()
                && self.nodes.is_none()
                && self.move_time.is_none()
                && self.time[0].is_none()
                && self.time[1].is_none()
                && self.mate.is_none())
    }

    /// True when the search must manage a clock rather than a fixed budget.
    ///
    /// A clock on either side is the whole test, matching upstream. `movetime` is NOT
    /// excluded here: it is enforced directly against the elapsed count, not through the
    /// budget, so folding it in would give one limit two enforcement paths.
    #[must_use]
    pub fn uses_time_management(&self) -> bool {
        self.time.iter().any(|t| t.is_some_and(|ms| ms > 0))
    }
}

/// The option values the search reads.
///
/// Upstream hands the search its whole `OptionsMap` and it pulls out what it needs. The
/// engine crate cannot see the shell's option model, so the shell fills this in instead —
/// which also makes the set of options the search actually depends on a declared list
/// rather than something a reader has to grep for.
#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    /// How many principal variations to report.
    pub multi_pv: usize,
    /// Milliseconds reserved for the move to reach the GUI.
    pub move_overhead: u64,
    /// Nodes per millisecond, or zero for a real clock.
    pub nodestime: u64,
    /// Whether the GUI allows pondering, which buys the current move extra time.
    pub ponder: bool,
    /// Playing strength, 0 (weakest) to 20 (full strength).
    pub skill_level: i32,
    /// The Elo to play at, or `None` when `UCI_LimitStrength` is off.
    pub uci_elo: Option<i32>,
}

impl Default for SearchOptions {
    fn default() -> SearchOptions {
        SearchOptions {
            multi_pv: 1,
            move_overhead: 10,
            nodestime: 0,
            ponder: false,
            skill_level: 20,
            uci_elo: None,
        }
    }
}

/// One move from the root, with everything the emitter needs to report it.
#[derive(Clone, Debug)]
pub struct RootMove {
    pub mv: Move,
    /// The score from the completed iteration.
    pub score: Value,
    /// The score from the previous completed iteration, used to detect instability.
    pub previous_score: Value,
    /// The best score this move reached at any depth, for `MultiPV` ordering.
    pub average_score: Value,
    /// How deep the search went below this move.
    pub sel_depth: i32,
    /// The principal variation starting with this move.
    pub pv: Vec<Move>,
    /// Tablebase rank, for root filtering. Zero when no tablebase was consulted.
    pub tb_rank: i32,
    /// What the tablebases say this move is worth.
    ///
    /// Kept apart from `score`, which the search owns. When the root is in the tables the
    /// reporter shows THIS: the tables know the result exactly and a search score is only
    /// an estimate of a fact already established.
    pub tb_score: Value,
    /// Nodes spent below this move across the whole search, for time management.
    ///
    /// A best move that already accounts for most of the tree is unlikely to be displaced
    /// by spending longer, so the time manager reads this as a reason to stop early.
    pub effort: u64,
}

impl RootMove {
    /// A fresh root move, before any search.
    #[must_use]
    pub fn new(mv: Move) -> RootMove {
        RootMove {
            mv,
            score: -VALUE_INFINITE,
            previous_score: -VALUE_INFINITE,
            average_score: -VALUE_INFINITE,
            sel_depth: 0,
            pv: vec![mv],
            tb_rank: 0,
            tb_score: VALUE_DRAW,
            effort: 0,
        }
    }
}

/// Root moves sort by score, best first — the order `MultiPV` reports them in.
impl PartialEq for RootMove {
    fn eq(&self, other: &RootMove) -> bool {
        self.mv == other.mv
    }
}

impl Eq for RootMove {}

impl Ord for RootMove {
    fn cmp(&self, other: &RootMove) -> core::cmp::Ordering {
        // Descending by score, then by the previous iteration's score to break ties
        // stably: an unstable tie-break makes the reported PV flicker between equal moves.
        other.score.cmp(&self.score).then_with(|| other.previous_score.cmp(&self.previous_score))
    }
}

impl PartialOrd for RootMove {
    fn partial_cmp(&self, other: &RootMove) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The signals every search thread reads, and the counters they all write.
///
/// Shared by `Arc` and mutated only through atomics, so a stop request from the UCI thread
/// reaches a searching thread without a lock and without either side blocking.
#[derive(Debug, Default)]
pub struct SharedState {
    /// Set by the shell to abort the search.
    pub stop: AtomicBool,
    /// Set when `ponderhit` is expected to convert the search into a real one.
    pub ponder: AtomicBool,
    /// Total nodes searched across every thread.
    pub nodes: AtomicU64,
    /// Total tablebase hits across every thread.
    pub tb_hits: AtomicU64,
    /// Root best-move changes, pooled across every thread since the last read.
    ///
    /// Upstream walks the thread list each iteration, sums each worker's counter and zeroes
    /// it. A thread here is inside a `scope` and the main thread cannot reach into it, so
    /// the sum accumulates in one place instead and the read takes it: a `swap` is exactly
    /// "add them all up, then set them all to zero".
    pub best_move_changes: AtomicU64,
    /// Cleared by the time manager when a move is taking long enough to be worth
    /// re-searching the same depth rather than deepening.
    pub increase_depth: AtomicBool,
    /// Set when the budget has run out but the engine is pondering, so the search keeps
    /// going and stops the moment the GUI converts the ponder into a real move.
    pub stop_on_ponderhit: AtomicBool,
}

impl SharedState {
    /// A fresh set of signals, ready for one `go`.
    #[must_use]
    pub fn new() -> Arc<SharedState> {
        Arc::new(SharedState::default())
    }

    /// True when the search has been told to stop.
    #[inline(always)]
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Ask every searching thread to stop.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Clear the signals and counters for a new search.
    pub fn reset(&self) {
        self.stop.store(false, Ordering::Relaxed);
        self.nodes.store(0, Ordering::Relaxed);
        self.tb_hits.store(0, Ordering::Relaxed);
        self.best_move_changes.store(0, Ordering::Relaxed);
        self.increase_depth.store(true, Ordering::Relaxed);
        self.stop_on_ponderhit.store(false, Ordering::Relaxed);
    }

    /// True while the search is thinking on the opponent's clock.
    #[inline]
    #[must_use]
    pub fn pondering(&self) -> bool {
        self.ponder.load(Ordering::Relaxed)
    }

    /// Stop as soon as the ponder becomes a real search, but not before.
    pub fn request_stop_on_ponderhit(&self) {
        self.stop_on_ponderhit.store(true, Ordering::Relaxed);
    }

    /// Convert a ponder into a real search, honouring a budget that already ran out.
    ///
    /// This is the whole of `ponderhit`: the thinking already done counts, and if the
    /// budget was spent while pondering the move is played immediately.
    pub fn ponder_hit(&self) {
        self.ponder.store(false, Ordering::Relaxed);
        if self.stop_on_ponderhit.load(Ordering::Relaxed) {
            self.request_stop();
        }
    }

    /// Set whether iterations should keep deepening.
    pub fn set_increase_depth(&self, yes: bool) {
        self.increase_depth.store(yes, Ordering::Relaxed);
    }

    /// True when the budget ran out while pondering.
    #[inline]
    #[must_use]
    pub fn stop_on_ponderhit(&self) -> bool {
        self.stop_on_ponderhit.load(Ordering::Relaxed)
    }

    /// Record that this thread's best root move changed.
    #[inline]
    pub fn note_best_move_change(&self) {
        self.best_move_changes.fetch_add(1, Ordering::Relaxed);
    }

    /// Take the pooled best-move-change count, leaving zero behind.
    #[inline]
    #[must_use]
    pub fn take_best_move_changes(&self) -> u64 {
        self.best_move_changes.swap(0, Ordering::Relaxed)
    }

    /// True while iterations should keep deepening rather than re-running a depth.
    #[inline]
    #[must_use]
    pub fn increase_depth(&self) -> bool {
        self.increase_depth.load(Ordering::Relaxed)
    }

    /// Add `n` to the global node count.
    #[inline(always)]
    pub fn add_nodes(&self, n: u64) {
        self.nodes.fetch_add(n, Ordering::Relaxed);
    }

    /// The global node count.
    #[inline(always)]
    #[must_use]
    pub fn node_count(&self) -> u64 {
        self.nodes.load(Ordering::Relaxed)
    }
}

/// The time budget for one move, in the unit the clock is being measured in.
///
/// Upstream's `TimeManagement` is two numbers plus the flag that says what the numbers
/// count. Under `nodestime` the whole time model switches to nodes — the clock, the
/// increment and the move overhead are all multiplied into node counts, and `elapsed`
/// returns nodes searched rather than milliseconds. Keeping the flag next to the two
/// bounds is what stops a caller comparing a node count against a millisecond.
#[derive(Clone, Copy, Debug)]
pub struct TimeBudget {
    /// When the search started.
    pub start: Instant,
    /// The point past which a new iteration should not be started.
    pub optimum: i64,
    /// The point past which the search stops even mid-iteration.
    pub maximum: i64,
    /// True when the two bounds count nodes rather than milliseconds.
    pub use_nodes_time: bool,
}

impl Default for TimeBudget {
    fn default() -> TimeBudget {
        TimeBudget { start: Instant::now(), optimum: 0, maximum: 0, use_nodes_time: false }
    }
}

impl TimeBudget {
    /// How far the search has got, in whichever unit the budget counts.
    ///
    /// `nodes` is the pool-wide count, needed only in nodes-as-time mode; the caller passes
    /// it unconditionally because reading a relaxed atomic is cheaper than branching around
    /// it.
    #[inline]
    #[must_use]
    pub fn elapsed(&self, nodes: u64) -> i64 {
        if self.use_nodes_time { nodes as i64 } else { self.elapsed_time() }
    }

    /// Wall-clock milliseconds since the search started, whatever the mode.
    ///
    /// Reporting stays in real time: a GUI that is told a search took 40 000 "milliseconds"
    /// because the engine was counting nodes will draw the wrong conclusion.
    #[inline]
    #[must_use]
    pub fn elapsed_time(&self) -> i64 {
        self.start.elapsed().as_millis() as i64
    }
}

/// The maximum ply the search stack is sized for.
pub const STACK_SIZE: usize = MAX_PLY + 10;

/// One ply of the search stack.
///
/// Upstream indexes this array from `-7`, so a node can look back seven plies without a
/// bounds test. Here the array starts at zero and the search offsets by [`STACK_BASE`],
/// which is the same trick with the offset written down instead of hidden in a pointer.
#[derive(Clone, Debug)]
pub struct StackEntry {
    /// The move being searched at this ply.
    pub current_move: Move,
    /// The best move found so far below this ply.
    pub pv: Vec<Move>,
    /// The static evaluation of this node.
    pub static_eval: Value,
    /// Which move was excluded by a singular-extension search, or [`Move::NONE`].
    pub excluded_move: Move,
    /// The two killer moves for this ply.
    pub killers: [Move; 2],
    /// How many moves have been searched at this ply.
    pub move_count: i32,
    /// True when this node is on the principal variation.
    pub in_check: bool,
    /// True when the parent's move was a capture.
    pub tt_pv: bool,
    /// The number of consecutive cut nodes above this one.
    pub cutoff_count: i32,
    /// The reduction this node inherited from its parent.
    pub reduction: i32,
    /// The piece that moved into this node, for the continuation tables.
    pub moved_piece: crate::board::types::Piece,
}

impl Default for StackEntry {
    fn default() -> StackEntry {
        StackEntry {
            current_move: Move::NONE,
            pv: Vec::new(),
            static_eval: VALUE_INFINITE,
            excluded_move: Move::NONE,
            killers: [Move::NONE; 2],
            move_count: 0,
            in_check: false,
            tt_pv: false,
            cutoff_count: 0,
            reduction: 0,
            moved_piece: crate::board::types::Piece::NONE,
        }
    }
}

/// How far into the stack array ply 0 sits, so a node can index four plies back.
///
/// The continuation tables look back one, two, four and six plies, so ply 0 must sit at
/// least six entries in. Asserted at compile time rather than in a test, because a test
/// can be deleted and this bound is what keeps the lookback in range.
pub const STACK_BASE: usize = 8;
const _: () = assert!(STACK_BASE >= 6, "the continuation lookback reaches six plies back");
const _: () = assert!(STACK_SIZE > MAX_PLY + STACK_BASE, "the stack must reach MAX_PLY");

/// Which side's clock a colour reads.
#[inline(always)]
#[must_use]
pub fn clock_index(c: Color) -> usize {
    c.index()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_limits_is_unbounded() {
        assert!(Limits::default().is_unbounded());
        let depth = Limits { depth: Some(8), ..Limits::default() };
        assert!(!depth.is_unbounded());
        assert!(!depth.uses_time_management());
    }

    #[test]
    fn a_clock_is_the_whole_test_for_time_management() {
        let clock = Limits { time: [Some(60_000), Some(60_000)], ..Limits::default() };
        assert!(clock.uses_time_management());

        // A clock plus `movetime` still manages time. `movetime` is enforced against the
        // elapsed count directly, so excluding it here would silence the maximum as well
        // and a `go movetime` on a clock would run past both.
        assert!(Limits { move_time: Some(1000), ..clock.clone() }.uses_time_management());
        assert!(Limits { infinite: true, ..clock }.uses_time_management());

        assert!(!Limits { infinite: true, ..Limits::default() }.uses_time_management());
        assert!(!Limits { move_time: Some(1000), ..Limits::default() }.uses_time_management());
        // A zero clock is "no clock given", not "no time left".
        assert!(!Limits { time: [Some(0), Some(0)], ..Limits::default() }.uses_time_management());
    }

    #[test]
    fn root_moves_sort_best_first() {
        let mut a = RootMove::new(Move::from_raw(1));
        let mut b = RootMove::new(Move::from_raw(2));
        a.score = 10;
        b.score = 50;
        let mut v = [a, b];
        v.sort();
        assert_eq!(v[0].score, 50);
    }

    #[test]
    fn the_stop_signal_crosses_threads() {
        let shared = SharedState::new();
        let s2 = Arc::clone(&shared);
        let h = std::thread::spawn(move || {
            while !s2.stopped() {
                std::hint::spin_loop();
            }
            s2.add_nodes(7);
        });
        shared.request_stop();
        h.join().expect("the worker thread stops");
        assert_eq!(shared.node_count(), 7);
    }

    #[test]
    fn reset_clears_the_stop_flag_and_the_counters() {
        let shared = SharedState::new();
        shared.request_stop();
        shared.add_nodes(100);
        shared.reset();
        assert!(!shared.stopped());
        assert_eq!(shared.node_count(), 0);
    }

    /// The lookback bound is a `const` assertion at the definition, so this only has to
    /// confirm the stack can be indexed at both extremes without panicking.
    #[test]
    fn the_stack_can_be_indexed_at_both_extremes() {
        let stack = vec![StackEntry::default(); STACK_SIZE];
        assert!(stack.get(STACK_BASE - 4).is_some(), "the deepest lookback is in range");
        assert!(stack.get(STACK_BASE + MAX_PLY).is_some(), "the deepest ply is in range");
    }
}
