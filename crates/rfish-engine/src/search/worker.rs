//! The search: iterative deepening, aspiration windows, alpha-beta and quiescence.
//!
//! # The worker owns everything it mutates
//!
//! A `SearchWorker` holds its own position, its own histories and its own stack. The only
//! things it shares are the transposition table and the stop/counter signals, and both are
//! atomic. That is what makes Lazy-SMP expressible without a lock and without `unsafe`:
//! `std::thread::scope` hands each thread a `&mut SearchWorker` it alone owns, plus a `&`
//! to the shared state, and the borrow checker proves the split.
//!
//! # Pointers become indices
//!
//! Upstream threads the search on raw pointers: `Stack* ss` walks an array that is indexed
//! from `-7`, `contHist[]` holds six pointers into the continuation tables, and `ss->pv`
//! is a pointer that may be null. None of those survive borrow checking while `&mut self`
//! is also live, so each becomes an index instead: `si` for the stack, a plane index for a
//! continuation table, and an explicit `pv_valid` flag for the null pointer. The arithmetic
//! is upstream's; only the addressing changed.
//!
//! # No I/O
//!
//! The search never prints. It reports through an [`InfoSink`], which the shell implements
//! by writing UCI `info` lines. That is what keeps the engine crate free of the transport
//! and lets a test drive a full search without capturing stdout.
//!
//! Golden: `Stockfish/src/search.cpp`.

use std::sync::Arc;
use std::time::Instant;

use crate::board::movegen::{generate_legal, has_legal_move, move_to_uci};
use crate::board::position::Position;
use crate::board::types::{
    Bound, Color, Expect, MAX_MOVES, MAX_PLY, Move, NodeKind, NonPv, Piece, PieceType, Ply, Pv,
    QuiescentKind, Root, Square, SquareOrNone, VALUE_DRAW, VALUE_INFINITE, VALUE_MATE,
    VALUE_MATE_IN_MAX_PLY, VALUE_NONE, VALUE_TB, VALUE_TB_LOSS_IN_MAX_PLY, VALUE_TB_WIN_IN_MAX_PLY,
    VALUE_ZERO, Value, is_decisive, is_loss, is_mate_or_mated, is_valid, is_win, mate_in, mated_in,
    piece_value,
};
use crate::eval;
use crate::eval::nnue::{Network, Scratch};
use crate::state::{
    Elapsed, Limits, MEAN_SQUARED_SENTINEL, RootMove, STACK_BASE, STACK_SIZE, SearchOptions,
    SharedState, StackEntry, StackIx, TimeBudget,
};

use super::history::{
    Bonus, CORRECTION_LIMIT, ContKey, CorrKey, Histories, InCheck, LOW_PLY_HISTORY_SIZE,
    WasCapture, cont_plane_index, corr_plane_index,
};
use super::movepick::{ContKeys, MoveBuf, MovePicker};
use super::score::Score;
use super::skill::{Prng, Skill};
use super::tt::{TranspositionTable, value_from_tt, value_to_tt};

/// Move-buffer slots per ply: the ordinary node, the singular re-entry, and quiescence.
const SLOTS_PER_PLY: usize = 3;
/// The slot an ordinary node at `ply` uses.
const SLOT_NODE: usize = 0;
/// The slot a singular search -- the same ply, one move excluded -- uses.
const SLOT_EXCLUDED: usize = 1;
/// The slot quiescence uses, so razoring out of a singular search cannot collide with the
/// ordinary node's list two levels up.
const SLOT_QSEARCH: usize = 2;

/// How many moves a node remembers for the end-of-node history update.
const SEARCHED_LIST_CAPACITY: usize = 32;

/// The node count past which the search starts emitting per-move progress.
///
/// Depth would be the obvious trigger and is the wrong one: a shallow depth can be reached
/// in milliseconds, and some GUIs hang when flooded with updates that fast.
const NODES_LIMIT_OUTPUT: u64 = 10_000_000;

/// The divisor the late-move reduction applies to a move's history score, by depth.
///
/// Indexed by `min(depth, 16) - 1`. The values are not monotonic, and that is not a
/// transcription error: they are fitted, and the shape they encode is what makes the same
/// history score mean different things at different depths.
const LMR_DIVISOR: [i32; 16] = [
    3637, 2787, 2761, 2939, 3171, 3347, 3147, 2762, 2772, 3106, 3107, 3060, 3112, 2991, 3090, 3542,
];

/// Interpolate `y0..y1` linearly over `x0..x1`, without clamping to the ends.
///
/// Upstream's `interpolate`. The callers clamp afterwards, and they clamp to a range that
/// is not the endpoints, so folding the clamp in here would change the answer.
fn interpolate(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    debug_assert!((x0 - x1).abs() > f64::EPSILON, "interpolating over an empty range");
    y0 + (y1 - y0) * (x - x0) / (x1 - x0)
}

/// A draw score with a bit of noise in it.
///
/// Two adjacent node counts give two different draw scores, which keeps the search from
/// treating every repetition as exactly equal and getting stuck comparing them. The noise
/// is one centipawn wide and derived from the node count, so it is deterministic.
#[inline]
fn value_draw(nodes: u64) -> Value {
    VALUE_DRAW - 1 + (nodes & 0x2) as i32
}

/// The depth below which futility pruning applies at a child node.
///
/// Upstream's `futility_depth`. The table holds the steps of the fitted expression
///
/// ```text
/// depth = 13 + int(0.5 + 6 / int(1 + pow(abs(eval) + abs(beta), 3) / 50_000_000_000))
/// ```
///
/// so the lookup IS that formula rather than an approximation of it. The cutoff falls from 19
/// toward 13 as the scores in play grow, which is what lets a mate search keep descending
/// where the fixed cutoff it replaces would have pruned it away. Upstream says plainly that
/// this is not a tuning knob.
///
/// The last entry is twice `VALUE_INFINITE`, which exceeds any `prob` the callers admit — they
/// test `is_loss(beta)` and `is_win(eval)` first — so the scan stops on the table rather than
/// at its end.
#[inline]
fn futility_depth(eval: Value, beta: Value) -> i32 {
    const LUT: [i32; 7] = [1657, 2555, 3294, 4122, 5314, 8194, VALUE_INFINITE.get() * 2];
    let prob = eval.abs().get() + beta.abs().get();
    19 - LUT.iter().take_while(|&&step| step < prob).count() as i32
}

/// The correction-history bonus a multi-cut records (upstream `c5aef2bf1`'s predecessor).
///
/// A fail high above the static evaluation is evidence the evaluation was low, and how much
/// evidence depends on the depth it came from — the SINGULAR depth, which is where the
/// search happened, not the depth of the node handing the result back.
///
/// A free function rather than a closure over the stack, so the arithmetic can be pinned by
/// a test at the boundaries it turns on: a node count moves when any term here changes but
/// cannot say WHICH term moved, and cannot tell a transcription slip from an intended
/// retune at all.
#[inline]
fn multicut_correction_bonus(value: Value, static_eval: Value, singular_depth: i32) -> Bonus {
    Bonus::new(
        ((value - static_eval) * singular_depth * 177 / 1024)
            .clamp(-CORRECTION_LIMIT / 4, CORRECTION_LIMIT / 4),
    )
}

/// How the search reports progress.
///
/// Implemented by the shell as UCI `info` lines, and by tests as a no-op or a recorder.
///
/// Deliberately NOT `Send`. Only the main thread reports, and it does so on the caller's
/// own thread inside [`std::thread::scope`], so requiring `Send` would exclude a
/// `StdoutLock` -- the one type every shell actually wants to write through.
pub trait InfoSink {
    /// One `info` line's worth of search progress.
    fn depth_finished(&mut self, report: &DepthReport<'_>);
    /// A `currmove` update: the root move now being searched, and its number in the root
    /// list. The default does nothing, because a caller that only wants completed
    /// iterations should not have to say so.
    fn current_move(&mut self, _mv: &str, _number: usize, _depth: i32) {}
    /// The report for a root with NO legal moves.
    ///
    /// Its own hook rather than a [`DepthReport`], because upstream prints a shorter line
    /// here -- a depth and a score, with no seldepth, nodes or PV -- and a GUI told that a
    /// mated position was searched to depth 0 with an empty PV would be told something
    /// different from what upstream tells it.
    fn no_moves(&mut self, _depth: i32, _score: &Score) {}
}

/// A no-op sink, for a search whose output nobody reads.
#[derive(Debug, Default)]
pub struct SilentSink;

impl InfoSink for SilentSink {
    fn depth_finished(&mut self, _report: &DepthReport<'_>) {}
}

/// How a node hands out the root move it is about to search.
///
/// **This exists to cost nothing below the root, and that was measured rather than
/// assumed.** Upstream announces from inside the node body by naming a global
/// (`main_manager()->updates`) under `if (rootNode && ...)`, where `rootNode` is a template
/// constant -- so the whole thing is deleted from every instantiation but the root's, and
/// non-root nodes pay zero. Safe Rust has no global to name, so the reporter has to travel
/// down the recursion, and travelling as a live pointer is NOT free: threading one through
/// [`SearchWorker::node`] measured **+0.028%** on `perf-budget`, 423k instructions over a
/// 163k-node profile, against a tolerance of 0.005%. Hoisting it into a local was worse
/// still, at +0.041%.
///
/// So the pointer travels only where it can be used. [`Root`] carries the sink; every other
/// kind carries `()`, whose `announce` is empty and which is zero-sized -- Rust passes no
/// register and no stack slot for it, and the call inlines to nothing. That is upstream's
/// `constexpr` elision, spelled in the type system.
trait Announce {
    fn announce(&mut self, mv: &str, number: usize, depth: i32);
}

impl Announce for () {
    #[inline(always)]
    fn announce(&mut self, _mv: &str, _number: usize, _depth: i32) {}
}

impl Announce for &mut dyn InfoSink {
    #[inline(always)]
    fn announce(&mut self, mv: &str, number: usize, depth: i32) {
        (**self).current_move(mv, number, depth);
    }
}

/// Which announcer a node kind carries.
///
/// A second trait rather than a field on [`NodeKind`], because `NodeKind` lives in the board
/// zone and the sink lives here: the search may name the board's types, and not the reverse.
trait Announces: NodeKind {
    type Announcer<'a>: Announce;
}

impl Announces for Root {
    type Announcer<'a> = &'a mut dyn InfoSink;
}

impl Announces for Pv {
    type Announcer<'a> = ();
}

impl Announces for NonPv {
    type Announcer<'a> = ();
}

/// Everything one completed iteration has to report.
#[derive(Debug)]
pub struct DepthReport<'a> {
    pub depth: i32,
    pub sel_depth: i32,
    pub multi_pv: usize,
    pub score: Score,
    /// Win, draw and loss chances in per mille, for `UCI_ShowWDL`.
    pub wdl: [i32; 3],
    /// True when the score is a lower bound (fail high), false for an upper bound; `None`
    /// for an exact score.
    pub bound: Option<bool>,
    pub nodes: u64,
    pub nps: u64,
    pub hashfull: u32,
    pub tb_hits: u64,
    pub time_ms: u64,
    pub pv: &'a [String],
}

/// What a finished search produced.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub best_move: Move,
    /// The move the engine expects in reply, for `bestmove ... ponder ...`.
    pub ponder_move: Option<Move>,
    pub score: Value,
    pub depth: i32,
    pub nodes: u64,
}

/// One search thread's private state.
pub struct SearchWorker {
    /// Thread index. Thread 0 is the one that reports and whose result is played.
    pub id: usize,
    pos: Position,
    histories: Box<Histories>,
    /// A boxed ARRAY, not a `Vec`. The length is then a compile-time constant, so every
    /// `self.stack[si - k]` bounds check compares against an immediate instead of loading
    /// the length -- and LLVM can fold the checks of a node's several accesses into one. The
    /// profile put 12.8M instructions of slice-index machinery inside `node` alone.
    stack: Box<[StackEntry; STACK_SIZE]>,
    root_moves: Vec<RootMove>,
    shared: Arc<SharedState>,
    limits: Limits,
    opts: SearchOptions,
    budget: TimeBudget,
    nodes: u64,
    tb_hits: u64,
    /// How much of `nodes`/`tb_hits` this worker has already contributed to the pool.
    ///
    /// The counters a GUI is shown, and the ones a `nodes` limit is enforced against, are
    /// upstream's POOL totals: `ThreadPool::nodes_searched` sums every worker's own atomic.
    /// Keeping the hot counter a plain field and publishing the DELTA periodically gets the
    /// same number without an atomic per node.
    published_nodes: u64,
    published_tb_hits: u64,
    sel_depth: i32,
    root_depth: i32,
    completed_depth: i32,
    /// The window width of the current root search, which the reduction formula scales by.
    root_delta: i32,
    pv_index: usize,
    /// One past the last root move sharing the current tablebase rank.
    pv_last: usize,
    multi_pv: usize,
    /// The ply below which null-move pruning is disabled during a verification search.
    nmp_min_ply: Ply,
    /// The previous iteration's principal variation for the current PV slot, which the
    /// follow-PV test compares against.
    last_iteration_pv: Vec<Move>,
    /// How often this thread's root best move changed, pooled by the time manager.
    best_move_changes: f64,
    /// The log-scaled reduction table, indexed by depth and by move number.
    /// A boxed ARRAY: `reduction` reads it twice per move searched, and a `Vec` makes each
    /// read load a length. `MAX_MOVES` is the bound upstream sizes it to.
    reductions: Box<[i32; MAX_MOVES]>,
    /// One reusable move buffer per (ply, kind) slot, allocated once and never freed.
    ///
    /// Keyed by kind as well as ply because the search RE-ENTERS a ply twice over: a
    /// singular search re-runs the same ply with a move excluded while the outer node is
    /// still walking its own move list, and razoring drops into quiescence at the same ply
    /// from inside that singular search. One buffer per ply would let the inner picker
    /// overwrite the outer's list mid-iteration. Neither re-entry can nest further --
    /// singular search requires no excluded move -- so three slots per ply are enough.
    /// A boxed ARRAY, for the same reason as `stack`: the slot count is a constant, so the
    /// per-node lookup compares against an immediate.
    move_pool: Box<[MoveBuf; SLOTS_PER_PLY * STACK_SIZE]>,
    /// How many nodes remain before the next clock check.
    calls_cnt: i32,
    /// How many threads share the root, for the pooled instability term.
    thread_count: usize,
    optimism: [Value; 2],
    /// The stability factor the previous move settled on, carried into this one.
    previous_time_reduction: f64,
    /// The previous move's root score, for the falling-eval term.
    best_previous_score: Value,
    /// The previous move's averaged root score, for the same term.
    best_previous_average_score: Value,
    /// The last four iterations' scores, for the same term over a longer window.
    iter_value: [Value; 4],
    /// The network, shared read-only across every thread. `None` runs the classical
    /// fallback, which is what a run with no net on disk does.
    network: Option<Arc<Network>>,
    /// Per-thread evaluation buffers, so a search allocates nothing per node.
    scratch: Scratch,
    /// The tablebases, shared read-only across every thread.
    tablebases: Option<Arc<crate::platform::syzygy::TableRegistry>>,
    tb_probe_depth: i32,
    tb_probe_limit: u32,
    tb_use_rule50: crate::platform::syzygy::Rule50,
    /// The piece count the in-search probe is still willing to answer at. Zero once the
    /// root ranking has settled the game and further probing would only cost time.
    tb_cardinality: u32,
    /// True when the root itself is a tablebase position, so the reported score is a
    /// tablebase fact rather than a search result.
    root_in_tb: bool,
}

impl core::fmt::Debug for SearchWorker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchWorker")
            .field("id", &self.id)
            .field("root_depth", &self.root_depth)
            .field("nodes", &self.nodes)
            .finish_non_exhaustive()
    }
}

impl SearchWorker {
    /// A worker with upstream's starting histories, ready for a `go`.
    #[must_use]
    pub fn new(id: usize, shared: Arc<SharedState>) -> SearchWorker {
        let mut w = SearchWorker {
            id,
            pos: Position::startpos(),
            histories: Box::default(),
            stack: match vec![StackEntry::default(); STACK_SIZE].into_boxed_slice().try_into() {
                Ok(b) => b,
                // Built with exactly STACK_SIZE elements, so the conversion cannot fail.
                // A match rather than `expect`, because the error type is the boxed slice.
                Err(_) => unreachable!("the vec was built with exactly STACK_SIZE items"),
            },
            root_moves: Vec::new(),
            shared,
            limits: Limits::default(),
            opts: SearchOptions::default(),
            budget: TimeBudget::default(),
            nodes: 0,
            tb_hits: 0,
            sel_depth: 0,
            root_depth: 0,
            completed_depth: 0,
            root_delta: 0,
            pv_index: 0,
            pv_last: 0,
            multi_pv: 1,
            nmp_min_ply: Ply::ROOT,
            last_iteration_pv: Vec::new(),
            best_move_changes: 0.0,
            reductions: match vec![0; MAX_MOVES].into_boxed_slice().try_into() {
                Ok(b) => b,
                // Built with exactly MAX_MOVES elements, so this cannot fail.
                Err(_) => unreachable!("the vec was built with exactly MAX_MOVES items"),
            },
            move_pool: match (0..SLOTS_PER_PLY * STACK_SIZE)
                .map(|_| MoveBuf::new())
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .try_into()
            {
                Ok(b) => b,
                // Built with exactly that many elements, so this cannot fail.
                Err(_) => unreachable!("the vec was built with exactly the slot count"),
            },
            calls_cnt: 0,
            published_nodes: 0,
            published_tb_hits: 0,
            thread_count: 1,
            optimism: [VALUE_ZERO; 2],
            previous_time_reduction: 0.85,
            best_previous_score: VALUE_INFINITE,
            best_previous_average_score: VALUE_INFINITE,
            iter_value: [VALUE_ZERO; 4],
            network: None,
            scratch: Scratch::default(),
            tablebases: None,
            tb_probe_depth: 1,
            tb_probe_limit: 7,
            tb_use_rule50: crate::platform::syzygy::Rule50::Apply,
            tb_cardinality: 0,
            root_in_tb: false,
        };
        w.clear();
        w
    }

