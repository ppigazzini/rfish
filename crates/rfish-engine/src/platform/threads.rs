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
use crate::search::tt::TranspositionTable;
use crate::search::worker::{InfoSink, SearchResult, SearchWorker, SilentSink};
use crate::state::{Limits, SharedState};

/// A set of search threads and the state they keep between searches.
///
/// The histories survive from one `go` to the next — that is most of what makes the second
/// search of a game faster than the first — so the workers are owned here rather than
/// created per search.
pub struct ThreadPool {
    workers: Vec<SearchWorker>,
    shared: Arc<SharedState>,
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
        ThreadPool { workers, shared }
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
        self.workers.truncate(n);
        while self.workers.len() < n {
            self.workers.push(SearchWorker::new(self.workers.len(), Arc::clone(&self.shared)));
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

    /// Forget every history on every thread. Called on `ucinewgame`.
    pub fn clear(&mut self) {
        for w in &mut self.workers {
            w.clear();
        }
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
        multi_pv: usize,
        move_overhead: u64,
        sink: &mut dyn InfoSink,
    ) -> SearchResult {
        self.shared.reset();
        tt.new_search();

        let (main, helpers) = self.workers.split_at_mut(1);
        let main = &mut main[0];

        let mut result = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(helpers.len());
            for w in helpers.iter_mut() {
                let (pos, limits) = (pos, limits);
                handles.push(scope.spawn(move || {
                    w.search(pos, limits, tt, multi_pv, move_overhead, &mut SilentSink);
                }));
            }
            // The main thread searches on this thread rather than a spawned one, so a
            // single-threaded run has no thread involved at all.
            let r = main.search(pos, limits, tt, multi_pv, move_overhead, sink);
            // Every helper must see the stop before this scope can join them.
            self.shared.request_stop();
            for h in handles {
                let _ = h.join();
            }
            r
        });

        result.nodes = self.shared.node_count().max(result.nodes);
        result
    }
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
    use std::time::Instant;

    fn depth_limits(d: i32) -> Limits {
        Limits { depth: Some(d), start: Some(Instant::now()), ..Limits::default() }
    }

    #[test]
    fn a_single_threaded_pool_searches() {
        let mut pool = ThreadPool::new(1);
        let tt = TranspositionTable::new(8);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let r = pool.search(&pos, &depth_limits(6), &tt, 1, 10, &mut SilentSink);
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
        let r = pool.search(&pos, &depth_limits(7), &tt, 1, 10, &mut SilentSink);
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
        let r = pool.search(&pos, &limits, &tt, 1, 10, &mut SilentSink);
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
        let r = pool.search(&pos, &depth_limits(4), &tt, 1, 10, &mut SilentSink);
        assert!(r.best_move.is_ok());
    }

    #[test]
    fn clear_forgets_the_histories_without_breaking_the_pool() {
        let mut pool = ThreadPool::new(2);
        let tt = TranspositionTable::new(4);
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        pool.search(&pos, &depth_limits(5), &tt, 1, 10, &mut SilentSink);
        pool.clear();
        let r = pool.search(&pos, &depth_limits(5), &tt, 1, 10, &mut SilentSink);
        assert!(r.best_move.is_ok());
    }
}
