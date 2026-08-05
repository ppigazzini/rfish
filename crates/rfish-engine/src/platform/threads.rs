//! Lazy-SMP: run N workers over one root and take thread 0's answer.
//!
//! # Why there is no thread pool
//!
//! Upstream keeps a persistent pool because its workers hold raw pointers into shared
//! state and rebuilding that wiring per search would be expensive and error-prone. Here the
//! workers own their state and share only the transposition table and the atomic signals,
//! so [`std::thread::scope`] expresses the whole thing: it lends each thread a `&mut
//! SearchWorker` for exactly the duration of the search, joins them all before returning,
//! and the borrow checker proves no worker outlives the data it borrowed.
//!
//! The cost is one thread spawn per `go`, which is microseconds against a search measured
//! in seconds. What it buys is that "a worker used after the search ended" is not a bug
//! that can be written.
//!
//! # How the threads diverge
//!
//! Lazy-SMP does not partition the tree. Every thread searches the same root; they diverge
//! because they hit the shared transposition table in a different order and because the
//! helpers start at staggered depths. The gain comes from the table, not from a split.
//!
//! Golden: `Stockfish/src/thread.cpp`.

use std::sync::Arc;

use crate::board::position::Position;
use crate::board::types::{Move, VALUE_ZERO};
use crate::eval::nnue::Network;
use crate::search::timeman::TimeManagement;
use crate::search::tt::TranspositionTable;
use crate::search::worker::{InfoSink, SearchResult, SearchWorker, SilentSink};
use crate::state::{Limits, SearchOptions, SharedState};

/// A set of search threads and the state they keep between searches.
///
/// The histories survive from one `go` to the next — that is most of what makes the second
/// search of a game faster than the first — so the workers are owned here rather than
/// created per search.
pub struct ThreadPool {
    workers: Vec<SearchWorker>,
    shared: Arc<SharedState>,
    /// The clock model, which outlives one `go`: a `nodestime` budget is spent across the
    /// whole game, and the time-left factor is fixed on the first move and reused.
    tm: TimeManagement,
}

impl core::fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadPool").field("threads", &self.workers.len()).finish_non_exhaustive()
    }
}

impl ThreadPool {
    /// A pool of `n` workers.
    #[must_use]
    pub fn new(n: usize) -> ThreadPool {
        let shared = SharedState::new();
        let workers = (0..n.max(1)).map(|i| SearchWorker::new(i, Arc::clone(&shared))).collect();
        ThreadPool { workers, shared, tm: TimeManagement::default() }
    }

    /// Resize to `n` threads, keeping thread 0's histories.
    ///
    /// Thread 0's history is the one the next search benefits most from, so a `Threads`
    /// change mid-game does not throw away what the game has learned.
    pub fn resize(&mut self, n: usize) {
        let n = n.max(1);
        if n == self.workers.len() {
            return;
        }
        // A new worker inherits whatever network the pool is using, or `Threads 4` after
        // `EvalFile` would leave three workers evaluating with the classical fallback.
        let network = self.workers.first().and_then(SearchWorker::network);
        let tb = self.workers.first().and_then(SearchWorker::tablebases);
        self.workers.truncate(n);
        while self.workers.len() < n {
            let mut w = SearchWorker::new(self.workers.len(), Arc::clone(&self.shared));
            w.set_network(network.clone());
            w.set_tablebases(tb.clone());
            self.workers.push(w);
        }
    }

    /// How many threads the pool runs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// True when the pool has no workers, which [`ThreadPool::new`] makes impossible.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// The signals the shell uses to stop a running search.
    #[must_use]
    pub fn shared(&self) -> &Arc<SharedState> {
        &self.shared
    }

    /// Point every worker at `network`, or at none.
    ///
    /// One `Arc`, N workers: the weights are read-only for the whole search, so a clone per
    /// thread would cost 112 MiB each for nothing.
    pub fn set_network(&mut self, network: &Option<Arc<Network>>) {
        for w in &mut self.workers {
            w.set_network(network.clone());
        }
    }

    /// Point every worker at a tablebase registry, and at the option values that bound
    /// when it is consulted.
    pub fn set_tablebases(
        &mut self,
        tb: &Option<Arc<crate::platform::syzygy::TableRegistry>>,
        probe_depth: i32,
        probe_limit: u32,
        use_rule50: bool,
    ) {
        for w in &mut self.workers {
            w.set_tablebases(tb.clone());
            w.set_tb_limits(probe_depth, probe_limit, use_rule50);
        }
    }

    /// Forget every history on every thread. Called on `ucinewgame`.
    pub fn clear(&mut self) {
        for w in &mut self.workers {
            w.clear();
        }
        self.tm.clear();
    }