    /// Point this worker at a tablebase registry, or at none.
    pub fn set_tablebases(&mut self, tb: Option<Arc<crate::platform::syzygy::TableRegistry>>) {
        self.tablebases = tb;
    }

    /// The registry this worker is using, if any.
    #[must_use]
    pub fn tablebases(&self) -> Option<Arc<crate::platform::syzygy::TableRegistry>> {
        self.tablebases.clone()
    }

    /// How deep a node must be before it is worth probing, and how many pieces a table
    /// covers. Both come from UCI options.
    pub fn set_tb_limits(
        &mut self,
        probe_depth: i32,
        probe_limit: u32,
        use_rule50: crate::platform::syzygy::Rule50,
    ) {
        self.tb_probe_depth = probe_depth;
        self.tb_probe_limit = probe_limit;
        self.tb_use_rule50 = use_rule50;
    }

    /// Point this worker at a network, or at none.
    ///
    /// Shared by `Arc`: the weights are around 112 MiB and read-only for the whole search,
    /// so every thread points at one copy. Upstream replicates them per NUMA node instead;
    /// see `docs/04-multithreading.md`.
    pub fn set_network(&mut self, network: Option<Arc<Network>>) {
        self.network = network;
    }

    /// The network this worker is using, if any.
    #[must_use]
    pub fn network(&self) -> Option<Arc<Network>> {
        self.network.clone()
    }

    /// How many nodes this worker has searched.
    #[must_use]
    pub fn nodes(&self) -> u64 {
        self.nodes
    }

    /// Tell this worker how many threads share the root, for the instability term.
    pub fn set_thread_count(&mut self, n: usize) {
        self.thread_count = n.max(1);
    }

    /// Whether this worker announces the root move it is about to search.
    ///
    /// Both clauses are upstream's, and each is load-bearing on its own: `is_mainthread()`
    /// keeps N searches from interleaving into one stream of `currmove` lines, and the node
    /// count keeps a fast iteration from emitting thousands of them a second. The caller
    /// tests `N::ROOT` first, so nothing below the root pays for this.
    #[inline]
    fn announces_root_moves(&self) -> bool {
        self.id == 0 && self.nodes > NODES_LIMIT_OUTPUT
    }

    /// Reset the histories and the reduction table. Called on `ucinewgame`, never mid-game.
    pub fn clear(&mut self) {
        self.histories.clear();
        // `reductions[i] = 2872/128 * ln(i)`, evaluated in floating point and truncated,
        // exactly as upstream does. Computing it in integers instead would round
        // differently at several depths and move every reduction that reads them.
        self.reductions[0] = 0;
        for i in 1..self.reductions.len() {
            self.reductions[i] = (2872.0 / 128.0 * (i as f64).ln()) as i32;
        }
        self.previous_time_reduction = 0.85;
        self.best_previous_score = VALUE_INFINITE;
        self.best_previous_average_score = VALUE_INFINITE;
    }

    /// The static evaluation of the worker's current position.
    #[inline]
    fn evaluate(&mut self, ply: Ply) -> Value {
        let optimism = self.optimism[self.pos.side_to_move().index()];
        let ply = ply.max(Ply::ROOT);
        eval::evaluate(&self.pos, self.network.as_deref(), ply, &mut self.scratch, optimism)
    }

    // -- the stack ---------------------------------------------------------

    /// Make `mv`, recording everything the child will read back off the stack.
    ///
    /// This is where the node counter advances. Upstream counts MOVES MADE, not nodes
    /// entered, and the two differ: a node that returns before its move loop — a
    /// transposition cutoff, a draw, a stand-pat — costs nothing. Counting entries instead
    /// would inflate the bench by every such node.
    // Always: zfish has no separate wrapper here at all -- the work is inline in
    // `searchImpl`, where the stack entry being written is already addressed.
    #[inline(always)]
    fn do_move(&mut self, mv: Move, gives_check: bool, si: StackIx) {
        let capture = self.pos.is_capture_stage(mv);
        let moved = self.pos.moved_piece(mv);
        let in_check = self.stack[si.index()].in_check;
        self.nodes += 1;
        // Every 1024 nodes, and on every worker rather than only the one that reports:
        // a helper that never publishes leaves the pool total reading as if it had searched
        // nothing. The cadence is the same order as the countdown `check_time` already runs
        // on, so the total is as fresh as upstream's -- which is also read while the other
        // threads are still incrementing.
        if self.nodes.is_multiple_of(1024) {
            self.publish_counters();
        }
        // Record what the child's accumulator will roll forward from. Split the borrow by
        // FIELD: the record list belongs to the scratch and the pieces to the position.
        let ply = si.ply().next();
        #[cfg(not(feature = "eval-material"))]
        {
            let dts = self.scratch.record_threats(ply);
            let dpp = self.pos.do_move_recording(mv, gives_check, Some(dts));
            self.scratch.record(ply, &self.pos, self.pos.raw_key(), dpp);
        }
        // MEASUREMENT HARNESS, the mirror of `oracle --spine`'s `patch_out_threat_scan`: the
        // threat records and the accumulator slot are read only by the NNUE evaluation, which
        // a material build never runs. Stubbing the oracle's half and not this one charges
        // rfish for bookkeeping upstream is excused from, which is the asymmetry the spine
        // ratio is there to avoid. The slot write STAYS on both sides -- upstream's stub
        // leaves its own `DirtyPiece` in place.
        #[cfg(feature = "eval-material")]
        {
            let dpp = self.pos.do_move_recording(mv, gives_check, None);
            self.scratch.record(ply, &self.pos, self.pos.raw_key(), dpp);
        }
        self.stack[si.index()].current_move = mv;
        self.stack[si.index()].continuation =
            cont_plane_index(InCheck::of(in_check), WasCapture::of(capture), moved, mv.to());
        self.stack[si.index()].continuation_correction = corr_plane_index(moved, mv.to());
    }

    fn undo_move(&mut self, mv: Move) {
        self.pos.undo_move(mv);
    }

    /// Pass the move. The continuation planes fall back to the sentinel, which no real
    /// move can select and which is therefore never written.
    fn do_null_move(&mut self, si: StackIx) {
        self.pos.do_null_move();
        // A null move moves no piece, so it changes no threat and no pawn pair: the child
        // rolls forward from the parent with an EMPTY delta rather than refreshing.
        self.scratch.record_null(si.ply().next(), &self.pos, self.pos.raw_key());
        self.stack[si.index()].current_move = Move::NULL;
        // The sentinel plane, by its name rather than by recomputing it. A move by
        // `Piece::NONE` to a1 that is neither in check nor a capture indexes plane zero,
        // which is what `UNREAD` already is.
        self.stack[si.index()].continuation = ContKey::UNREAD;
        self.stack[si.index()].continuation_correction = CorrKey::UNREAD;
    }

    fn undo_null_move(&mut self) {
        self.pos.undo_null_move();
    }

    // -- correction history ------------------------------------------------

    /// How far the static evaluation of positions like this one has historically been from
    /// what the search found.
    ///
    /// Five terms keyed by five different summaries of the position — the pawn structure,
    /// the minor-piece configuration, each side's non-pawn material, and the pair of moves
    /// that led here. A position the evaluation systematically misjudges is one every term
    /// has an opinion about.
    ///
    /// The continuation term falls back to a large constant when there is no previous move
    /// to key on. That constant is not a neutral zero: it is what the sum looks like when
    /// the other four terms have nothing to say, and it biases the correction the same way
    /// a typical continuation entry would.
    // Always, not a hint: zfish folds its correction read into `searchImpl`, and the caller
    // already holds the state this walks.
    #[inline(always)]
    fn correction_value(&self, si: StackIx) -> i32 {
        let us = self.pos.side_to_move();
        let st = self.pos.st();
        let h = &self.histories;
        let pcv = h.correction.pawn(st.pawn_key, us);
        let micv = h.correction.minor(st.minor_piece_key, us);
        let wnpcv = h.correction.non_pawn(self.pos.non_pawn_key(Color::White), us);
        let bnpcv = h.correction.non_pawn(self.pos.non_pawn_key(Color::Black), us);

        let m = self.stack[si.back(1).index()].current_move;
        let cntcv = if m.is_ok() {
            let to = m.to();
            let pc = self.pos.piece_on(to);
            8761 * (h.continuation_correction.get(
                self.stack[si.back(2).index()].continuation_correction,
                pc,
                to,
            ) + h.continuation_correction.get(
                self.stack[si.back(4).index()].continuation_correction,
                pc,
                to,
            ))
        } else {
            64049
        };

        15341 * pcv + 10569 * micv + 12906 * (wnpcv + bnpcv) + cntcv
    }

    /// Apply the correction to a raw static evaluation.
    ///
    /// The clamp keeps it out of the tablebase range: a corrected evaluation that reached
    /// it would be read as a proven result rather than an estimate.
    #[inline]
    fn to_corrected_static_eval(v: Value, cv: i32) -> Value {
        (v + cv / 131_072).clamp(VALUE_TB_LOSS_IN_MAX_PLY + 1, VALUE_TB_WIN_IN_MAX_PLY - 1)
    }

    /// Record how far the static evaluation was from what the search found.
    fn update_correction_history(&mut self, si: StackIx, bonus: Bonus) {
        // Material changes far less often than structure, so an error attributed to it is
        // weaker evidence and is recorded at a lower weight.
        const NON_PAWN_WEIGHT: i32 = 186;

        let us = self.pos.side_to_move();
        let (pawn_key, minor_key, np) = (
            self.pos.st().pawn_key,
            self.pos.st().minor_piece_key,
            [self.pos.non_pawn_key(Color::White), self.pos.non_pawn_key(Color::Black)],
        );

        let h = &mut self.histories;
        super::history::update_correction_entry(h.correction.pawn_mut(pawn_key, us), bonus);
        super::history::update_correction_entry(
            h.correction.minor_mut(minor_key, us),
            bonus * 150 / 128,
        );
        super::history::update_correction_entry(
            h.correction.non_pawn_mut(np[0], us),
            bonus * NON_PAWN_WEIGHT / 128,
        );
        super::history::update_correction_entry(
            h.correction.non_pawn_mut(np[1], us),
            bonus * NON_PAWN_WEIGHT / 128,
        );

        let m = self.stack[si.back(1).index()].current_move;
        if m.is_ok() {
            let to = m.to();
            let pc = self.pos.piece_on(to);
            let (p2, p4) = (
                self.stack[si.back(2).index()].continuation_correction,
                self.stack[si.back(4).index()].continuation_correction,
            );
            self.histories.continuation_correction.update(p2, pc, to, bonus * 130 / 128);
            self.histories.continuation_correction.update(p4, pc, to, bonus * 70 / 128);
        }
    }

    // -- move-ordering history ---------------------------------------------

    /// The per-ply continuation bonus, at the width upstream computes it at.
    ///
    /// **The product WRAPS, and the wrap is upstream's.** `weight` reaches 1040 and `multiplier`
    /// 126, so any `|bonus|` above ~16,400 puts `bonus * weight * multiplier` past `i32`, and the
    /// fail-low caller reaches it: `scaled_bonus * 263 / 16384` passes 30,000 once the histories
    /// saturate. Upstream computes all three in `int`, a release build wraps, and the intended
    /// bonus is applied at nearly full magnitude with the OPPOSITE sign. Spelled here rather than
    /// inherited from the profile, because the gate profile's overflow checks turn the same input
    /// into a panic while release wraps silently — the exact split the profile exists to catch.
    ///
    /// The bench never reaches it, so no gated number depends on the wrap; a search on a real
    /// clock does. Widening to `i64` would remove the sign flip and diverge from the golden on
    /// every input that overflows, which is why the width is reproduced rather than repaired.
    #[inline(always)]
    fn continuation_delta(bonus: Bonus, weight: i32, multiplier: i32) -> Bonus {
        Bonus::new(bonus.get().wrapping_mul(weight).wrapping_mul(multiplier) / 131_072)
    }

    /// Reward or punish the follow-ups to the moves played one to six plies back.
    ///
    /// The multiplier grows with how many of those planes already rate this follow-up
    /// positively: a move several previous contexts agree about is stronger evidence than
    /// one only the immediate parent likes.
    fn update_continuation_histories(&mut self, si: StackIx, pc: Piece, to: Square, bonus: Bonus) {
        const CMHC_MULTIPLIERS: [i32; 7] = [94, 103, 110, 106, 119, 126, 121];

        let in_check = self.stack[si.index()].in_check;
        let mut positive_count = 0usize;
        let histories = &mut self.histories;
        // Written out per ply rather than looped over the table above. The loop did not
        // unroll -- its `break` and `continue` kept `i` a runtime value -- so the frame
        // lookup stayed a bounds-checked index and cost 33 instructions per call on its own,
        // where mcfish and zfish both reach the frame by constant pointer arithmetic. With
        // the ply a literal, `si - $i` is a constant offset and `in_check && $i > 2` folds
        // to `in_check` or to `false`.
        //
        // Skipping each late ply individually is the same thing as breaking out of the loop
        // at the first one: `in_check` does not change within a call.
        macro_rules! ply_back {
            ($i:literal, $weight:literal) => {{
                if !(in_check && $i > 2) {
                    let entry = &self.stack[si.back($i).index()];
                    if entry.current_move.is_ok() {
                        let plane = entry.continuation;
                        if histories.continuation.get(plane, pc, to) > 0 {
                            positive_count += 1;
                        }
                        let multiplier = CMHC_MULTIPLIERS[positive_count];
                        let delta = Self::continuation_delta(bonus, $weight, multiplier)
                            + 73 * i32::from($i < 2);
                        histories.continuation.update(plane, pc, to, delta);
                    }
                }
            }};
        }
        // In check only the two nearest plies are updated: the rest of the line was forced,
        // so crediting it teaches the ordering nothing.
        ply_back!(1, 1040);
        ply_back!(2, 780);
        ply_back!(3, 290);
        ply_back!(4, 502);
        ply_back!(5, 132);
        ply_back!(6, 418);
    }

    /// Reward or punish a quiet move across every table that orders quiet moves.
    fn update_quiet_histories(&mut self, si: StackIx, mv: Move, bonus: Bonus) {
        let us = self.pos.side_to_move();
        let ply = si.ply();
        let pc = self.pos.moved_piece(mv);
        let to = mv.to();
        let pawn_row = super::history::PawnHistory::row(self.pos.st().pawn_key);

        self.histories.main.update(us, mv.raw(), bonus);
        if ply.index() < LOW_PLY_HISTORY_SIZE {
            self.histories.low_ply.update(ply, mv.raw(), bonus * 712 / 1024);
        }
        self.update_continuation_histories(si, pc, to, bonus * 750 / 1024);
        // A malus is applied to the pawn table at less than half the weight of a bonus:
        // the pawn structure is weak evidence that a move was BAD, and punishing it at
        // full weight blanks the table.
        let pawn_bonus = bonus * if bonus > -4 { 1104 } else { 459 } / 1024;
        self.histories.pawn.update(pawn_row, pc, to, pawn_bonus);
    }

    /// Update every table at the end of a node that found a best move.
    #[allow(clippy::too_many_arguments)]
    fn update_all_stats(
        &mut self,
        si: StackIx,
        best_move: Move,
        prev_sq: SquareOrNone,
        quiets_searched: &[Move],
        captures_searched: &[Move],
        depth: i32,
        tt_move: Move,
        pv_node: bool,
    ) {
        let moved_piece = self.pos.moved_piece(best_move);

        let mut bonus = Bonus::new((133 * depth - 81).min(1487))
            + 364 * i32::from(best_move == tt_move)
            + self.stack[si.back(1).index()].stat_score / 28;
        let malus = Bonus::new((968 * depth - 235).min(2244));

        if !pv_node {
            // A node that searched many moves before finding its best one has stronger
            // evidence for that move than one that found it first.
            //
            // Upstream multiplies an `int` by a `u64`, which in C++ converts the int to
            // unsigned. For a NEGATIVE bonus that is not a no-op: the division then floors
            // instead of truncating toward zero, so the result is one lower whenever the
            // product does not divide exactly. Reproduced here rather than corrected,
            // because "corrected" is a different engine — the off-by-one propagates into
            // every history table and moves the node count.
            let searched = (quiets_searched.len() + captures_searched.len()) as u64;
            let scaled = (bonus.get() as i64 as u64).wrapping_mul(searched) / 256;
            bonus = Bonus::new(bonus.get().wrapping_add(scaled as u32 as i32));
        }

        if self.pos.is_capture_stage(best_move) {
            let captured = self.pos.piece_on(best_move.to()).piece_type();
            self.histories.captures.update(
                moved_piece,
                best_move.to(),
                captured,
                bonus * 1427 / 1024,
            );
        } else {
            self.update_quiet_histories(si, best_move, bonus * 899 / 1024);

            // The malus DECAYS across the list: the moves searched first were the
            // best-ordered, so failing is weaker evidence against them than against the
            // ones the ordering had already given up on.
            let mut actual_malus = malus * 1159 / 1024;
            for &mv in quiets_searched {
                actual_malus = actual_malus * 921 / 1024;
                self.update_quiet_histories(si, mv, -actual_malus);
            }
        }

        // An early quiet move at the previous ply that was not its transposition move, and
        // that this node just refuted, is punished there rather than here.
        if let Some(prev_sq) = prev_sq.square()
            && self.stack[si.back(1).index()].move_count
                == 1 + i32::from(self.stack[si.back(1).index()].tt_hit)
            && self.pos.captured_piece().is_none()
        {
            let pc = self.pos.piece_on(prev_sq);
            self.update_continuation_histories(si.back(1), pc, prev_sq, -malus * 713 / 1024);
        }

        for &mv in captures_searched {
            let pc = self.pos.moved_piece(mv);
            let captured = self.pos.piece_on(mv.to()).piece_type();
            self.histories.captures.update(pc, mv.to(), captured, -malus * 1489 / 1024);
        }
    }

