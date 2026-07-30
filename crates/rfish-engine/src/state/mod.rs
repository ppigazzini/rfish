//! The state blocks the search and the shell share.
//!
//! Nothing here searches or evaluates. These are the types that cross the zone boundary:
//! what the caller asked for ([`Limits`]), what a root move is worth ([`RootMove`]), and
//! the per-thread block a worker owns ([`crate::search::worker::SearchWorker`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::board::types::{Color, MAX_PLY, Move, VALUE_INFINITE, Value};

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
    #[must_use]
    pub fn uses_time_management(&self) -> bool {
        !self.infinite && self.move_time.is_none() && self.time.iter().any(Option::is_some)
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

/// The time budget for one move.
#[derive(Clone, Copy, Debug)]
pub struct TimeBudget {
    /// When the search started.
    pub start: Instant,
    /// The point past which a new iteration should not be started.
    pub optimum: Duration,
    /// The point past which the search stops even mid-iteration.
    pub maximum: Duration,
}

impl TimeBudget {
    /// How long the search has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// True when the hard limit has passed.
    #[must_use]
    pub fn out_of_time(&self) -> bool {
        self.elapsed() >= self.maximum
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
    fn a_clock_means_time_management_but_movetime_does_not() {
        let clock = Limits { time: [Some(60_000), Some(60_000)], ..Limits::default() };
        assert!(clock.uses_time_management());
        let fixed = Limits { move_time: Some(1000), ..clock };
        assert!(!fixed.uses_time_management());
        let infinite = Limits { infinite: true, ..Limits::default() };
        assert!(!infinite.uses_time_management());
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