    /// Search `pos` with every thread and return thread 0's result.
    ///
    /// Only thread 0 reports through `sink`; the helpers run silently, because N threads
    /// all printing `info` lines would make the output unreadable and tell the GUI nothing
    /// it does not already get from thread 0.
    pub fn search(
        &mut self,
        pos: &Position,
        limits: &Limits,
        tt: &TranspositionTable,
        opts: &SearchOptions,
        sink: &mut dyn InfoSink,
    ) -> SearchResult {
        self.shared.reset();
        self.shared.set_searching_unbounded(limits.is_unbounded());
        tt.new_search();

        // The clock model is the pool's, not a worker's: `nodestime` spends a budget that
        // lasts the whole game, so it cannot live in state that is rebuilt per `go`. The
        // limits it hands back are the converted ones, and every worker searches those.
        let mut limits = limits.clone();
        let ply = limits.ply;
        self.tm.init(&mut limits, pos.side_to_move(), ply, opts);
        let budget = self.tm.budget();
        let limits = &limits;

        let n = self.workers.len();
        for w in &mut self.workers {
            w.set_thread_count(n);
        }

        let (main, helpers) = self.workers.split_at_mut(1);
        let main = &mut main[0];

        let (mut result, votes) = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(helpers.len());
            for w in helpers.iter_mut() {
                let (pos, limits) = (pos, limits);
                handles.push(
                    scope.spawn(move || w.search(pos, limits, tt, opts, budget, &mut SilentSink)),
                );
            }
            // The main thread searches on this thread rather than a spawned one, so a
            // single-threaded run has no thread involved at all.
            let r = main.search(pos, limits, tt, opts, budget, sink);
            // Every helper must see the stop before this scope can join them.
            self.shared.request_stop();
            let votes: Vec<SearchResult> =
                handles.into_iter().filter_map(|h| h.join().ok()).collect();
            (r, votes)
        });

        // Thread voting. The helpers searched the same root and may have found something
        // thread 0 did not, so the move played is the one the pool agrees on rather than the
        // one thread 0 happened to end on.
        //
        // With one thread there is nothing to vote on and this reduces to thread 0's answer,
        // which is what keeps `Threads 1` deterministic. `MultiPV` is excluded because the
        // caller asked for a ranked list, not a single answer. A strength handicap is
        // excluded for the same reason in reverse: it has already chosen a move on purpose,
        // and a vote would put the best one back.
        if !votes.is_empty() && opts.multi_pv == 1 && limits.depth.is_none() && !handicapped(opts) {
            result = elect(std::iter::once(result).chain(votes));
        }

        self.shared.set_searching_unbounded(false);
        // Drop the stop this search raised to bring its own helpers home, so the next one
        // does not inherit it and abort at depth zero. Only the search's OWN leftover is
        // cleared, and only once it is over: a `stop` that arrives before the next search
        // starts belongs to that next search, and dropping it here would be the very bug
        // this split exists to prevent. See `SharedState::clear_stop`.
        self.shared.clear_stop();
        result.nodes = self.shared.node_count().max(result.nodes);

        // Charge the game-long node budget for what this move spent, less the increment it
        // earned back by playing.
        if self.tm.uses_nodes_time() {
            let inc = limits.inc[pos.side_to_move().index()] as i64;
            self.tm.advance_nodes_time(result.nodes as i64 - inc);
        }

        result
    }
}

/// True when a strength handicap is choosing the move.
fn handicapped(opts: &SearchOptions) -> bool {
    crate::search::skill::Skill::new(opts.skill_level, opts.uci_elo).enabled()
}

/// Choose the move a set of finished searches agrees on.
///
/// Each candidate contributes `(score - worst + 14) * depth`, summed over the threads that
/// chose it. Two parts of that are load-bearing:
///
/// - **Offsetting by the worst score** keeps every weight non-negative, so a deeply searched
///   losing move cannot outvote a shallow winning one by sign alone.
/// - **The `+ 14` floor** is what makes AGREEMENT count. Without it, threads whose scores are
///   equal to the worst contribute nothing, and a lone slightly-higher score outvotes two
///   threads that agree. Upstream carries the same constant for the same reason.
fn elect(results: impl Iterator<Item = SearchResult>) -> SearchResult {
    let all: Vec<SearchResult> = results.filter(|r| r.best_move.is_ok()).collect();
    let Some(first) = all.first().cloned() else {
        return SearchResult {
            best_move: Move::NONE,
            ponder_move: None,
            score: VALUE_ZERO,
            depth: 0,
            nodes: 0,
        };
    };
    let min_score = all.iter().map(|r| r.score).min().unwrap_or(VALUE_ZERO);

    let mut tally: Vec<(Move, i64)> = Vec::new();
    for r in &all {
        let weight = i64::from(r.score - min_score + 14) * i64::from(r.depth.max(1));
        match tally.iter_mut().find(|(m, _)| *m == r.best_move) {
            Some((_, w)) => *w += weight,
            None => tally.push((r.best_move, weight)),
        }
    }

    let mut best = first;
    let mut best_weight = i64::MIN;
    for r in &all {
        let total = tally.iter().find(|(m, _)| *m == r.best_move).map_or(0, |(_, w)| *w);
        // Ties go to the deeper search, then to the higher score -- never to iteration
        // order, which would make the answer depend on how the threads happened to finish.
        if (total, r.depth, r.score) > (best_weight, best.depth, best.score) {
            best_weight = total;
            best = r.clone();
        }
    }
    best
}