    /// The base reduction for a move, in 1024ths of a ply.
    ///
    /// Both factors are logarithmic, so the table is multiplicative rather than additive:
    /// the hundredth move at depth 20 is far less likely to matter than the third at depth
    /// 4, and no linear rule can be right at both ends.
    ///
    /// `window` is `(beta - alpha) * 577 / root_delta`, which the caller computes rather
    /// than this function: see [`Self::reduction_window`].
    fn reduction(&self, improving: bool, d: i32, mn: i32, window: i32) -> i32 {
        let scale = self.reductions[d as usize] * self.reductions[mn as usize];
        // The entries are `int(2872 / 128 * ln(i))`, 0 through 124, so the product and the
        // improving term are non-negative -- which the type does not say, so the signed
        // `/ 512` would carry the round-toward-zero correction. Dividing at `u32` is the
        // same quotient for a non-negative dividend and is the shift alone.
        debug_assert!(scale >= 0, "the reduction table is a scaled logarithm, never negative");
        let improving_term = (i32::from(!improving) * scale * 197) as u32 / 512;
        scale - window + improving_term as i32 + 982
    }

    /// The window term [`Self::reduction`] subtracts, for the window `alpha..beta`.
    ///
    /// Held by the move loop rather than recomputed per move. `root_delta` is fixed for the
    /// whole search and `beta` does not move inside `search`, so the quotient can change
    /// only where `alpha` is raised -- the single assignment in step 20. At a non-PV node
    /// `alpha` is `beta - 1` and cannot be raised at all, so there the divide ran once per
    /// move to produce the same constant every time.
    fn reduction_window(&self, alpha: Value, beta: Value) -> i32 {
        (beta - alpha) * 577 / self.root_delta.max(1)
    }

    /// True when `mv` walks a piece back and forth without progress.
    ///
    /// Shuffling makes the tree explode: the position barely changes, so nothing prunes,
    /// and the search spends its depth going nowhere. The test looks for the move that
    /// completes a two-ply cycle. Its parameters are deliberately untuned upstream, and
    /// tuning them here would be an improvement rather than a port.
    fn is_shuffling(&self, mv: Move, si: StackIx) -> bool {
        if self.pos.is_capture_stage(mv) || self.pos.rule50_count() < 10 {
            return false;
        }
        if self.pos.st().plies_from_null < 6 || si.ply() < Ply::new(20) {
            return false;
        }
        mv.from() == self.stack[si.back(2).index()].current_move.to()
            && self.stack[si.back(2).index()].current_move.from()
                == self.stack[si.back(4).index()].current_move.to()
    }

    // -- the driver --------------------------------------------------------

    /// Run one search to completion.
    ///
    /// Returns the best move and its score. Every thread runs this; only thread 0's return
    /// value is played, and only thread 0 reports through `sink`.
    pub fn search(
        &mut self,
        pos: &Position,
        limits: &Limits,
        tt: &TranspositionTable,
        opts: &SearchOptions,
        budget: TimeBudget,
        sink: &mut dyn InfoSink,
    ) -> SearchResult {
        self.pos = pos.clone();
        self.limits = limits.clone();
        self.opts = *opts;
        self.budget = budget;
        self.nodes = 0;
        self.tb_hits = 0;
        // The pool totals are zeroed once per `go` by `SharedState::reset`, so what this
        // worker has already contributed to them goes back to zero with them.
        self.published_nodes = 0;
        self.published_tb_hits = 0;
        self.sel_depth = 0;
        self.root_depth = 0;
        self.completed_depth = 0;
        self.nmp_min_ply = Ply::ROOT;
        self.best_move_changes = 0.0;
        self.calls_cnt = 0;
        self.optimism = [VALUE_ZERO; 2];

        // Build the root move list, honouring `searchmoves` when the caller gave one.
        //
        // In the ORDER THE CALLER GAVE, not in generation order. Upstream builds the list
        // straight from `limits.searchmoves` and only falls back to the generator when that
        // is empty, so `searchmoves e2e4 d2d4` searches e2e4 first. Filtering the generator
        // instead reorders the pair, and the root order decides which move is searched first
        // at depth one and breaks ties in the stable sort between iterations -- so the
        // restricted search took a different path and a different node count.
        self.root_moves = if limits.search_moves.is_empty() {
            generate_legal(&self.pos).iter().copied().map(RootMove::new).collect()
        } else {
            limits.search_moves.iter().copied().map(RootMove::new).collect()
        };

        if self.root_moves.is_empty() {
            // Checkmate or stalemate at the root: there is nothing to search, and the
            // protocol still expects a `bestmove` -- preceded by upstream's one-line report,
            // which says whether the position is lost or drawn.
            let score = if self.pos.in_check() { -VALUE_MATE } else { VALUE_DRAW };
            if self.id == 0 {
                sink.no_moves(0, &Score::new(score, &self.pos));
            }
            return SearchResult {
                best_move: Move::NONE,
                ponder_move: None,
                score,
                depth: 0,
                nodes: 0,
            };
        }

        // Rank the root against the tablebases before searching. When the position is in
        // the tables this decides the game outright, and the search below is then only
        // choosing between moves that already preserve the result.
        self.rank_root_moves();

        let mut uci_pv_sent = self.iterative_deepening(tt, sink);

        // A search stopped during a fail high at the root leaves a PV one move long, and a
        // `bestmove` with no ponder move gives a pondering GUI nothing to think about. Look
        // the reply up in the table instead, as upstream does.
        if self.root_moves[0].pv.len() == 1 && self.extract_ponder_from_tt(tt) {
            uci_pv_sent = false;
        }

        // Send the final line if the last one no longer describes the move about to be
        // played. Without this the `bestmove` and the last `info ... pv` disagree whenever
        // the search was cut off mid-iteration -- which at a short time control is most
        // moves, and which every GUI flags.
        if self.id == 0 && !uci_pv_sent {
            let score = self.root_moves[0].uci_score;
            // `root_depth`, not `completed_depth`: upstream reports the depth it was WORKING
            // on, which is one deeper than the last finished iteration whenever the search
            // was cut off mid-iteration -- and being cut off mid-iteration is exactly when
            // this line is sent.
            self.report(self.root_depth, score, tt, sink);
        }

        let best = &self.root_moves[0];
        SearchResult {
            best_move: best.mv,
            ponder_move: best.pv.get(1).copied(),
            score: best.score,
            depth: self.completed_depth,
            nodes: self.nodes,
        }
    }

    /// Fill in a missing ponder move from the transposition table.
    ///
    /// Called only when the search left a PV one move long — a fail high at the root that
    /// was stopped before the reply was searched. Playing the best move and reading the
    /// table entry for the position it reaches recovers a reply that was already searched,
    /// which is worth more to a pondering GUI than no reply at all.
    ///
    /// The draw test is upstream's and is load-bearing: a repetition or fifty-move draw
    /// stores an entry whose move belongs to a different line, and pondering it would have
    /// the engine thinking about a move the opponent cannot sensibly play.
    ///
    /// Returns whether a ponder move was appended.
    fn extract_ponder_from_tt(&mut self, tt: &TranspositionTable) -> bool {
        let best = self.root_moves[0].pv[0];
        if !best.is_ok() {
            return false;
        }

        self.pos.do_move(best);
        if !self.pos.is_draw(Ply::new(1)) {
            let probe = tt.probe(self.pos.key());
            if probe.hit
                && probe.data.mv.is_ok()
                && crate::board::movegen::generate_legal(&self.pos).contains(&probe.data.mv)
            {
                self.root_moves[0].pv.push(probe.data.mv);
            }
        }
        self.pos.undo_move(best);

        self.root_moves[0].pv.len() > 1
    }

    /// The iterative-deepening loop: search at depth one, then two, and so on, each
    /// iteration seeding the next one's move ordering and aspiration window.
    ///
    /// Returns whether the LAST line emitted describes the state the search ended in. It
    /// usually does; it does not when the search was stopped part-way through an iteration,
    /// because the root list was re-sorted after the report went out. The caller re-emits
    /// in that case -- otherwise the `bestmove` names one move and the last `info` line
    /// names another, which is what a GUI reads as an engine bug.
    fn iterative_deepening(&mut self, tt: &TranspositionTable, sink: &mut dyn InfoSink) -> bool {
        let main_thread = self.id == 0;
        let mut uci_pv_sent = false;

        for e in &mut self.stack {
            *e = StackEntry::default();
        }
        // The accumulator stack describes plies of the PREVIOUS root. Ply zero is the one
        // slot no `do_move` stamps, so a stale entry there reads as self-consistent and
        // would be rolled forward from -- against a position the search never stood on.
        self.scratch.new_search();
        // The seven entries below ply zero are sentinels, so a node near the root can look
        // back without a bounds test. Their static evaluation must read as "unknown"
        // rather than as a number, or the improving test at ply 0 and 1 would compare
        // against a value that was never computed.
        for i in 1..=7 {
            self.stack[StackIx::pre_root(i).index()].static_eval = VALUE_NONE;
            self.stack[StackIx::pre_root(i).index()].continuation = ContKey::UNREAD;
            self.stack[StackIx::pre_root(i).index()].continuation_correction = CorrKey::UNREAD;
        }

        if main_thread {
            // Seed the iteration history with the previous move's score, so the
            // falling-eval term compares against something real from the first iteration.
            let seed = if self.best_previous_score == VALUE_INFINITE {
                VALUE_ZERO
            } else {
                self.best_previous_score
            };
            self.iter_value = [seed; 4];
        }

        let mut skill = Skill::new(self.opts.skill_level, self.opts.uci_elo);
        let mut rng = Prng::from_clock();

        // A handicap needs alternatives to choose between, so it searches several lines
        // behind the GUI's back whatever the GUI asked for.
        let mut multi_pv = self.opts.multi_pv;
        if skill.enabled() {
            multi_pv = multi_pv.max(4);
        }
        self.multi_pv = multi_pv.min(self.root_moves.len());

        let mut search_again_counter = 0i32;
        let mut last_best_pv: Vec<Move> = Vec::new();
        let mut last_best_depth = 0i32;
        let mut last_best_score = -VALUE_INFINITE;
        let mut time_reduction = 1.0f64;
        let mut tot_best_move_changes = 0.0f64;
        let mut iter_idx = 0usize;
        let mut best_value = -VALUE_INFINITE;

        // The low-ply table is refilled every search rather than cleared: a positive
        // starting value makes an untried move near the root sort above one that has
        // already failed there.
        self.histories.low_ply.fill(102);
        // The butterfly table is DECAYED rather than cleared. The previous move's ordering
        // is still evidence about this one, just weaker than the evidence this search will
        // gather itself.
        self.histories.main.decay(729, 1024);

        let max_depth = self.limits.depth.unwrap_or(MAX_PLY as i32 - 1);
        while self.root_depth + 1 < MAX_PLY as i32
            && !self.shared.stopped()
            && !(self.limits.depth.is_some() && main_thread && self.root_depth >= max_depth)
        {
            self.root_depth += 1;

            if main_thread {
                // Halve rather than clear: a move that was unstable two iterations ago is
                // weaker evidence than one that is unstable now, not no evidence.
                tot_best_move_changes /= 2.0;
                // Whatever was reported belonged to the previous iteration.
                uci_pv_sent = false;
            }

            for rm in &mut self.root_moves {
                rm.previous_score = rm.score;
                rm.previous_pv.clone_from(&rm.pv);
                rm.previous_score_exact = false;
            }
            for i in 0..self.multi_pv {
                self.root_moves[i].previous_score_exact = true;
            }

            let mut pv_first = 0usize;
            self.pv_last = 0;

            if !self.shared.increase_depth() {
                search_again_counter += 1;
            }

            for pv_idx in 0..self.multi_pv {
                self.pv_index = pv_idx;
                if pv_idx == self.pv_last {
                    // Root moves the tablebases rank equally form one block; `MultiPV`
                    // reports across a block rather than through it, because the tables
                    // have already decided that those moves are interchangeable.
                    pv_first = self.pv_last;
                    self.pv_last += 1;
                    while self.pv_last < self.root_moves.len() {
                        if self.root_moves[self.pv_last].tb_rank
                            != self.root_moves[pv_first].tb_rank
                        {
                            break;
                        }
                        self.pv_last += 1;
                    }
                }

                self.last_iteration_pv.clone_from(&self.root_moves[pv_idx].previous_pv);
                self.sel_depth = 0;

                // The window opens around the move's averaged score, widened by how much
                // that score has been swinging.
                let mut delta = 5
                    + (self.id % 8) as i32
                    + self.root_moves[pv_idx].mean_squared_score.abs().get() / 10193;
                let avg = self.root_moves[pv_idx].average_score;
                let mut alpha = (avg - delta).max(-VALUE_INFINITE);
                let mut beta = (avg + delta).min(VALUE_INFINITE);

                // The evaluation is nudged toward the side that is already doing well, so
                // a won position is evaluated by a network that expects to win it.
                let us = self.pos.side_to_move();
                self.optimism[us.index()] = 114 * avg / (avg.abs().get() + 85);
                self.optimism[(!us).index()] = -self.optimism[us.index()];

                let mut failed_high_cnt = 0i32;
                loop {
                    // Ensure at least one effective increment for every four re-search
                    // steps, so a root that keeps being re-searched still deepens.
                    let adjusted_depth =
                        (self.root_depth - failed_high_cnt - 3 * (search_again_counter + 1) / 4)
                            .max(1);
                    self.root_delta = beta - alpha;
                    // Reborrowed rather than moved, so `sink` is free again for the report
                    // below: the root holds it for exactly as long as it is inside the
                    // search.
                    best_value = self.node::<Root>(
                        alpha,
                        beta,
                        adjusted_depth,
                        Ply::ROOT,
                        Expect::All,
                        tt,
                        &mut *sink,
                    );

                    // Stable, because every score but the first and the new best is set to
                    // -INFINITE and the order of the rest must be preserved.
                    self.root_moves[pv_idx..self.pv_last].sort();

                    if self.shared.stopped() {
                        break;
                    }

                    if main_thread
                        && self.multi_pv == 1
                        && (best_value <= alpha || best_value >= beta)
                        && self.nodes > NODES_LIMIT_OUTPUT
                    {
                        self.report(self.root_depth, best_value, tt, sink);
                    }

                    if best_value <= alpha {
                        beta = alpha;
                        alpha = (best_value - delta).max(-VALUE_INFINITE);
                        failed_high_cnt = 0;
                        if main_thread {
                            self.shared.clear_stop_on_ponderhit();
                        }
                    } else if best_value >= beta {
                        alpha = (beta - delta).max(alpha);
                        beta = (best_value + delta).min(VALUE_INFINITE);
                        failed_high_cnt += 1;
                    } else {
                        break;
                    }

                    delta += 47 * delta / 128;
                }

                if self.shared.stopped() && pv_idx > 0 {
                    self.repair_aborted_multipv_line(pv_idx);
                }

                self.root_moves[pv_first..=pv_idx].sort();

                if main_thread
                    && !self.shared.stopped()
                    && (pv_idx + 1 == self.multi_pv || self.nodes > NODES_LIMIT_OUTPUT)
                {
                    self.report(self.root_depth, best_value, tt, sink);
                    // Only the LAST `MultiPV` slot completes the picture. A report for slot
                    // one of three describes a state the caller must still re-emit.
                    uci_pv_sent = pv_idx + 1 == self.multi_pv;
                }

                if self.shared.stopped() {
                    break;
                }
            }

            // A mate found at an earlier iteration must not be forgotten by a later one
            // that failed to reproduce it: a shorter mate is a stronger claim, and a
            // search that lost track of it has not refuted it.
            let forgotten_mate = last_best_score != -VALUE_INFINITE
                && is_mate_or_mated(last_best_score)
                && (self.root_moves[0].score.abs() < last_best_score.abs()
                    || self.root_moves[0].score_is_bound());

            if !self.shared.stopped() {
                if last_best_pv.is_empty() || last_best_pv[0] != self.root_moves[0].pv[0] {
                    last_best_depth = self.root_depth;
                    if !last_best_pv.is_empty() {
                        self.best_move_changes += 1.0;
                    }
                }
                if !forgotten_mate {
                    last_best_pv.clone_from(&self.root_moves[0].pv);
                    last_best_score = self.root_moves[0].score;
                }
                self.completed_depth = self.root_depth;
            }

            let aborted_loss_search = self.shared.stopped()
                && self.pv_index == 0
                && self.root_moves[0].score_is_exact_loss();

            // An exact loss from an aborted search cannot be trusted: the loss could be
            // delayed, or refuted outright, by the root moves that were never reached.
            if aborted_loss_search
                || (self.root_moves[0].score != -VALUE_INFINITE && forgotten_mate)
            {
                if !last_best_pv.is_empty() {
                    if let Some(i) = self.root_moves.iter().position(|rm| rm.mv == last_best_pv[0])
                    {
                        let rm = self.root_moves.remove(i);
                        self.root_moves.insert(0, rm);
                    }
                    self.root_moves[0].score = last_best_score;
                    self.root_moves[0].uci_score = last_best_score;
                    self.root_moves[0].pv.clone_from(&last_best_pv);
                    self.root_moves[0].unset_bound_flags();
                    if main_thread {
                        // The reported line is now the wrong one.
                        uci_pv_sent = false;
                    }
                } else if aborted_loss_search {
                    self.root_moves[0].score_lowerbound = true;
                }
            }

            if let Some(mate) = self.limits.mate
                && !self.shared.stopped()
                && is_mate_or_mated(self.root_moves[0].score)
                // SATURATING, and the shell clamps the count at its own boundary as
                // well -- `Limits::MAX_MATE`. Identical for every value inside that bound,
                // so the two together mean no `go mate N` can panic the engine.
                && VALUE_MATE - self.root_moves[0].score.abs() <= mate.saturating_mul(2)
            {
                self.shared.request_stop();
            }

            if !main_thread {
                continue;
            }

            // Pick the handicapped move at the depth the level corresponds to, and keep it:
            // later iterations refine a line this opponent is not supposed to see.
            if skill.enabled() && skill.time_to_pick(self.root_depth) {
                skill.pick_best(&self.root_moves, self.multi_pv, &mut rng);
            }

            tot_best_move_changes +=
                self.best_move_changes + self.shared.take_best_move_changes() as f64;
            self.best_move_changes = 0.0;

            if self.limits.uses_time_management()
                && !self.shared.stopped()
                && !self.shared.stop_on_ponderhit()
            {
                let nodes = self.pool_nodes();
                let nodes_effort = self.root_moves[0].effort * 100_000 / nodes.max(1);

                // A score that is falling relative to the last move, or to recent
                // iterations, is a reason to keep looking rather than to bank the answer.
                let falling_eval = (11.48
                    + 2.30 * f64::from(self.best_previous_average_score - best_value)
                    + 1.1 * f64::from(self.iter_value[iter_idx] - best_value))
                    / 100.0;
                let falling_eval = falling_eval.clamp(0.576, 1.728);

                // A best move that has survived several iterations is unlikely to change
                // now, so spend less on it.
                time_reduction = interpolate(
                    f64::from(self.root_depth - last_best_depth),
                    4.96,
                    18.79,
                    0.639,
                    1.712,
                )
                .clamp(0.629, 1.544);
                let reduction = (1.468 + self.previous_time_reduction) / (2.284 * time_reduction);

                let best_move_instability =
                    1.077 + 2.229 * tot_best_move_changes / self.thread_count as f64;

                // A best move that already accounts for most of the tree has little left to
                // be displaced by.
                let high_best_move_effort =
                    interpolate(nodes_effort as f64, 75_800.0, 104_510.0, 0.969, 0.714)
                        .clamp(0.693, 0.838);

                let mut total_time = self.budget.optimum().get() as f64
                    * falling_eval
                    * reduction
                    * best_move_instability
                    * high_best_move_effort;

                if self.root_moves.len() == 1 {
                    // With one legal move, cap the think at half a second: there is nothing
                    // to decide and a viewer should not be left waiting.
                    total_time = total_time.min(500.0);
                }

                let elapsed = self.budget.elapsed(nodes).get() as f64;

                // Stop when the budget is spent, or when the line is already decided.
                if elapsed > total_time.min(self.budget.maximum().get() as f64)
                    || self.root_moves[self.multi_pv - 1].score >= mate_in(Ply::new(3))
                    || self.root_moves[0].score == mated_in(Ply::new(2))
                {
                    if self.shared.pondering() {
                        // Pondering is free time. Keep going until the GUI says otherwise.
                        self.shared.request_stop_on_ponderhit();
                    } else {
                        self.shared.request_stop();
                    }
                } else {
                    self.shared.set_increase_depth(
                        self.shared.pondering() || elapsed <= total_time * 0.50,
                    );
                }
            }

            self.iter_value[iter_idx] = best_value;
            iter_idx = (iter_idx + 1) & 3;
        }

        if !main_thread {
            return false;
        }

        self.previous_time_reduction = time_reduction;
        self.best_previous_score = self.root_moves[0].score;
        self.best_previous_average_score = self.root_moves[0].average_score;

        // Swap the handicapped choice into first place. It was searched like any other
        // root move, so its PV and score are real; it is simply not the best one.
        if skill.enabled() {
            let chosen = if skill.best().is_none() {
                skill.pick_best(&self.root_moves, self.multi_pv, &mut rng)
            } else {
                skill.best()
            };
            if let Some(i) = self.root_moves.iter().position(|rm| rm.mv == chosen) {
                // Swap ONLY. Upstream does not mark the PV unsent here, so the last line the
                // GUI saw describes the strongest move while `bestmove` names the
                // handicapped one -- marking it unsent re-sent a line identical to the one
                // already published, which is a duplicate rather than a correction.
                self.root_moves.swap(0, i);
            }
        }

        uci_pv_sent
    }

    /// Keep an aborted `MultiPV` line from overtaking the one above it.
    ///
    /// A search stopped part-way through a PV slot can report a loss it never proved, and
    /// because the slots are sorted against each other that unproven loss can displace a
    /// line the search DID finish. The previous iteration's score is used when it is
    /// trustworthy; otherwise the score is capped and marked as a bound, which is also a
    /// valid excuse for the incomplete PV.
    fn repair_aborted_multipv_line(&mut self, pv_idx: usize) {
        let prev_score = self.root_moves[pv_idx - 1].score;
        let overtakes = is_loss(prev_score)
            && self.root_moves[pv_idx].cmp(&self.root_moves[pv_idx - 1])
                == core::cmp::Ordering::Less;
        if overtakes || self.root_moves[pv_idx].score_is_exact_loss() {
            if self.root_moves[pv_idx].previous_score != -VALUE_INFINITE
                && self.root_moves[pv_idx].previous_score_exact
                && self.root_moves[pv_idx].previous_score <= prev_score
            {
                let ps = self.root_moves[pv_idx].previous_score;
                let ppv = self.root_moves[pv_idx].previous_pv.clone();
                let rm = &mut self.root_moves[pv_idx];
                rm.score = ps;
                rm.uci_score = ps;
                rm.previous_score = -VALUE_INFINITE;
                rm.pv = ppv;
                rm.unset_bound_flags();
            } else {
                if is_loss(prev_score) {
                    let rm = &mut self.root_moves[pv_idx];
                    rm.score = prev_score;
                    rm.uci_score = prev_score;
                    rm.previous_score = -VALUE_INFINITE;
                    rm.pv.truncate(1);
                    rm.score_upperbound = true;
                } else {
                    self.root_moves[pv_idx].score_upperbound = false;
                }
                self.root_moves[pv_idx].score_lowerbound =
                    !self.root_moves[pv_idx].score_upperbound;
            }
        }

        // Every loss score from a partially searched move is a bound, not a fact.
        for i in pv_idx + 1..self.multi_pv {
            if self.root_moves[i].score_is_exact_loss() {
                self.root_moves[i].score_lowerbound = true;
            }
        }
    }

    // -- the node ----------------------------------------------------------