impl Default for ThreadPool {
    fn default() -> ThreadPool {
        ThreadPool::new(1)
    }
}

/// How many threads the machine can usefully run, for the `Threads` option's upper bound.
#[must_use]
pub fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::movegen::generate_legal;
    use crate::board::position::START_FEN;
    use crate::board::types::Value;
    use std::time::Instant;

    fn opts(multi_pv: usize) -> SearchOptions {
        SearchOptions { multi_pv, ..SearchOptions::default() }
    }

    fn depth_limits(d: i32) -> Limits {
        Limits { depth: Some(d), start: Some(Instant::now()), ..Limits::default() }
    }

    #[test]
    fn a_single_threaded_pool_searches() {
        let mut pool = ThreadPool::new(1);
        let tt = TranspositionTable::new(8);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let r = pool.search(&pos, &depth_limits(6), &tt, &opts(1), &mut SilentSink);
        assert!(generate_legal(&pos).contains(&r.best_move));
    }

    /// The property Lazy-SMP has to hold: more threads must not produce an illegal move,
    /// must not deadlock, and must join cleanly.
    #[test]
    fn four_threads_agree_on_a_legal_move_and_join() {
        let mut pool = ThreadPool::new(4);
        let tt = TranspositionTable::new(16);
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq \
                                - 0 1",
            false,
        )
        .expect("valid");
        let r = pool.search(&pos, &depth_limits(7), &tt, &opts(1), &mut SilentSink);
        assert!(generate_legal(&pos).contains(&r.best_move));
        assert_eq!(pool.len(), 4);
        // The helpers' nodes are counted too, so the total exceeds thread 0's alone.
        assert!(r.nodes > 0);
    }

    #[test]
    fn a_stop_request_ends_an_infinite_search() {
        let mut pool = ThreadPool::new(2);
        let tt = TranspositionTable::new(8);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let shared = Arc::clone(pool.shared());
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            shared.request_stop();
        });
        let limits = Limits { infinite: true, start: Some(Instant::now()), ..Limits::default() };
        let r = pool.search(&pos, &limits, &tt, &opts(1), &mut SilentSink);
        stopper.join().expect("the stopper thread finishes");
        assert!(r.best_move.is_ok());
    }

    #[test]
    fn resizing_keeps_the_pool_usable() {
        let mut pool = ThreadPool::new(1);
        pool.resize(3);
        assert_eq!(pool.len(), 3);
        pool.resize(1);
        assert_eq!(pool.len(), 1);
        pool.resize(0);
        assert_eq!(pool.len(), 1, "a pool always has at least the main thread");

        let tt = TranspositionTable::new(4);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let r = pool.search(&pos, &depth_limits(4), &tt, &opts(1), &mut SilentSink);
        assert!(r.best_move.is_ok());
    }

    /// The vote must never invent a move: it can only choose among what the threads found.
    #[test]
    fn the_vote_chooses_among_the_candidates_and_prefers_agreement() {
        let mk = |m: u16, score: Value, depth: i32| SearchResult {
            best_move: Move::from_raw(m),
            ponder_move: None,
            score,
            depth,
            nodes: 0,
        };
        // Two threads agree on move 20 at a modest score; one prefers 30 slightly higher.
        // Agreement plus depth carries it.
        let winner = elect(
            [mk(20, Value::new(10), 12), mk(30, Value::new(12), 12), mk(20, Value::new(10), 12)]
                .into_iter(),
        );
        assert_eq!(winner.best_move, Move::from_raw(20));

        // A single deep, decisive thread beats two shallow dissenters.
        let winner = elect(
            [mk(40, Value::new(900), 20), mk(50, Value::new(0), 3), mk(60, Value::new(0), 3)]
                .into_iter(),
        );
        assert_eq!(winner.best_move, Move::from_raw(40));

        // One candidate: the vote is the identity.
        assert_eq!(elect([mk(7, Value::new(5), 9)].into_iter()).best_move, Move::from_raw(7));
        // No candidate: no move, rather than a panic.
        assert!(elect(std::iter::empty()).best_move.is_none());
    }

    /// `Threads 1` must stay deterministic: with no helpers there is nothing to vote on.
    #[test]
    fn one_thread_is_unaffected_by_the_vote() {
        let mut a = ThreadPool::new(1);
        let tt = TranspositionTable::new(8);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let first = a.search(&pos, &depth_limits(7), &tt, &opts(1), &mut SilentSink);

        let mut b = ThreadPool::new(1);
        let tt2 = TranspositionTable::new(8);
        let second = b.search(&pos, &depth_limits(7), &tt2, &opts(1), &mut SilentSink);
        assert_eq!(first.best_move, second.best_move);
        assert_eq!(first.score, second.score);
    }

    #[test]
    fn clear_forgets_the_histories_without_breaking_the_pool() {
        let mut pool = ThreadPool::new(2);
        let tt = TranspositionTable::new(4);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        pool.search(&pos, &depth_limits(5), &tt, &opts(1), &mut SilentSink);
        pool.clear();
        let r = pool.search(&pos, &depth_limits(5), &tt, &opts(1), &mut SilentSink);
        assert!(r.best_move.is_ok());
    }
}