    /// The alpha-beta node.
    ///
    /// `PV` distinguishes a node on the principal variation, which is searched with a real
    /// window and whose children must all be examined, from a zero-window node, where the
    /// first move that beats beta ends the node. `ROOT` marks the one node that owns the
    /// root move list. Both are const generics rather than runtime flags, so the optimiser
    /// drops the PV-only and root-only bookkeeping from the zero-window instantiation,
    /// which is the overwhelming majority of nodes.
    #[allow(clippy::too_many_lines)]
    fn node<N: Announces>(
        &mut self,
        mut alpha: Value,
        mut beta: Value,
        mut depth: i32,
        ply: Ply,
        cut_node: Expect,
        tt: &TranspositionTable,
        mut announce: N::Announcer<'_>,
    ) -> Value {
        let all_node = !(N::PV || cut_node.is_cut());

        if depth <= 0 {
            return self.qsearch::<N::Quiescent>(alpha, beta, ply, tt);
        }
        depth = depth.min(MAX_PLY as i32 - 1);

        // A move that repeats a position already on the board is available here, so the
        // side to move can guarantee at least a draw. Checked before anything else,
        // because it can raise alpha above beta outright.
        if !N::ROOT && alpha < VALUE_DRAW && self.pos.upcoming_repetition(ply) {
            alpha = value_draw(self.nodes);
            if alpha >= beta {
                return alpha;
            }
        }

        let si = StackIx::from_ply(ply);
        let mut best_value;
        let mut max_value = VALUE_INFINITE;
        // Fixed arrays, not `Vec`. Upstream's `ValueList<Move, 32>` is inline storage and
        // never allocates; a `Vec` here is one malloc per node that reaches its move loop,
        // and the node loop is the hottest code in the engine. The capacity is upstream's,
        // and the loop below already refuses to record past it.
        let mut captures_searched = [Move::NONE; SEARCHED_LIST_CAPACITY];
        let mut n_captures = 0usize;
        let mut quiets_searched = [Move::NONE; SEARCHED_LIST_CAPACITY];
        let mut n_quiets = 0usize;

        // Step 1. Initialize node
        self.stack[si.index()].in_check = self.pos.in_check();
        let in_check = self.stack[si.index()].in_check;
        let prior_capture = !self.pos.captured_piece().is_none();
        let us = self.pos.side_to_move();
        self.stack[si.index()].move_count = 0;
        best_value = -VALUE_INFINITE;

        // Still on the previous iteration's principal variation? Several pruning rules are
        // relaxed there, because that line is the one the iteration most needs resolved.
        self.stack[si.index()].follow_pv = N::ROOT
            || (self.stack[si.back(1).index()].follow_pv
                && ply.prev().index() < self.last_iteration_pv.len()
                && self.stack[si.back(1).index()].current_move
                    == self.last_iteration_pv[ply.prev().index()]);

        if self.id == 0 {
            self.check_time();
        }

        if N::PV && self.sel_depth < ply.next().get() {
            self.sel_depth = ply.next().get();
        }

        if !N::ROOT {
            // Step 2. Check for aborted search and immediate draw
            if self.shared.stopped() || self.pos.is_draw(ply) || ply >= Ply::MAX {
                return if ply >= Ply::MAX && !in_check {
                    self.evaluate(ply)
                } else {
                    value_draw(self.nodes)
                };
            }

            // Step 3. Mate distance pruning. A mate found closer to the root beats anything
            // this subtree could produce, so the window can be narrowed to the mate range —
            // and when that empties the window, the node is already answered.
            alpha = alpha.max(mated_in(ply));
            beta = beta.min(mate_in(ply.next()));
            if alpha >= beta {
                return alpha;
            }
        }

        let prev_sq = if self.stack[si.back(1).index()].current_move.is_ok() {
            self.stack[si.back(1).index()].current_move.to().some()
        } else {
            SquareOrNone::NONE
        };
        let mut best_move = Move::NONE;
        let prior_reduction = self.stack[si.back(1).index()].reduction;
        self.stack[si.back(1).index()].reduction = 0;
        self.stack[si.index()].stat_score = 0;
        self.stack[si.next().next().index()].cutoff_count = 0;

        let correction_value = self.correction_value(si);

        // Step 4. Transposition table lookup
        let excluded_move = self.stack[si.index()].excluded_move;
        let pos_key = self.pos.key();
        let probe = tt.probe(pos_key);
        let tt_hit = probe.hit;
        self.stack[si.index()].tt_hit = tt_hit;
        let tt_move = if N::ROOT {
            self.root_moves[self.pv_index].pv[0]
        } else if tt_hit {
            probe.data.mv
        } else {
            Move::NONE
        };
        let tt_value = if tt_hit {
            value_from_tt(probe.data.value, ply, self.pos.rule50_count())
        } else {
            VALUE_NONE
        };
        let tt_depth = probe.data.depth;
        let tt_bound = probe.data.bound;
        self.stack[si.index()].tt_pv = if excluded_move.is_some() {
            self.stack[si.index()].tt_pv
        } else {
            N::PV || (tt_hit && probe.data.is_pv)
        };
        let tt_capture = tt_move.is_some() && self.pos.is_capture_stage(tt_move);

        // Step 5. Static evaluation of the position
        let mut unadjusted_static_eval = VALUE_NONE;
        let mut eval;
        if in_check {
            // In check there is no standing estimate: every move is forced. The value two
            // plies back is carried forward so the improving test still has an anchor.
            self.stack[si.index()].static_eval = self.stack[si.back(2).index()].static_eval;
            eval = self.stack[si.index()].static_eval;
        } else if excluded_move.is_some() {
            // The singular search re-enters this node; re-evaluating would be wasted work
            // and, through the correction tables, would not even give the same answer.
            unadjusted_static_eval = self.stack[si.index()].static_eval;
            eval = self.stack[si.index()].static_eval;
        } else if tt_hit {
            unadjusted_static_eval = probe.data.eval;
            if !is_valid(unadjusted_static_eval) {
                unadjusted_static_eval = self.evaluate(ply);
            }
            self.stack[si.index()].static_eval =
                Self::to_corrected_static_eval(unadjusted_static_eval, correction_value);
            eval = self.stack[si.index()].static_eval;

            // A searched score is a better estimate than a static one, when its bound
            // points the right way.
            if is_valid(tt_value)
                && match tt_bound {
                    Bound::Exact => true,
                    Bound::Lower => tt_value > eval,
                    Bound::Upper => tt_value <= eval,
                    Bound::None => false,
                }
            {
                eval = tt_value;
            }
        } else {
            unadjusted_static_eval = self.evaluate(ply);
            self.stack[si.index()].static_eval =
                Self::to_corrected_static_eval(unadjusted_static_eval, correction_value);
            eval = self.stack[si.index()].static_eval;

            // The evaluation is stored as it was BEFORE the correction. The table is shared
            // between nodes whose histories differ, and a corrected value would not mean
            // the same thing to the next reader.
            tt.store(
                probe,
                Move::NONE,
                VALUE_NONE,
                unadjusted_static_eval,
                DEPTH_UNSEARCHED,
                Bound::None,
                self.stack[si.index()].tt_pv,
            );
        }

        let mut improving =
            self.stack[si.index()].static_eval > self.stack[si.back(2).index()].static_eval;
        let opponent_worsening =
            self.stack[si.index()].static_eval > -self.stack[si.back(1).index()].static_eval;

        // Hindsight adjustment. The parent reduced this node on an expectation the static
        // evaluation has now contradicted, so the reduction is partly undone — or, when the
        // evaluation agrees emphatically, pushed a little further.
        if prior_reduction >= 3 && !opponent_worsening {
            depth += 1;
        }
        if prior_reduction >= 2
            && depth >= 2
            && self.stack[si.index()].static_eval.get()
                + self.stack[si.back(1).index()].static_eval.get()
                > 166
        {
            depth -= 1;
        }

        // At non-N::PV nodes a stored score searched deep enough, whose bound is on the right
        // side of the window, answers the node outright.
        if !N::PV
            && excluded_move.is_none()
            && tt_depth > depth - i32::from(tt_value <= beta)
            && is_valid(tt_value)
            && match tt_bound {
                Bound::Exact => true,
                Bound::Lower => tt_value >= beta,
                Bound::Upper => tt_value < beta,
                Bound::None => false,
            }
            && (cut_node.is_cut() == (tt_value >= beta) || depth > 4)
        {
            if tt_move.is_some() && tt_value >= beta {
                if !tt_capture {
                    self.update_quiet_histories(si, tt_move, Bonus::new((112 * depth).min(695)));
                }
                if let Some(prev_sq) = prev_sq.square()
                    && self.stack[si.back(1).index()].move_count < 5
                    && !prior_capture
                {
                    let pc = self.pos.piece_on(prev_sq);
                    self.update_continuation_histories(si.back(1), pc, prev_sq, Bonus::new(-2210));
                }
            }

            // A high halfmove clock is where the transposition table and the fifty-move
            // rule disagree: the same position is a different game depending on how much
            // clock is left, and the table cannot express that.
            if self.pos.rule50_count() < 96 {
                if depth >= 7
                    && tt_move.is_some()
                    && self.pos.pseudo_legal(tt_move)
                    && self.pos.legal(tt_move)
                    && !is_decisive(tt_value)
                {
                    // Verify that the position AFTER the transposition move also cuts off.
                    // A cutoff that survives one move is far less likely to be an artefact
                    // of the table than one that does not.
                    self.pos.do_move(tt_move);
                    let next = tt.probe(self.pos.key());
                    let next_value = next.data.value;
                    let next_hit = next.hit;
                    self.pos.undo_move(tt_move);

                    if !next_hit || !is_valid(next_value) {
                        return tt_value;
                    }
                    if (tt_value >= beta) == (-next_value >= beta) {
                        return tt_value;
                    }
                } else {
                    return tt_value;
                }
            }
        } else if !N::PV
            && excluded_move.is_none()
            && tt_depth > depth - i32::from(tt_value <= beta)
            && is_valid(tt_value)
            && tt_bound != Bound::Exact
            && match tt_bound {
                Bound::Upper => tt_value >= beta,
                Bound::Lower => tt_value < beta,
                _ => false,
            }
            && depth > 5
        {
            // The entry is not wrong, but its bound sits on the useless side of this
            // window and it is occupying a deep slot. Penalise it so the slot can be
            // reclaimed by an entry that would answer something.
            tt.penalize(probe, 1);
        }

        // Step 6. Tablebases probe
        if !N::ROOT
            && excluded_move.is_none()
            && self.tb_cardinality > 0
            && let Some(v) = self.probe_tablebases(depth, ply, alpha, beta, tt, probe)
        {
            match v {
                TbOutcome::Cutoff(value) => return value,
                TbOutcome::LowerBound(value) => {
                    best_value = value;
                    alpha = alpha.max(best_value);
                }
                TbOutcome::UpperBound(value) => max_value = value,
            }
        }

        let mut prob_cut_beta;
        if !in_check {
            // Use the static evaluation's own change to order quiet moves. A move after
            // which the evaluation jumped is a move worth trying earlier next time, and
            // this is the cheapest possible measurement of that.
            if self.stack[si.back(1).index()].current_move.is_ok()
                && !self.stack[si.back(1).index()].in_check
                && !prior_capture
            {
                let eval_diff = (-(self.stack[si.back(1).index()].static_eval.get()
                    + self.stack[si.index()].static_eval.get()))
                .clamp(-189, 194)
                    + 60;
                let prev_move = self.stack[si.back(1).index()].current_move;
                self.histories.main.update(!us, prev_move.raw(), Bonus::new(eval_diff * 11));
                if let Some(prev_sq) = prev_sq.square()
                    && !tt_hit
                    && self.pos.piece_on(prev_sq).piece_type() != PieceType::Pawn
                    && prev_move.move_type() != crate::board::types::MoveType::Promotion
                {
                    let pawn_row = super::history::PawnHistory::row(self.pos.st().pawn_key);
                    let pc = self.pos.piece_on(prev_sq);
                    self.histories.pawn.update(pawn_row, pc, prev_sq, Bonus::new(eval_diff * 13));
                }
            }

            // Step 7. Razoring. So far below alpha that the full search is very unlikely to
            // recover; ask quiescence instead, which is far cheaper.
            if !N::PV && eval < alpha - 482 * depth * depth {
                return self.qsearch::<NonPv>(alpha, beta, ply, tt);
            }

            // Step 8. Futility pruning at a child node. Far enough above beta that even
            // giving material away could not bring the position below it.
            //
            // The depth cutoff is dynamic, and it is what keeps mates findable. Test it LAST:
            // it is the only clause here that costs a scan, and the four before it are free.
            if !self.stack[si.index()].tt_pv
                && eval >= beta
                && (tt_move.is_none() || tt_capture)
                && !is_loss(beta)
                && !is_win(eval)
                && depth < futility_depth(eval, beta)
            {
                let mut futility_mult = (45 + depth * 4).min(85);
                futility_mult -= 20 * i32::from(!self.stack[si.index()].tt_hit);

                let futility_margin = futility_mult * depth
                    - (2789 * i32::from(improving) + 335 * i32::from(opponent_worsening))
                        * futility_mult
                        / 1024
                    + correction_value.abs() / 198_435;

                if eval - futility_margin >= beta {
                    // Return a blend rather than the raw evaluation: the node was never
                    // searched, so a value that far above beta would be over-claiming.
                    return Value::new((661 * beta.get() + 363 * eval.get()) / 1024);
                }
            }

            // Step 9. Null move search with verification search
            if cut_node.is_cut()
                && self.stack[si.index()].static_eval
                    >= beta - 13 * depth - 47 * i32::from(improving) + 365
                && excluded_move.is_none()
                && self.pos.non_pawn_material(us) > 0
                && ply >= self.nmp_min_ply
                && beta >= -2000
            {
                // Give the opponent a free move; if the position still beats beta, the
                // real move would too. Skipped in a pawn endgame, where zugzwang makes
                // "pass" a genuinely good option and the assumption fails.
                // `>> 8` rather than `/ 256`, and the two agree here with no precondition
                // on the sign: for a non-negative margin they are the same value, and for a
                // negative one both are at most zero, so the `max` returns zero either way.
                // Moving the `max` off it is what would break the identity. rustc pays the
                // round-toward-zero correction for the division -- `lea 0xff(%rax),%edx`,
                // `test`, `cmovns` ahead of the `sar $0x8` -- and the shift is the sar alone.
                let r = 7 + depth / 3 + ((self.stack[si.index()].static_eval - beta) >> 8).max(0);
                self.do_null_move(si);
                let null_value = -self.node::<NonPv>(
                    -beta,
                    -beta + 1,
                    depth - r,
                    ply.next(),
                    Expect::All,
                    tt,
                    (),
                );
                self.undo_null_move();

                if null_value >= beta && !is_win(null_value) {
                    if self.nmp_min_ply != Ply::ROOT || depth < 16 {
                        return null_value;
                    }

                    // At high depth the null-move assumption is verified with a real
                    // search, with null moves disabled below so the verification cannot
                    // lean on the very assumption it is checking.
                    self.nmp_min_ply = ply.offset(3 * (depth - r) / 4);
                    let v = self.node::<NonPv>(beta - 1, beta, depth - r, ply, Expect::All, tt, ());
                    self.nmp_min_ply = Ply::ROOT;
                    if v >= beta {
                        return null_value;
                    }
                }
            }

            improving |= self.stack[si.index()].static_eval >= beta;

            // Step 10. Internal iterative reductions. A node with no transposition move has
            // no ordering to work with, so the depth mostly buys a badly ordered tree.
            if !self.stack[si.index()].follow_pv && !all_node && depth >= 6 && tt_move.is_none() {
                depth -= 1;
            }

            // Step 11. ProbCut. A capture good enough to beat a raised beta on a shallow
            // search is good enough to prune the whole node: whatever the opponent was
            // hoping for, this refutes it.
            prob_cut_beta = beta + 241 - 64 * i32::from(improving);
            if depth >= 3 && !is_decisive(beta) && !(is_valid(tt_value) && tt_value < prob_cut_beta)
            {
                let slot = SLOTS_PER_PLY * ply.index()
                    + if excluded_move.is_some() { SLOT_EXCLUDED } else { SLOT_NODE };
                let mut mp = MovePicker::new_probcut(
                    &self.pos,
                    tt_move,
                    Value::new(prob_cut_beta - self.stack[si.index()].static_eval),
                );
                let prob_cut_depth = depth - if improving { 5 } else { 3 };

                loop {
                    let mv = mp.next_move(&self.pos, &self.histories, &mut self.move_pool[slot]);
                    if mv.is_none() {
                        break;
                    }
                    if mv == excluded_move || !self.pos.legal(mv) {
                        continue;
                    }

                    let gives_check = self.pos.gives_check(mv);
                    self.do_move(mv, gives_check, si);

                    // A quiescence probe first: it is far cheaper than the real search and
                    // rejects most candidates outright.
                    let mut value =
                        -self.qsearch::<NonPv>(-prob_cut_beta, -prob_cut_beta + 1, ply.next(), tt);
                    if value >= prob_cut_beta && prob_cut_depth > 0 {
                        value = -self.node::<NonPv>(
                            -prob_cut_beta,
                            -prob_cut_beta + 1,
                            prob_cut_depth,
                            ply.next(),
                            cut_node.flipped(),
                            tt,
                            (),
                        );
                    }
                    self.undo_move(mv);

                    if value >= prob_cut_beta {
                        tt.store(
                            probe,
                            mv,
                            value_to_tt(value, ply),
                            unadjusted_static_eval,
                            prob_cut_depth + 1,
                            Bound::Lower,
                            self.stack[si.index()].tt_pv,
                        );
                        if !is_decisive(value) {
                            return value - (prob_cut_beta - beta);
                        }
                    }
                }
            }
        }

        // Step 12. A small ProbCut idea. A stored lower bound far above beta, at nearly
        // this depth, is enough on its own.
        prob_cut_beta = beta + 428;
        if (tt_bound == Bound::Lower || tt_bound == Bound::Exact)
            && tt_depth >= depth - 4
            && tt_value >= prob_cut_beta
            && !is_decisive(beta)
            && is_valid(tt_value)
            && !is_decisive(tt_value)
        {
            return prob_cut_beta;
        }

        let cont_keys: ContKeys = [
            self.stack[si.back(1).index()].continuation,
            self.stack[si.back(2).index()].continuation,
            self.stack[si.back(3).index()].continuation,
            self.stack[si.back(4).index()].continuation,
            self.stack[si.back(5).index()].continuation,
            self.stack[si.back(6).index()].continuation,
        ];

        let slot = SLOTS_PER_PLY * ply.index()
            + if excluded_move.is_some() { SLOT_EXCLUDED } else { SLOT_NODE };
        let mut mp = MovePicker::new(&self.pos, cont_keys, tt_move, depth, ply);
        let mut value = best_value;
        let mut move_count = 0i32;
        let mut reduction_window = self.reduction_window(alpha, beta);

        // Step 13. Loop through all pseudo-legal moves until no moves remain or a beta
        // cutoff occurs.
        loop {
            let mv = mp.next_move(&self.pos, &self.histories, &mut self.move_pool[slot]);
            if mv.is_none() {
                break;
            }
            if mv == excluded_move || !self.pos.legal(mv) {
                continue;
            }
            if N::ROOT && !self.root_moves[self.pv_index..self.pv_last].iter().any(|rm| rm.mv == mv)
            {
                continue;
            }

            move_count += 1;
            self.stack[si.index()].move_count = move_count;

            // Announce the root move about to be searched. Rendered here rather than
            // handed over as a `Move`, because notation is the shell's business only once
            // it knows whether the game is Chess960 -- and that is a property of THIS
            // position, which the sink does not hold.
            //
            // The node count is the gate upstream put here on purpose: a GUI shown a
            // `currmove` for every root move of every iteration is shown thousands of
            // lines in the first second, and Fritz 19 hangs on it. Past 10M nodes an
            // iteration is slow enough that the operator wants to know where it is.
            if N::ROOT && self.announces_root_moves() {
                let announced = move_to_uci(&self.pos, mv);
                // `move_count + pv_index`, not `move_count`: under `MultiPV` the moves
                // already settled in earlier slots are skipped rather than counted, so the
                // number a GUI is shown would restart at one for every slot.
                announce.announce(&announced, move_count as usize + self.pv_index, depth);
            }
            if N::PV {
                self.stack[si.next().index()].pv_valid = false;
            }

            let mut extension = 0i32;
            let capture = self.pos.is_capture_stage(mv);
            let moved_piece = self.pos.moved_piece(mv);
            let gives_check = self.pos.gives_check(mv);

            let mut new_depth = depth - 1;
            let mut r = self.reduction(improving, depth, move_count, reduction_window);

            // A node the table considers important is reduced MORE, not less: it will be
            // revisited, so a cheap first look costs little.
            if self.stack[si.index()].tt_pv {
                r += 929;
            }

            // Step 14. Pruning at shallow depths.
            if !N::ROOT && self.pos.non_pawn_material(us) > 0 && !is_loss(best_value) {
                if move_count >= (3 + depth * depth) / (2 - i32::from(improving)) {
                    mp.skip_quiet_moves();
                }

                let mut lmr_depth = new_depth - r / 1024;

                if capture || gives_check {
                    let captured_piece = self.pos.piece_on(mv.to());
                    let capt_hist = self.histories.captures.get(
                        moved_piece,
                        mv.to(),
                        captured_piece.piece_type(),
                    );

                    if !gives_check && lmr_depth < 8 {
                        let futility_value = self.stack[si.index()].static_eval
                            + 234
                            + 247 * lmr_depth
                            + piece_value(captured_piece).get()
                            + 134 * capt_hist / 1024;
                        if futility_value <= alpha {
                            continue;
                        }
                    }

                    // Never prune a sacrifice of the last piece: without it the side to
                    // move may be stalemated, and a stalemate is not a loss.
                    let margin = 177 * depth + capt_hist * 34 / 1024;
                    if (alpha >= VALUE_DRAW
                        || self.pos.non_pawn_material(us) != piece_value(moved_piece))
                        && !self.pos.see_ge(mv, Value::new(-margin))
                    {
                        continue;
                    }
                } else if !self.stack[si.index()].follow_pv || !N::PV {
                    let d_index = (depth.min(LMR_DIVISOR.len() as i32) - 1) as usize;
                    let pawn_row = super::history::PawnHistory::row(self.pos.st().pawn_key);
                    let mut history = self.histories.continuation.get(
                        self.stack[si.back(1).index()].continuation,
                        moved_piece,
                        mv.to(),
                    ) + self.histories.continuation.get(
                        self.stack[si.back(2).index()].continuation,
                        moved_piece,
                        mv.to(),
                    ) + self.histories.pawn.get(pawn_row, moved_piece, mv.to());

                    if history < -4136 * depth {
                        continue;
                    }

                    history += 69 * self.histories.main.get(us, mv.raw()) / 32;
                    lmr_depth += history / LMR_DIVISOR[d_index];

                    let futility_value = self.stack[si.index()].static_eval
                        + 119 * lmr_depth
                        + 90 * i32::from(self.stack[si.index()].static_eval > alpha)
                        + 164;

                    if !in_check && lmr_depth < 12 && futility_value <= alpha {
                        if best_value <= futility_value
                            && !is_decisive(best_value)
                            && !is_win(futility_value)
                        {
                            best_value = futility_value;
                        }
                        continue;
                    }

                    lmr_depth = lmr_depth.max(0);

                    if !self.pos.see_ge(mv, Value::new(-23 * lmr_depth * lmr_depth)) {
                        continue;
                    }
                }
            }

            // Step 15. Extensions. If every move but the transposition move fails low on a
            // narrowed window, the node hinges on that one move and is worth a deeper look.
            if !N::ROOT
                && mv == tt_move
                && excluded_move.is_none()
                && depth >= 6 + i32::from(self.stack[si.index()].tt_pv)
                && is_valid(tt_value)
                && !is_decisive(tt_value)
                && (tt_bound == Bound::Lower || tt_bound == Bound::Exact)
                && tt_depth >= depth - 3
                && !self.is_shuffling(mv, si)
            {
                let singular_beta = tt_value
                    - (59 + 66 * i32::from(self.stack[si.index()].tt_pv && !N::PV)) * depth / 63;
                let singular_depth = new_depth / 2;

                self.stack[si.index()].excluded_move = mv;
                let v = self.node::<NonPv>(
                    singular_beta - 1,
                    singular_beta,
                    singular_depth,
                    ply,
                    cut_node,
                    tt,
                    (),
                );
                self.stack[si.index()].excluded_move = Move::NONE;

                if v < singular_beta {
                    // How far below the margin decides how far to extend: a move that is
                    // singular by a wide margin is more likely to be the only move.
                    let corr_val_adj = correction_value.abs() / 198_368;
                    let double_margin = -2 + 204 * i32::from(N::PV)
                        - 152 * i32::from(!tt_capture)
                        - corr_val_adj
                        - 1175 * i32::from(self.histories.tt_move) / 114_178
                        - i32::from(ply.get() > self.root_depth) * 38;
                    let triple_margin = 70 + 279 * i32::from(N::PV) - 188 * i32::from(!tt_capture)
                        + 81 * i32::from(self.stack[si.index()].tt_pv)
                        - corr_val_adj
                        - i32::from(ply.get() > self.root_depth) * 43;

                    extension = 1
                        + i32::from(v < singular_beta - double_margin)
                        + i32::from(v < singular_beta - triple_margin);
                    depth += 1;
                } else if v >= beta && !is_decisive(v) {
                    // Multi-cut. The transposition move was assumed to fail high, and with
                    // it excluded the node STILL fails high -- so more than one move does,
                    // and the whole subtree can be skipped.
                    self.histories.update_tt_move(Bonus::new(-421 - 110 * depth));

                    // The fail-high is evidence about the static evaluation too: the search
                    // found more here than the evaluation said was available, so feed the
                    // difference back the way a completed search would.
                    if !self.stack[si.index()].in_check && v > self.stack[si.index()].static_eval {
                        let bonus = multicut_correction_bonus(
                            v,
                            self.stack[si.index()].static_eval,
                            singular_depth,
                        );
                        self.update_correction_history(si, bonus);
                    }

                    return v;
                } else if tt_value >= beta || cut_node.is_cut() {
                    // Neither singular nor multi-cut. The transposition move is expected to
                    // fail high anyway, or this is a cut node -- either way search it
                    // shallower in favour of the others. Upstream collapsed the two cases
                    // into one: the -2 arm is gone, and a cut node now reduces by 3.
                    extension = -3;
                }
            }

            let node_count = if N::ROOT { self.nodes } else { 0 };

            // Step 16. Make the move
            self.do_move(mv, gives_check, si);
            new_depth += extension;

            // A node the table considers important is worth a fuller look once it has been
            // reached, which is why this undoes more than the pre-move increase added.
            if self.stack[si.index()].tt_pv {
                r -= 3023
                    + i32::from(N::PV) * 1004
                    + i32::from(tt_value > alpha) * 885
                    + i32::from(tt_depth >= depth) * (816 + i32::from(cut_node.is_cut()) * 940);
            }

            r += 697;
            r -= move_count * 65;
            r -= correction_value.abs() / 26310;

            if cut_node.is_cut() {
                r += 4026 + 933 * i32::from(tt_move.is_none());
            }
            if tt_capture {
                r += 1079;
            }

            // A child that has already produced several cutoffs is an easy node; its
            // siblings can afford to be looked at less carefully.
            if self.stack[si.next().index()].cutoff_count > 1 {
                r += 264
                    + 1095 * i32::from(self.stack[si.next().index()].cutoff_count > 2)
                    + 1138 * i32::from(all_node);
            } else if mv == tt_move {
                r -= 2179;
            }

            self.stack[si.index()].stat_score = if capture {
                let captured = self.pos.captured_piece();
                873 * piece_value(captured).get() / 128
                    + self.histories.captures.get(moved_piece, mv.to(), captured.piece_type())
            } else {
                (2252 * self.histories.main.get(us, mv.raw())
                    + 1126
                        * self.histories.continuation.get(
                            self.stack[si.back(1).index()].continuation,
                            moved_piece,
                            mv.to(),
                        )
                    + 1093
                        * self.histories.continuation.get(
                            self.stack[si.back(2).index()].continuation,
                            moved_piece,
                            mv.to(),
                        ))
                    / 1024
            };

            r -= self.stack[si.index()].stat_score * 439 / 4096;

            // The further the window's floor sits ABOVE the static eval, the more a quiet
            // move has to prove to reach it, so reduce it harder; where the eval is already
            // above alpha, reduce it less. Bounded either way, and skipped once alpha is
            // about a mate or a tablebase verdict rather than about a score.
            if !capture && !is_decisive(alpha) {
                r += 3 * (alpha - eval).clamp(-64, 96);
            }

            // An all-node will be searched exhaustively whatever happens, so reducing there
            // costs the least and saves the most.
            if all_node {
                r += r * 276 / (256 * depth + 268);
            }

            // Step 17. Late moves reduction / extension (LMR)
            if depth >= 2 && move_count > 1 {
                // Cap the reduced depth at `newDepth`, but allow a NEGATIVE reduction to
                // extend a little beyond it. Written as nested min/max rather than a clamp
                // because the upper bound can fall below the lower one, which a clamp
                // treats as a programming error rather than as the intended behaviour.
                let d = (new_depth - r / 1024).min(new_depth + 2).max(1) + i32::from(N::PV);

                self.stack[si.index()].reduction = new_depth - d;
                value =
                    -self.node::<NonPv>(-(alpha + 1), -alpha, d, ply.next(), Expect::Cut, tt, ());
                self.stack[si.index()].reduction = 0;

                if value > alpha {
                    // The reduced search's own result says how much depth this move
                    // deserved: a large margin over the best so far earns more, a narrow
                    // one earns less.
                    let do_deeper_search = d < new_depth && value > best_value + 53;
                    let do_shallower_search = value < best_value + 8;

                    new_depth += i32::from(do_deeper_search) - i32::from(do_shallower_search);

                    if new_depth > d {
                        value = -self.node::<NonPv>(
                            -(alpha + 1),
                            -alpha,
                            new_depth,
                            ply.next(),
                            cut_node.flipped(),
                            tt,
                            (),
                        );
                    }

                    self.update_continuation_histories(si, moved_piece, mv.to(), Bonus::new(1334));
                }
            }
            // Step 18. Full-depth search when LMR is skipped
            else if !N::PV || move_count > 1 {
                if tt_move.is_none() {
                    r += 1127;
                }
                let d = new_depth - i32::from(r > 5234) - i32::from(r > 5487 && new_depth > 2);
                value = -self.node::<NonPv>(
                    -(alpha + 1),
                    -alpha,
                    d,
                    ply.next(),
                    cut_node.flipped(),
                    tt,
                    (),
                );
            }

            // For N::PV nodes only, do a full N::PV search on the first move or after a fail
            // high; otherwise let the parent fail low and try another move.
            if N::PV && (move_count == 1 || value > alpha) {
                self.stack[si.next().index()].pv.clear();
                self.stack[si.next().index()].pv_valid = true;

                // A transposition move about to drop into quiescence is given one ply, so
                // a decisive score the table already knows is not thrown away at the
                // horizon.
                if mv == tt_move
                    && ((is_valid(tt_value) && is_decisive(tt_value) && tt_depth > 0)
                        || tt_depth > 1)
                {
                    new_depth = new_depth.max(1);
                }

                value = -self.node::<Pv>(-beta, -alpha, new_depth, ply.next(), Expect::All, tt, ());
            }

            // Step 19. Undo move
            self.undo_move(mv);

            // Step 20. Check for a new best move
            if self.shared.stopped() {
                return VALUE_ZERO;
            }

            if N::ROOT {
                self.update_root_move(mv, value, alpha, beta, node_count, move_count, si);
            }

            // Two moves with the same score are genuinely interchangeable, and always
            // keeping the first makes the engine repeat itself. Occasionally promoting the
            // later one breaks that up without changing the score.
            let inc = i32::from(
                value == best_value
                    && ply.offset(2).get() >= self.root_depth
                    && (self.nodes as i32 & 0xE) == 0
                    && !is_win(value.abs() + 1),
            );

            if value + inc > best_value {
                best_value = value;

                if value + inc > alpha {
                    best_move = mv;

                    if N::PV && !N::ROOT {
                        let child = core::mem::take(&mut self.stack[si.next().index()].pv);
                        let child_valid = self.stack[si.next().index()].pv_valid;
                        self.stack[si.index()].pv.clear();
                        self.stack[si.index()].pv.push(mv);
                        if child_valid {
                            self.stack[si.index()].pv.extend_from_slice(&child);
                        }
                        self.stack[si.next().index()].pv = child;
                    }

                    if value >= beta {
                        self.stack[si.index()].cutoff_count += i32::from(extension < 2 || N::PV);
                        break;
                    }

                    // One improvement found is evidence the rest are unlikely to beat it,
                    // so the remaining moves are searched shallower. Bounded away from the
                    // frontier, where losing three plies would drop into quiescence.
                    if depth > 3 && depth < 12 && !is_decisive(value) {
                        depth -= 3;
                    }

                    alpha = value;
                    // The only place the window moves inside this loop, so the only place
                    // the term the reduction subtracts has to be re-derived.
                    reduction_window = self.reduction_window(alpha, beta);
                }
            }

            if mv != best_move && (move_count as usize) <= SEARCHED_LIST_CAPACITY {
                if capture {
                    if n_captures < SEARCHED_LIST_CAPACITY {
                        captures_searched[n_captures] = mv;
                        n_captures += 1;
                    }
                } else if n_quiets < SEARCHED_LIST_CAPACITY {
                    quiets_searched[n_quiets] = mv;
                    n_quiets += 1;
                }
            }
        }

        // Step 21. Check for mate and stalemate
        //
        // A fail-high value is pulled back toward beta in proportion to how shallow the
        // node was: a shallow search that overshot is claiming more than it proved.
        if best_value >= beta && !is_decisive(best_value) && !is_decisive(alpha) {
            best_value = Value::new((best_value.get() * depth + beta.get()) / (depth + 1));
        }

        if move_count == 0 {
            best_value = if excluded_move.is_some() {
                alpha
            } else if in_check {
                mated_in(ply)
            } else {
                VALUE_DRAW
            };
        } else if best_move.is_some() {
            self.update_all_stats(
                si,
                best_move,
                prev_sq,
                &quiets_searched[..n_quiets],
                &captures_searched[..n_captures],
                depth,
                tt_move,
                N::PV,
            );
            if !N::PV {
                self.histories.update_tt_move(Bonus::new(if best_move == tt_move {
                    918
                } else {
                    -747
                }));
            }
        } else if let Some(prev_sq) = prev_sq.square()
            && !prior_capture
        {
            // No move improved on alpha, so the move that led here is looking good for the
            // opponent. Reward it, scaled by how badly this node failed.
            let mut bonus_scale = -241;
            bonus_scale -= self.stack[si.back(1).index()].stat_score / 98;
            bonus_scale += (59 * depth).min(420);
            bonus_scale += 186 * i32::from(self.stack[si.back(1).index()].move_count > 9);
            bonus_scale += 142
                * i32::from(!in_check && best_value <= self.stack[si.index()].static_eval - 106);
            bonus_scale += 159
                * i32::from(
                    !self.stack[si.back(1).index()].in_check
                        && best_value <= -self.stack[si.back(1).index()].static_eval - 68,
                );
            bonus_scale = bonus_scale.max(0);

            let scaled_bonus = (150 * depth - 85).min(1337) * bonus_scale;

            let pc = self.pos.piece_on(prev_sq);
            self.update_continuation_histories(
                si.back(1),
                pc,
                prev_sq,
                Bonus::new(scaled_bonus * 263 / 16384),
            );

            let prev_move = self.stack[si.back(1).index()].current_move;
            self.histories.main.update(
                !us,
                prev_move.raw(),
                Bonus::new(scaled_bonus * 215 / 32768),
            );

            if self.pos.piece_on(prev_sq).piece_type() != PieceType::Pawn
                && prev_move.move_type() != crate::board::types::MoveType::Promotion
            {
                let pawn_row = super::history::PawnHistory::row(self.pos.st().pawn_key);
                self.histories.pawn.update(
                    pawn_row,
                    pc,
                    prev_sq,
                    Bonus::new(scaled_bonus * 324 / 8192),
                );
            }
        } else if let Some(prev_sq) = prev_sq.square()
            && prior_capture
        {
            let captured = self.pos.captured_piece();
            let pc = self.pos.piece_on(prev_sq);
            self.histories.captures.update(pc, prev_sq, captured.piece_type(), Bonus::new(892));
        }

        if N::PV {
            best_value = best_value.min(max_value);
        }

        // A node that found nothing good, below a node that was on the principal
        // variation, is probably where the opponent's good move led. Mark it so the table
        // treats it as important next time.
        if best_value <= alpha {
            self.stack[si.index()].tt_pv =
                self.stack[si.index()].tt_pv || self.stack[si.back(1).index()].tt_pv;
        }

        if excluded_move.is_none() && !(N::ROOT && self.pv_index > 0) {
            let bound = if best_value >= beta {
                Bound::Lower
            } else if N::PV && best_move.is_some() {
                Bound::Exact
            } else {
                Bound::Upper
            };
            let store_depth =
                if move_count != 0 { depth } else { (depth + 6).min(MAX_PLY as i32 - 1) };
            tt.store(
                probe,
                best_move,
                value_to_tt(best_value, ply),
                unadjusted_static_eval,
                store_depth,
                bound,
                self.stack[si.index()].tt_pv,
            );
        }

        // Feed the static evaluation's error back into the correction tables, so a position
        // shaped like this one starts closer to the truth next time. Only when the sign of
        // the error agrees with whether a move improved on the evaluation at all —
        // otherwise the difference is not the evaluation's fault.
        let best_was_capture = best_move.is_some() && self.pos.is_capture(best_move);
        if !in_check
            && !best_was_capture
            && (best_value > self.stack[si.index()].static_eval) == best_move.is_some()
        {
            let limit = super::history::CORRECTION_LIMIT;
            let scale = if best_move.is_none() { 18 } else { 12 };
            let bonus = Bonus::new(
                ((best_value - self.stack[si.index()].static_eval) * depth * scale / 128)
                    .clamp(-limit / 4, limit / 4),
            );
            self.update_correction_history(si, bonus * 1061 / 1024);
        }

        best_value
    }

    /// Record what a root move's search found.
    fn update_root_move(
        &mut self,
        mv: Move,
        value: Value,
        alpha: Value,
        beta: Value,
        node_count: u64,
        move_count: i32,
        si: StackIx,
    ) {
        // An exponential moving average whose weight depends on how much of the tree this
        // one search contributed. A move whose first search was most of its total effort
        // gets a heavy weight; one that has been searched many times already gets a light
        // one, so a single deep re-search cannot swamp its own history.
        const SCALE: u64 = 32;
        const CHI_NUM: u64 = 3;
        const CHI_DEN: u64 = 2;
        const MIN_WEIGHT: u64 = 12;
        const MAX_WEIGHT: u64 = 24;

        let idx = self
            .root_moves
            .iter()
            .position(|rm| rm.mv == mv)
            .expect("a root move that was searched is in the list");

        let n = self.nodes - node_count;
        self.root_moves[idx].effort += n;

        let e_prev = (self.root_moves[idx].effort - n).max(1);
        let w = ((SCALE * n * CHI_DEN) / (n * CHI_DEN + CHI_NUM * e_prev))
            .clamp(MIN_WEIGHT, MAX_WEIGHT);
        let w_mss = w.min(16);
        let v2 = i64::from(value) * i64::from(value.abs());

        // Both averages are computed the way upstream computes them: in UNSIGNED 64-bit
        // arithmetic, because a signed operand multiplied by a `u64` converts to unsigned
        // in C++. The sum still wraps to the mathematically right bit pattern, but the
        // division that follows floors rather than truncating, and the final narrowing to
        // 32 bits is a truncation rather than a clamp. Both are visible in the aspiration
        // window that reads these, so both are reproduced.
        if self.root_moves[idx].average_score == -VALUE_INFINITE {
            self.root_moves[idx].average_score = value;
        } else {
            let avg = self.root_moves[idx].average_score;
            let sum = (value.get() as i64 as u64)
                .wrapping_mul(w)
                .wrapping_add((avg.get() as i64 as u64).wrapping_mul(SCALE - w));
            self.root_moves[idx].average_score = Value::new((sum / SCALE) as u32 as i32);
        }

        if self.root_moves[idx].mean_squared_score == MEAN_SQUARED_SENTINEL {
            self.root_moves[idx].mean_squared_score = Value::new(v2 as u32 as i32);
        } else {
            let mss = self.root_moves[idx].mean_squared_score;
            let sum = (v2 as u64)
                .wrapping_mul(w_mss)
                .wrapping_add((mss.get() as i64 as u64).wrapping_mul(SCALE - w_mss));
            self.root_moves[idx].mean_squared_score = Value::new((sum / SCALE) as u32 as i32);
        }

        if move_count == 1 || value > alpha {
            self.root_moves[idx].score = value;
            self.root_moves[idx].uci_score = value;
            self.root_moves[idx].sel_depth = self.sel_depth;
            self.root_moves[idx].unset_bound_flags();

            if value >= beta {
                self.root_moves[idx].score_lowerbound = true;
                self.root_moves[idx].uci_score = beta;
            } else if value <= alpha {
                self.root_moves[idx].score_upperbound = true;
                self.root_moves[idx].uci_score = alpha;
            }

            let child = core::mem::take(&mut self.stack[si.next().index()].pv);
            let child_valid = self.stack[si.next().index()].pv_valid;
            self.root_moves[idx].pv.truncate(1);
            self.root_moves[idx].pv[0] = mv;
            if child_valid {
                self.root_moves[idx].pv.extend_from_slice(&child);
            }
            self.stack[si.next().index()].pv = child;

            if move_count > 1 && self.pv_index == 0 {
                self.best_move_changes += 1.0;
            }
        } else {
            // Every move but the PV is set to the lowest value. The sort is stable, so the
            // rest keep their order and only the PV is pushed up.
            self.root_moves[idx].score = -VALUE_INFINITE;
        }
    }

    /// The quiescence search: play out the forcing moves so the evaluation is not called on
    /// a position where a piece is hanging.
    #[allow(clippy::too_many_lines)]
    fn qsearch<Q: QuiescentKind>(
        &mut self,
        mut alpha: Value,
        beta: Value,
        ply: Ply,
        tt: &TranspositionTable,
    ) -> Value {
        // A repetition available here is worth at least a draw, exactly as in the main
        // search.
        if alpha < VALUE_DRAW && self.pos.upcoming_repetition(ply) {
            alpha = value_draw(self.nodes);
            if alpha >= beta {
                return alpha;
            }
        }

        let si = StackIx::from_ply(ply);

        // Step 1. Initialize node
        if Q::PV {
            self.stack[si.next().index()].pv.clear();
            self.stack[si.next().index()].pv_valid = true;
            self.stack[si.index()].pv.clear();
        }

        let mut best_move = Move::NONE;
        self.stack[si.index()].in_check = self.pos.in_check();
        let in_check = self.stack[si.index()].in_check;
        let mut move_count = 0i32;

        if Q::PV && self.sel_depth < ply.next().get() {
            self.sel_depth = ply.next().get();
        }

        // Step 2. Check for an immediate draw or maximum ply reached
        if self.pos.is_draw(ply) || ply >= Ply::MAX {
            return if ply >= Ply::MAX && !in_check { self.evaluate(ply) } else { VALUE_DRAW };
        }

        // Step 3. Transposition table lookup
        let pos_key = self.pos.key();
        let probe = tt.probe(pos_key);
        let tt_hit = probe.hit;
        self.stack[si.index()].tt_hit = tt_hit;
        let tt_move = if tt_hit { probe.data.mv } else { Move::NONE };
        let tt_value = if tt_hit {
            value_from_tt(probe.data.value, ply, self.pos.rule50_count())
        } else {
            VALUE_NONE
        };
        let pv_hit = tt_hit && probe.data.is_pv;

        if !Q::PV
            && probe.data.depth >= DEPTH_QS
            && is_valid(tt_value)
            && match probe.data.bound {
                Bound::Exact => true,
                Bound::Lower => tt_value >= beta,
                Bound::Upper => tt_value < beta,
                Bound::None => false,
            }
        {
            return tt_value;
        }

        // Step 4. Static evaluation of the position
        let mut unadjusted_static_eval = VALUE_NONE;
        let mut best_value;
        let futility_base;

        if in_check {
            // In check there is no standing pat: the side to move must answer the check.
            best_value = -VALUE_INFINITE;
            futility_base = -VALUE_INFINITE;
        } else {
            let correction_value = self.correction_value(si);

            if tt_hit {
                unadjusted_static_eval = probe.data.eval;
                if !is_valid(unadjusted_static_eval) {
                    unadjusted_static_eval = self.evaluate(ply);
                }
                self.stack[si.index()].static_eval =
                    Self::to_corrected_static_eval(unadjusted_static_eval, correction_value);
                best_value = self.stack[si.index()].static_eval;

                if is_valid(tt_value)
                    && !is_decisive(tt_value)
                    && match probe.data.bound {
                        Bound::Exact => true,
                        Bound::Lower => tt_value > best_value,
                        Bound::Upper => tt_value <= best_value,
                        Bound::None => false,
                    }
                {
                    best_value = tt_value;
                }
            } else {
                unadjusted_static_eval = self.evaluate(ply);
                self.stack[si.index()].static_eval =
                    Self::to_corrected_static_eval(unadjusted_static_eval, correction_value);
                best_value = self.stack[si.index()].static_eval;
            }

            // Stand pat: the side to move is not obliged to capture, so the static
            // evaluation is a lower bound on what it can get.
            if best_value >= beta {
                if !is_decisive(best_value) {
                    // Blended toward beta rather than returned raw: nothing below this node
                    // was searched, so the full margin was never proved.
                    best_value = Value::new((441 * best_value.get() + 583 * beta.get()) / 1024);
                }
                if !tt_hit {
                    tt.store(
                        probe,
                        Move::NONE,
                        VALUE_NONE,
                        unadjusted_static_eval,
                        DEPTH_UNSEARCHED,
                        Bound::Lower,
                        false,
                    );
                }
                return best_value;
            }

            if best_value > alpha {
                alpha = best_value;
            }

            futility_base = self.stack[si.index()].static_eval + 306;
        }

        // Only plane zero is ever read here: quiescence reaches `score_evasions` at most and
        // never `QuietInit`. See `movepick::ContKey::UNREAD`.
        let cont_keys: ContKeys = [
            self.stack[si.back(1).index()].continuation,
            ContKey::UNREAD,
            ContKey::UNREAD,
            ContKey::UNREAD,
            ContKey::UNREAD,
            ContKey::UNREAD,
        ];
        let prev_sq = if self.stack[si.back(1).index()].current_move.is_ok() {
            self.stack[si.back(1).index()].current_move.to().some()
        } else {
            SquareOrNone::NONE
        };

        let slot = SLOTS_PER_PLY * ply.index() + SLOT_QSEARCH;
        let mut mp = MovePicker::new(&self.pos, cont_keys, tt_move, DEPTH_QS, ply);

        // Step 5. Loop through all pseudo-legal moves until no moves remain or a beta
        // cutoff occurs.
        loop {
            let mv = mp.next_move(&self.pos, &self.histories, &mut self.move_pool[slot]);
            if mv.is_none() {
                break;
            }
            if !self.pos.legal(mv) {
                continue;
            }

            let gives_check = self.pos.gives_check(mv);
            let capture = self.pos.is_capture_stage(mv);
            move_count += 1;

            // Step 6. Pruning
            if !is_loss(best_value) {
                if !gives_check
                    && mv.to().some() != prev_sq
                    && !is_loss(futility_base)
                    && mv.move_type() != crate::board::types::MoveType::Promotion
                {
                    // Past the second move the ordering has already offered the best
                    // captures; the rest are very unlikely to change the answer.
                    if move_count > 2 {
                        continue;
                    }

                    let futility_value =
                        futility_base + piece_value(self.pos.piece_on(mv.to())).get();
                    if futility_value <= alpha {
                        best_value = best_value.max(futility_value);
                        continue;
                    }

                    if !self.pos.see_ge(mv, Value::new(alpha - futility_base)) {
                        best_value = best_value.max(alpha.min(futility_base));
                        continue;
                    }
                }

                if !capture {
                    continue;
                }

                if !self.pos.see_ge(mv, Value::new(-74)) {
                    continue;
                }
            }

            // Step 7. Make and search the move
            self.do_move(mv, gives_check, si);
            let value = -self.qsearch::<Q>(-beta, -alpha, ply.next(), tt);
            self.undo_move(mv);

            // Step 8. Check for a new best move
            if value > best_value {
                best_value = value;

                if value > alpha {
                    best_move = mv;

                    if Q::PV {
                        let child = core::mem::take(&mut self.stack[si.next().index()].pv);
                        let child_valid = self.stack[si.next().index()].pv_valid;
                        self.stack[si.index()].pv.clear();
                        self.stack[si.index()].pv.push(mv);
                        if child_valid {
                            self.stack[si.index()].pv.extend_from_slice(&child);
                        }
                        self.stack[si.next().index()].pv = child;
                    }

                    if value < beta {
                        alpha = value;
                    } else {
                        break;
                    }
                }
            }
        }

        // Step 9. Check for mate and stalemate
        if move_count == 0 {
            if in_check {
                // Checkmate. No legality filter is needed: the evasion generator emits
                // every escape, so an empty list is proof.
                return mated_in(ply);
            }

            // Stalemate is only worth testing when the side to move has almost nothing:
            // a full legal-move generation at every quiescence leaf would cost far more
            // than the rare stalemate is worth.
            let us = self.pos.side_to_move();
            let pawns = self.pos.pieces_of(us, PieceType::Pawn);
            let blocked = (pawn_single_push(us, pawns) & !self.pos.occupied()).is_empty();
            if blocked
                && self.pos.non_pawn_material(us) == 0
                && self.pos.captured_piece().piece_type().index() >= PieceType::Knight.index()
                && !has_legal_move(&self.pos)
            {
                best_value = VALUE_DRAW;
            }
        }

        if !is_decisive(best_value) && best_value > beta {
            best_value = Value::new((462 * best_value.get() + 562 * beta.get()) / 1024);
        }

        tt.store(
            probe,
            best_move,
            value_to_tt(best_value, ply),
            unadjusted_static_eval,
            DEPTH_QS,
            if best_value >= beta { Bound::Lower } else { Bound::Upper },
            pv_hit,
        );

        best_value
    }

    /// Replace this node with a tablebase verdict, when one is available.
    fn probe_tablebases(
        &mut self,
        depth: i32,
        ply: Ply,
        alpha: Value,
        beta: Value,
        tt: &TranspositionTable,
        probe: super::tt::TTProbe,
    ) -> Option<TbOutcome> {
        let tb = self.tablebases.as_ref()?;
        let pieces = self.pos.piece_total();
        if pieces > self.tb_cardinality
            || (pieces == self.tb_cardinality && depth < self.tb_probe_depth)
        {
            return None;
        }
        // Only at a zeroed halfmove clock, and with no castling rights: a tablebase knows
        // the distance to the next irreversible move, not to mate, and it models no
        // castling at all.
        if self.pos.rule50_count() != 0
            || self.pos.can_castle(crate::board::types::CastlingRights::ANY)
        {
            return None;
        }

        // Upstream forces the next node to check the clock, whether or not the probe
        // succeeded: a probe READS A FILE, which is orders of magnitude slower than the node
        // it replaces, so the countdown that normally paces the check has just become a bad
        // estimate of elapsed time. `search.cpp` does this immediately after `probe_wdl`
        // and before testing the result, and rfish did not do it at all -- so a search with
        // tablebases could run well past its budget between two checks.
        //
        // BEFORE the probe here rather than after it, because upstream resets whatever the
        // probe returned and the `?` below leaves on a failure: a failed probe has already
        // paid the file read, which is the whole reason for the reset.
        if self.id == 0 {
            self.calls_cnt = 0;
        }
        let wdl = tb.probe_wdl(&self.pos).ok()?;
        self.tb_hits += 1;

        // `Syzygy50MoveRule` off means a cursed win counts as a win: the caller has told us
        // the fifty-move rule does not apply to this game.
        let draw_score = i32::from(self.tb_use_rule50.applies());
        let w = wdl as i32;
        let tb_value = VALUE_TB - ply.get();

        let value = if w < -draw_score {
            -tb_value
        } else if w > draw_score {
            tb_value
        } else {
            VALUE_DRAW + 2 * w * draw_score
        };

        let bound = if w < -draw_score {
            Bound::Upper
        } else if w > draw_score {
            Bound::Lower
        } else {
            Bound::Exact
        };

        if bound == Bound::Exact
            || (if bound == Bound::Lower { value >= beta } else { value <= alpha })
        {
            // Stored at a depth that outranks anything the search could reach here: the
            // answer is exact, so nothing deeper can improve on it.
            tt.store(
                probe,
                Move::NONE,
                value_to_tt(value, ply),
                VALUE_NONE,
                (depth + 6).min(MAX_PLY as i32 - 1),
                bound,
                self.stack[STACK_BASE + ply.index()].tt_pv,
            );
            return Some(TbOutcome::Cutoff(value));
        }

        match bound {
            Bound::Lower => Some(TbOutcome::LowerBound(value)),
            _ => Some(TbOutcome::UpperBound(value)),
        }
    }

    /// Order the root moves by their tablebase verdict, when the tables cover the root.
    ///
    /// This decides the game rather than informing it. Every move that preserves the
    /// result ranks above every move that throws it away, so the search below is choosing
    /// between moves that are all still winning — and because the tables have already
    /// answered, the in-search probe is switched off for the rest of the move.
    ///
    /// Castling rights disqualify the root: Syzygy tables model no castling, so a position
    /// that still has rights is not the position the table describes.
    fn rank_root_moves(&mut self) {
        self.tb_cardinality = self.tb_probe_limit;
        self.root_in_tb = false;

        let Some(tb) = self.tablebases.clone() else {
            self.tb_cardinality = 0;
            return;
        };
        let max = tb.max_cardinality();
        if max == 0 {
            self.tb_cardinality = 0;
            return;
        }
        if self.tb_cardinality > max {
            // Below the found tables' limit every probe is cheap, so stop gating on depth.
            self.tb_cardinality = max;
            self.tb_probe_depth = 0;
        }
        if self.tb_cardinality < self.pos.piece_total()
            || self.pos.can_castle(crate::board::types::CastlingRights::ANY)
        {
            return;
        }

        // When mate is the only zeroing move, DTZ IS distance to mate, so ranking by it
        // costs nothing and produces the shortest win rather than any win.
        let rank_dtz = if self.pos.dtz_is_dtm() {
            crate::platform::syzygy::RankDtz::Yes
        } else {
            crate::platform::syzygy::RankDtz::No
        };
        let mut dtz_available = true;
        let mut ranked = tb.root_probe(&self.pos, self.tb_use_rule50, rank_dtz);
        if ranked.is_none() {
            // The DTZ tables are missing. WDL still says who wins.
            dtz_available = false;
            ranked = tb.root_probe_wdl(&self.pos, self.tb_use_rule50);
        }
        let Some(ranked) = ranked else { return };

        self.root_in_tb = true;
        for rm in &mut self.root_moves {
            if let Some(r) = ranked.iter().find(|r| r.mv == rm.mv) {
                rm.tb_rank = r.rank;
                rm.tb_score = r.score;
            }
        }
        // Stable, so moves the tables rank equally keep the movegen order and the result
        // does not depend on the sort's internals.
        self.root_moves.sort_by_key(|rm| core::cmp::Reverse(rm.tb_rank));

        // Keep probing during the search only when WDL answered AND the root is not
        // winning: that is the one case where the ranking cannot finish the game by itself.
        if dtz_available || self.root_moves[0].tb_score <= VALUE_DRAW {
            self.tb_cardinality = 0;
        }
    }

    /// Hand this worker's new nodes and tablebase hits to the pool totals.
    fn publish_counters(&mut self) {
        let nodes = self.nodes - self.published_nodes;
        if nodes != 0 {
            self.shared.add_nodes(nodes);
            self.published_nodes = self.nodes;
        }
        let hits = self.tb_hits - self.published_tb_hits;
        if hits != 0 {
            self.shared.add_tb_hits(hits);
            self.published_tb_hits = self.tb_hits;
        }
    }

    /// The POOL's node count, with this worker's own contribution brought fully up to date.
    ///
    /// Upstream reads `threads.nodes_searched()`, a sum over every worker, for the number it
    /// reports, the number a `nodes` limit is checked against and the number `nodestime`
    /// converts a clock into. Reading only the reporting worker's own count made all three
    /// wrong by a factor of the thread count: `go nodes 1000` with four threads searched
    /// four thousand.
    fn pool_nodes(&mut self) -> u64 {
        self.publish_counters();
        self.shared.node_count().max(self.nodes)
    }

    /// Stop the search when a limit has been reached.
    ///
    /// Checked on a countdown rather than every node: reading a clock is a syscall on some
    /// platforms, and at a few million nodes per second the granularity is well under a
    /// millisecond either way. Under a node limit the countdown shortens, so the limit is
    /// honoured to within a tenth of a per cent rather than to within 512 nodes.
    fn check_time(&mut self) {
        self.calls_cnt -= 1;
        if self.calls_cnt > 0 {
            return;
        }
        self.calls_cnt = match self.limits.nodes {
            Some(n) => 512.min((n / 1024) as i32).max(1),
            None => 512,
        };

        // Pondering is thinking on the opponent's clock: nothing here can end it, only the
        // GUI can. The flag that says the budget ran out is honoured at `ponderhit`.
        if self.shared.pondering() {
            return;
        }

        let nodes = self.pool_nodes();
        let elapsed = self.budget.elapsed(nodes);

        let out_of_time = self.limits.uses_time_management()
            && (elapsed > self.budget.maximum() || self.shared.stop_on_ponderhit());
        // `move_time` is in the SAME unit as `elapsed` by the time this reads it, and
        // `TimeManagement::init` is what guarantees it: under `nodestime` it converts the
        // clock, the increment, the overhead and `movetime` together, so both sides of this
        // comparison are nodes, or both are milliseconds. The cast is the widening the unit
        // type asks for, not a unit change.
        let past_move_time =
            self.limits.move_time.is_some_and(|mt| elapsed >= Elapsed::new(mt as i64));
        let past_nodes = self.limits.nodes.is_some_and(|n| nodes >= n);

        if out_of_time || past_move_time || past_nodes {
            self.shared.request_stop();
        }
    }

    /// Trim the reported PV to what the tables vouch for, then walk it out to mate.
    ///
    /// A search that hands back "this is won" and a five-move line has answered the wrong
    /// question: the operator wants to see the win. The tables can supply the rest, and
    /// they can also expose the opposite problem — a PV whose later moves quietly throw the
    /// result away, which the search never noticed because it stopped believing the score
    /// once the tablebase supplied it.
    ///
    /// The result is a plausible continuation, NOT a proven mating line: only for simple
    /// endgames like `KRvK` is minimal DTZ also minimal distance to mate.
    fn syzygy_extend_pv(&mut self, v: &mut Value) {
        let Some(tb) = self.tablebases.clone() else { return };
        let rule50 = self.tb_use_rule50.applies();
        let start = Instant::now();

        // Never spend more than half the move overhead on this. It is presentation, and a
        // game lost on time because the PV looked nice is not a trade worth making.
        let overhead = self.opts.move_overhead;
        let uses_clock = self.limits.uses_time_management();
        let timed_out =
            |start: &Instant| uses_clock && 2 * start.elapsed().as_millis() as u64 > overhead;

        let mut pos = self.pos.clone();
        let mut pv = self.root_moves[self.pv_index].pv.clone();
        if pv.is_empty() {
            return;
        }

        // Step 0: play the root move itself, with no correction. `MultiPV` reports lines
        // that are not the best one, and correcting this move would report a different one.
        pos.do_move(pv[0]);
        let mut ply = 1usize;

        // Step 1: walk while the tables still rank each PV move top.
        while ply < pv.len() {
            let pv_move = pv[ply];
            let Some(ranked) =
                self.rank_for_extension(&tb, &pos, crate::platform::syzygy::RankDtz::No)
            else {
                break;
            };
            let Some(best) = ranked.iter().map(|r| r.rank).max() else { break };
            let Some(this) = ranked.iter().find(|r| r.mv == pv_move).map(|r| r.rank) else { break };
            if best != this {
                break;
            }

            ply += 1;
            pos.do_move(pv_move);

            // A repetition or a fifty-move draw inside a won line is not part of the win.
            // `ply` indexes the PV here AND counts plies from the root, and those are the
            // same number only because the walk starts at the root. Say which one the
            // predicate wants.
            let from_root = Ply::new(ply as i32);
            if (rule50 && pos.is_draw(from_root)) || pos.is_repetition(from_root) {
                pos.undo_move(pv_move);
                ply -= 1;
                break;
            }
            if timed_out(&start) {
                break;
            }
        }
        pv.truncate(ply);

        // Step 2: extend by playing the shortest win each time, as a reader of the tables
        // would.
        //
        // **NOT BOUNDED IN PLIES.** A won line may run past `MAX_PLY` — upstream's
        // `RootPVMoves` is a growable vector for exactly this loop — so a cap here would
        // print a truncated win where the golden prints the whole one. Only a table set
        // larger than the one this repository ships can tell the two apart.
        //
        // What holds the loop is therefore the same set upstream relies on: the mate test,
        // the draw test under `Syzygy50MoveRule`, the move-overhead deadline under time
        // management, and a DTZ that shortens on every step. The last of those is a claim
        // about FILE DATA — a table that ranks a repetition top loops here, pushing a move
        // and a state per iteration — and the Syzygy reader one zone over is what refuses
        // such a file.
        while !(rule50 && pos.is_draw(Ply::ROOT)) {
            if timed_out(&start) {
                break;
            }
            let legal = generate_legal(&pos);
            if legal.is_empty() {
                break; // Mate: the line is complete.
            }

            // Seed each move with how much it restricts the opponent, counting a reply that
            // captures as far worse than one that does not.
            let mut cand: Vec<(Move, i32)> = Vec::with_capacity(legal.len());
            for &m in legal.as_slice() {
                pos.do_move(m);
                let penalty: i32 = generate_legal(&pos)
                    .iter()
                    .map(|&opp| if pos.is_capture(opp) { 100 } else { 1 })
                    .sum();
                pos.undo_move(m);
                cand.push((m, -penalty));
            }
            // Order by that first, so the DTZ sort below — which is stable — keeps it as
            // the tie-break between moves the tables rank equally.
            cand.sort_by_key(|&(_, pen)| core::cmp::Reverse(pen));

            // Ranked by distance this time: the winner wants the shortest win, and the
            // loser the longest defeat, which is the same ordering seen from each side.
            let Some(ranked) =
                self.rank_for_extension(&tb, &pos, crate::platform::syzygy::RankDtz::Yes)
            else {
                break;
            };
            cand.sort_by_key(|&(m, _)| {
                core::cmp::Reverse(ranked.iter().find(|r| r.mv == m).map_or(i32::MIN, |r| r.rank))
            });

            let chosen = cand[0].0;
            pv.push(chosen);
            pos.do_move(chosen);
        }

        // Reaching a draw here is exceptional, and only possible when the position was set
        // up with a fifty-move counter the tables cannot rank exactly. The score follows the
        // line actually found rather than the one the search believed.
        if pos.is_draw(Ply::ROOT) {
            *v = VALUE_DRAW;
        }

        self.root_moves[self.pv_index].pv = pv;
    }

    /// Rank every legal move of `pos`, for the PV extension only.
    ///
    /// Returns `None` unless the DTZ tables answer for the whole position, because a
    /// partially ranked list would let the extension play a move nothing vouched for.
    fn rank_for_extension(
        &self,
        tb: &crate::platform::syzygy::TableRegistry,
        pos: &Position,
        rank_dtz: crate::platform::syzygy::RankDtz,
    ) -> Option<Vec<crate::platform::syzygy::RankedRootMove>> {
        if pos.piece_total() > tb.max_cardinality()
            || pos.can_castle(crate::board::types::CastlingRights::ANY)
        {
            return None;
        }
        // The OR is upstream's, and it lives inside `rank_root_moves` there -- so it applies
        // to EVERY caller, not just the root ranking. When mate is the only zeroing move,
        // DTZ is distance to mate, so ranking by it costs nothing and distinguishes a
        // shorter win from a longer one. Without it the PV walk in step 1 finds every win
        // equally top-ranked, never truncates, and shows the search's line where upstream
        // shows the tables' shortest.
        tb.root_probe(
            pos,
            self.tb_use_rule50,
            if rank_dtz.wanted() || pos.dtz_is_dtm() {
                crate::platform::syzygy::RankDtz::Yes
            } else {
                crate::platform::syzygy::RankDtz::No
            },
        )
    }

    /// Emit one `info` line for the current PV slot.
    fn report(
        &mut self,
        depth: i32,
        _best_value: Value,
        tt: &TranspositionTable,
        sink: &mut dyn InfoSink,
    ) {
        // EVERY multipv line, not just the one that finished. Upstream re-prints the whole
        // set each time it reports, so a GUI in MultiPV mode sees all of them at every
        // depth; reporting only `pv_index` published the last line and nothing else.
        //
        // The OPTION's value, not the searched count. `Skill Level` raises the searched
        // count to four so it has candidates to choose among, and upstream still reports the
        // one line the GUI asked for -- reporting the inflated number publishes three lines
        // nobody requested, at a strength the engine is not playing at.
        let lines = self.opts.multi_pv.min(self.root_moves.len());
        let elapsed = self.budget.wall_elapsed().get().max(1) as u64;
        let nodes = self.pool_nodes();
        let tb_hits = self.shared.tb_hit_count().max(self.tb_hits)
            + if self.root_in_tb { self.root_moves.len() as u64 } else { 0 };
        let hashfull = tt.hashfull(GENERATION_MAX_AGE);

        for i in 0..lines {
            // A move the current iteration has not scored yet is reported from the previous
            // one, at the previous depth. At depth one there IS no previous iteration, so
            // upstream prints only the first such line.
            let use_previous = self.root_moves[i].score == -VALUE_INFINITE;
            if depth == 1 && use_previous && i > 0 {
                continue;
            }
            let d = if use_previous { (depth - 1).max(1) } else { depth };
            let mut score = if use_previous {
                self.root_moves[i].previous_score
            } else {
                self.root_moves[i].uci_score
            };
            if score == -VALUE_INFINITE {
                score = VALUE_ZERO;
            }

            let bound = if self.root_moves[i].score_lowerbound {
                Some(true)
            } else if self.root_moves[i].score_upperbound {
                Some(false)
            } else {
                None
            };

            // With the root in the tables, show what the tables say. A mate score is left
            // alone: a forced mate is more specific than "this is a win".
            let is_tb_score = self.root_in_tb && score.abs() < VALUE_MATE_IN_MAX_PLY;
            if is_tb_score {
                score = self.root_moves[i].tb_score;
            }

            // A proven win whose PV stops in mid-air tells the operator nothing about how it
            // is won. Walk it out while the tables can still vouch for every move -- but not
            // for a line carried over from the previous iteration, which was extended then.
            if is_decisive(score)
                && score.abs() < VALUE_MATE_IN_MAX_PLY
                && !use_previous
                && (bound.is_none() || is_tb_score)
            {
                self.pv_index = i;
                self.syzygy_extend_pv(&mut score);
            }

            // Rendered against the ROOT, because that is all the notation needs. Upstream is
            // `pv += UCIEngine::move(m, pos.is_chess960())` over the stored line, and
            // `move_to_uci` reads exactly one thing off the position it is given: whether the
            // game is Chess960. That is a property of the GAME and cannot change along a PV.
            //
            // This walked a CLONE instead, playing each move to render the next -- which cost
            // a position clone and a `do_move` per PV move (1443 of each over one `bench 16 1
            // 8`, found by `cargo xtask fingerprint`), and, worse, broke out of the loop at
            // the first move that failed a legality check. Upstream prints the stored line
            // WHOLE and validates nothing, so a PV that `syzygy_extend_pv` or the previous
            // iteration left with a move unplayable in the walked line came out SHORTER here
            // than upstream prints it.
            let rm = &self.root_moves[i];
            let source = if use_previous { &rm.previous_pv } else { &rm.pv };
            let pv: Vec<String> = source.iter().map(|&m| move_to_uci(&self.pos, m)).collect();

            let report = DepthReport {
                depth: d,
                // The move's own selective depth, not the worker's running maximum: the two
                // differ once a later root move has been searched deeper than this one.
                sel_depth: rm.sel_depth,
                multi_pv: i + 1,
                score: Score::new(score, &self.pos),
                wdl: super::score::wdl(score, &self.pos),
                bound,
                nodes,
                nps: nodes * 1000 / elapsed,
                hashfull,
                tb_hits,
                time_ms: elapsed,
                pv: &pv,
            };
            sink.depth_finished(&report);
        }
    }
}

/// What a tablebase probe concluded about a node.
#[derive(Clone, Copy, Debug)]
enum TbOutcome {
    /// The verdict answers the node outright.
    Cutoff(Value),
    /// The verdict raises the floor but does not close the window.
    LowerBound(Value),
    /// The verdict caps what this node can return.
    UpperBound(Value),
}

/// Depth stored for an entry that was never searched, only evaluated.
const DEPTH_UNSEARCHED: i32 = -2;
/// Depth stored for a quiescence result.
const DEPTH_QS: i32 = 0;
/// How many generations an entry may be behind and still count toward `hashfull`.
const GENERATION_MAX_AGE: u8 = 0;

/// Single pawn pushes for `c`, before blockers are removed.
fn pawn_single_push(
    c: Color,
    pawns: crate::board::bitboard::Bitboard,
) -> crate::board::bitboard::Bitboard {
    pawns.shift(crate::board::types::Direction::pawn_push(c))
}

/// Mate score to `mate in N` in full moves, as the UCI `score mate` field wants it.
#[must_use]
pub fn score_to_uci(v: Value, pos: &Position) -> String {
    Score::new(v, pos).to_uci()
}

/// The colour whose clock a search reads.
#[must_use]
pub fn searching_side(pos: &Position) -> Color {
    pos.side_to_move()
}

#[cfg(test)]
mod tests {

    /// The continuation bonus wraps rather than trapping, at the width upstream uses.
    ///
    /// A pure function, deliberately: the input that reaches this is a real clock on a long
    /// search, and driving the binary to it costs two minutes and proves nothing this does
    /// not. Red before the wrap was spelled — the gate profile panicked on the multiply.
    #[test]
    fn the_continuation_bonus_wraps_the_way_upstream_does() {
        // The largest bonus the fail-low caller can hand over: `scaled_bonus * 263 / 16384`
        // once the histories have saturated.
        let bonus = Bonus::new(31_978);
        let (weight, multiplier) = (1040, 126);

        let got = SearchWorker::continuation_delta(bonus, weight, multiplier).get();
        let want = (i64::from(bonus.get()) * i64::from(weight) * i64::from(multiplier)) as i32;
        assert_eq!(got, want / 131_072, "the i32 product, then the divide");

        // The sign flip is the defect this reproduces rather than repairs: the intended
        // bonus is positive and what the tables receive is not.
        assert!(got < 0, "wrapped to {got}");

        // Every value the bench reaches is below the wrap, which is why no gated number
        // moves: the widened product and the wrapped one agree there.
        for b in [-2210, -1563, 1334, 16_000] {
            let narrow = SearchWorker::continuation_delta(Bonus::new(b), weight, multiplier).get();
            let wide = (i64::from(b) * i64::from(weight) * i64::from(multiplier) / 131_072) as i32;
            assert_eq!(narrow, wide, "bonus {b} fits and must not differ");
        }
    }
    use std::time::Instant;

    use super::*;
    use crate::board::position::START_FEN;

    fn search_fen(fen: &str, depth: i32) -> SearchResult {
        let pos = Position::from_fen(fen, false).expect("valid");
        let shared = SharedState::new();
        shared.reset();
        let tt = TranspositionTable::new(16);
        let mut w = SearchWorker::new(0, Arc::clone(&shared));
        let limits =
            Limits { depth: Some(depth), start: Some(Instant::now()), ..Limits::default() };
        w.search(
            &pos,
            &limits,
            &tt,
            &SearchOptions { multi_pv: 1, ..SearchOptions::default() },
            TimeBudget::default(),
            &mut SilentSink,
        )
    }

    /// A sink that keeps the `currmove` announcements, and nothing else.
    #[derive(Default)]
    struct CurrMoveSink {
        seen: Vec<(String, usize, i32)>,
    }

    impl InfoSink for CurrMoveSink {
        fn depth_finished(&mut self, _report: &DepthReport<'_>) {}
        fn current_move(&mut self, mv: &str, number: usize, depth: i32) {
            self.seen.push((mv.to_string(), number, depth));
        }
    }

    /// Drive one depth-1 iteration on thread 0 with the node counter PRESET, and collect
    /// what the root announced.
    ///
    /// `iterative_deepening` rather than `search`, because `search` zeroes the counter on
    /// the way in and the threshold this is about is ten million nodes -- forty seconds of
    /// searching to reach honestly, for a line decided before the first root move.
    /// Everything `search` sets up before it hands over is set up here instead.
    ///
    /// **Thread 0 only, and that is not an omission.** A helper ignores the depth limit by
    /// design -- upstream's loop guard reads `mainThread &&` -- so a helper driven this way
    /// searches until something stops it, which nothing here does. The `id` half of the
    /// rule is asserted directly against [`SearchWorker::announces_root_moves`] instead.
    fn announced_at(nodes: u64) -> Vec<(String, usize, i32)> {
        let shared = SharedState::new();
        shared.reset();
        let tt = TranspositionTable::new(16);
        let mut w = SearchWorker::new(0, Arc::clone(&shared));
        w.set_thread_count(1);
        w.clear();
        w.pos = Position::from_fen(START_FEN, false).expect("valid");
        w.limits = Limits { depth: Some(1), start: Some(Instant::now()), ..Limits::default() };
        w.opts = SearchOptions { multi_pv: 1, ..SearchOptions::default() };
        w.root_moves = generate_legal(&w.pos).iter().copied().map(RootMove::new).collect();
        w.nodes = nodes;
        let mut sink = CurrMoveSink::default();
        w.iterative_deepening(&tt, &mut sink);
        sink.seen
    }

    /// Upstream announces every root move it starts, once the search is long enough to be
    /// worth watching. rfish declared the hook and called it from nowhere, so the whole
    /// `info depth N currmove X currmovenumber M` line was missing from the port.
    #[test]
    fn the_root_announces_each_move_past_the_output_threshold() {
        let seen = announced_at(NODES_LIMIT_OUTPUT + 1);
        let legal = generate_legal(&Position::from_fen(START_FEN, false).expect("valid"));
        assert_eq!(seen.len(), legal.len(), "one announcement per root move");
        for (i, (mv, number, depth)) in seen.iter().enumerate() {
            // Numbered from one, in the order searched, and carrying the depth of the node
            // doing the searching -- not the iteration's nominal depth, which an aspiration
            // re-search lowers.
            assert_eq!(*number, i + 1, "root move {i} announced as number {number}");
            assert_eq!(*depth, 1);
            assert!(
                legal.iter().any(|m| format!("{m:?}") == *mv),
                "announced {mv}, which is not legal here"
            );
        }
    }

    /// The threshold at its boundary, which only a preset counter can reach.
    ///
    /// The comparison is `>`, so a worker sitting exactly ON the limit is still silent --
    /// and at depth one the start position spends exactly one node per root move, so the
    /// count crosses between the first move and the second. That makes the first move the
    /// one and only silent one: `>=` here would announce all twenty.
    #[test]
    fn the_announcement_starts_one_node_past_the_threshold() {
        let seen = announced_at(NODES_LIMIT_OUTPUT);
        assert_eq!(seen.len(), 19, "the first root move is announced below the threshold");
        assert_eq!(seen[0].1, 2, "the numbering counts root moves, not announcements");
    }

    /// Well below the threshold nothing is said at all. An engine that announced from node
    /// one is the excessive-output case the constant exists to prevent.
    #[test]
    fn nothing_is_announced_below_the_output_threshold() {
        assert!(announced_at(0).is_empty());
    }

    /// Only thread 0 announces. A pool whose helpers each announced their own root order
    /// would interleave several searches into one stream of `currmove` lines.
    #[test]
    fn only_the_main_thread_announces() {
        let shared = SharedState::new();
        let mut w = SearchWorker::new(1, Arc::clone(&shared));
        w.nodes = NODES_LIMIT_OUTPUT + 1;
        assert!(!w.announces_root_moves());
        let mut main = SearchWorker::new(0, shared);
        main.nodes = NODES_LIMIT_OUTPUT + 1;
        assert!(main.announces_root_moves());
    }

    #[test]
    fn a_search_returns_a_legal_move() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let r = search_fen(START_FEN, 6);
        assert!(r.best_move.is_ok());
        assert!(generate_legal(&pos).contains(&r.best_move));
        assert!(r.nodes > 0);
        assert_eq!(r.depth, 6);
    }

    #[test]
    fn mate_in_one_is_found_and_scored_as_mate() {
        // Back-rank mate: Ra8#.
        let r = search_fen("6k1/5ppp/8/8/8/8/8/R3K3 w - - 0 1", 6);
        assert_eq!(format!("{:?}", r.best_move), "a1a8");
        assert!(r.score >= VALUE_MATE - 3, "score {} is not a mate score", r.score);
        let pos = Position::from_fen("6k1/5ppp/8/8/8/8/8/R3K3 w - - 0 1", false).expect("valid");
        assert_eq!(score_to_uci(r.score, &pos), "mate 1");
    }

    #[test]
    fn a_forced_mate_several_moves_deep_is_found() {
        // Rf6-a6! and mate follows in three. The search must see past two intermediate
        // replies, which a shallow tactical trick cannot fake.
        let r = search_fen("r5rk/5p1p/5R2/4B3/8/8/7P/7K w - - 0 1", 12);
        assert!(r.score >= VALUE_MATE - 6, "score {} is not a forced mate", r.score);
        assert_eq!(format!("{:?}", r.best_move), "f6a6");
    }

    /// The node counter must count MOVES MADE, not nodes entered. Getting this backwards
    /// inflates every reported count, and the bench signature is the only thing that would
    /// notice.
    #[test]
    fn the_node_count_matches_the_moves_made() {
        let r = search_fen(START_FEN, 1);
        // At depth one the root's twenty moves are made, plus whatever quiescence needs.
        // The start position has no captures, so quiescence makes none.
        assert_eq!(r.nodes, 20);
    }

    #[test]
    fn the_null_move_margin_shift_is_the_division_under_the_max() {
        // The null-move reduction takes `max(margin / 256, 0)`, and `>> 8` is the same
        // value there for every margin two scores can produce: equal for a non-negative
        // margin, and at most zero for a negative one, which the max floors either way.
        // Checked by exhaustion over the whole reachable domain -- both operands are
        // bounded by VALUE_INFINITE, so the margin cannot leave twice it.
        let bound = 2 * crate::board::types::VALUE_INFINITE.get();
        for m in -bound..=bound {
            assert_eq!((m / 256).max(0), (m >> 8).max(0), "margin {m}");
        }
        // The two DISAGREE without the max, which is what makes moving it a bug rather
        // than a rearrangement.
        assert_ne!(-1i32 / 256, -1i32 >> 8);
    }

    #[test]
    fn the_multicut_correction_bonus_saturates_at_a_quarter_of_the_limit() {
        // Both ends of the clamp. A quarter of CORRECTION_LIMIT is the cap upstream chose,
        // and /4 read as /2 would let a single multi-cut move the table twice as far.
        let cap = CORRECTION_LIMIT / 4;
        assert_eq!(multicut_correction_bonus(Value::new(30_000), VALUE_ZERO, 64), cap);
        assert_eq!(multicut_correction_bonus(VALUE_ZERO, Value::new(30_000), 64), -cap);
    }

    #[test]
    fn the_multicut_correction_bonus_scales_with_the_singular_depth() {
        // The excess is measured from the static evaluation, and the depth it is weighted by
        // is the SINGULAR depth. Below the clamp the formula is exact, so pin it there.
        assert_eq!(multicut_correction_bonus(Value::new(100), Value::new(0), 1), 100 * 177 / 1024);
        assert_eq!(multicut_correction_bonus(Value::new(100), Value::new(0), 2), 200 * 177 / 1024);
        // Equal evaluation and value is no evidence at all.
        assert_eq!(multicut_correction_bonus(Value::new(50), Value::new(50), 8), 0);
        // 177/1024 and not 177/1000: the shift is a power of two.
        assert_ne!(
            multicut_correction_bonus(Value::new(1000), Value::new(0), 1),
            1000 * 177 / 1000
        );
    }
}
