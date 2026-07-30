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
//! # No I/O
//!
//! The search never prints. It reports through an [`InfoSink`], which the shell implements
//! by writing UCI `info` lines. That is what keeps the engine crate free of the transport
//! and lets a test drive a full search without capturing stdout.
//!
//! Golden: `Stockfish/src/search.cpp`.

use std::sync::Arc;
use std::time::Instant;

use crate::board::movegen::{generate_legal, move_to_uci};
use crate::board::position::Position;
use crate::board::types::{
    Bound, Color, MAX_PLY, Move, VALUE_DRAW, VALUE_INFINITE, VALUE_MATE, VALUE_MATE_IN_MAX_PLY,
    VALUE_MATED_IN_MAX_PLY, VALUE_NONE, Value, mated_in, piece_value,
};
use crate::eval;
use crate::eval::nnue::{Network, Scratch};
use crate::state::{Limits, RootMove, STACK_BASE, STACK_SIZE, SharedState, StackEntry, TimeBudget};

use super::history::{Histories, stat_bonus, stat_malus};
use super::movepick::{ContKey, MovePicker, continuation_to};
use super::timeman;
use super::tt::{TranspositionTable, value_from_tt, value_to_tt};

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
    /// A `currmove` update. The default does nothing: it is optional in the protocol and
    /// most callers do not want it.
    fn current_move(&mut self, _mv: &str, _number: usize, _depth: i32) {}
}

/// A no-op sink, for a search whose output nobody reads.
#[derive(Debug, Default)]
pub struct SilentSink;

impl InfoSink for SilentSink {
    fn depth_finished(&mut self, _report: &DepthReport<'_>) {}
}

/// Everything one completed iteration has to report.
#[derive(Debug)]
pub struct DepthReport<'a> {
    pub depth: i32,
    pub sel_depth: i32,
    pub multi_pv: usize,
    pub score: Value,
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
    stack: Vec<StackEntry>,
    root_moves: Vec<RootMove>,
    shared: Arc<SharedState>,
    limits: Limits,
    budget: TimeBudget,
    nodes: u64,
    sel_depth: i32,
    root_depth: i32,
    completed_depth: i32,
    pv_index: usize,
    multi_pv: usize,
    /// How often the best root move changed, decayed each iteration. High values buy time.
    best_move_changes: f64,
    optimism: [Value; 2],
    /// The network, shared read-only across every thread. `None` runs the classical
    /// fallback, which is what a run with no net on disk does.
    network: Option<Arc<Network>>,
    /// Per-thread evaluation buffers, so a search allocates nothing per node.
    scratch: Scratch,
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
    /// A worker with empty histories, ready for a `go`.
    #[must_use]
    pub fn new(id: usize, shared: Arc<SharedState>) -> SearchWorker {
        SearchWorker {
            id,
            pos: Position::startpos(),
            histories: Box::default(),
            stack: vec![StackEntry::default(); STACK_SIZE],
            root_moves: Vec::new(),
            shared,
            limits: Limits::default(),
            budget: TimeBudget {
                start: Instant::now(),
                optimum: std::time::Duration::MAX,
                maximum: std::time::Duration::MAX,
            },
            nodes: 0,
            sel_depth: 0,
            root_depth: 0,
            completed_depth: 0,
            pv_index: 0,
            multi_pv: 1,
            best_move_changes: 0.0,
            optimism: [0; 2],
            network: None,
            scratch: Scratch::default(),
        }
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

    /// The static evaluation of the worker's current position.
    #[inline]
    fn evaluate(&mut self) -> Value {
        let optimism = self.optimism[self.pos.side_to_move().index()];
        eval::evaluate(&self.pos, self.network.as_deref(), &mut self.scratch, optimism)
    }

    /// Forget every history. Called on `ucinewgame`, never mid-game.
    pub fn clear(&mut self) {
        self.histories.clear();
    }

    /// How many nodes this worker has searched.
    #[must_use]
    pub fn nodes(&self) -> u64 {
        self.nodes
    }

    /// Run one search to completion.
    ///
    /// Returns the best move and its score. Every thread runs this; only thread 0's return
    /// value is played, and only thread 0 reports through `sink`.
    pub fn search(
        &mut self,
        pos: &Position,
        limits: &Limits,
        tt: &TranspositionTable,
        multi_pv: usize,
        move_overhead: u64,
        sink: &mut dyn InfoSink,
    ) -> SearchResult {
        self.pos = pos.clone();
        self.limits = limits.clone();
        self.budget = timeman::allocate(limits, pos.side_to_move(), move_overhead);
        self.nodes = 0;
        self.sel_depth = 0;
        self.completed_depth = 0;
        self.best_move_changes = 0.0;
        for e in &mut self.stack {
            *e = StackEntry::default();
        }

        // Build the root move list, honouring `searchmoves` when the caller gave one.
        self.root_moves = generate_legal(&self.pos)
            .iter()
            .copied()
            .filter(|m| limits.search_moves.is_empty() || limits.search_moves.contains(m))
            .map(RootMove::new)
            .collect();

        if self.root_moves.is_empty() {
            // Checkmate or stalemate at the root: there is nothing to search, and the
            // protocol still expects a `bestmove`.
            return SearchResult {
                best_move: Move::NONE,
                ponder_move: None,
                score: if self.pos.in_check() { -VALUE_MATE } else { VALUE_DRAW },
                depth: 0,
                nodes: 0,
            };
        }

        self.multi_pv = multi_pv.clamp(1, self.root_moves.len());
        let max_depth = limits.depth.unwrap_or(MAX_PLY as i32 - 1).min(MAX_PLY as i32 - 1);

        let mut last_best = self.root_moves[0].mv;
        let mut root_depth = 1;
        while root_depth <= max_depth {
            if self.shared.stopped() {
                break;
            }
            self.root_depth = root_depth;
            for rm in &mut self.root_moves {
                rm.previous_score = rm.score;
            }
            self.best_move_changes *= 0.5;

            for pv in 0..self.multi_pv {
                self.pv_index = pv;
                self.sel_depth = 0;
                let score = self.aspiration(root_depth, tt);
                if self.shared.stopped() {
                    break;
                }
                // Sort only the moves at or after the current PV slot: the ones already
                // reported keep their place, which is what `MultiPV` output requires.
                self.root_moves[pv..].sort();
                if self.id == 0 {
                    self.report(root_depth, score, None, tt, sink);
                }
            }

            if !self.shared.stopped() {
                self.completed_depth = root_depth;
                if self.root_moves[0].mv != last_best {
                    self.best_move_changes += 1.0;
                    last_best = self.root_moves[0].mv;
                }
            }

            // Stop between iterations when the budget says the next one will not finish.
            if self.limits.uses_time_management() {
                let instability = 1.0 + 0.9 * self.best_move_changes;
                if !timeman::should_start_iteration(&self.budget, instability) {
                    break;
                }
            }
            if let Some(n) = self.limits.nodes
                && self.shared.node_count() >= n
            {
                break;
            }
            if let Some(mate) = self.limits.mate
                && self.root_moves[0].score >= VALUE_MATE - 2 * mate
            {
                break;
            }
            root_depth += 1;
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

    /// Search the root at `depth`, narrowing the window around the previous score.
    ///
    /// A narrow window cuts far more of the tree than a full one; when the true score
    /// falls outside it the search has to be redone, so the window widens on each failure
    /// rather than jumping straight back to infinity.
    fn aspiration(&mut self, depth: i32, tt: &TranspositionTable) -> Value {
        let mut delta = 10 + self.root_moves[self.pv_index].average_score.abs() / 12_000;
        let prev = self.root_moves[self.pv_index].average_score;
        let (mut alpha, mut beta) = if depth >= 4 && prev != -VALUE_INFINITE {
            ((prev - delta).max(-VALUE_INFINITE), (prev + delta).min(VALUE_INFINITE))
        } else {
            (-VALUE_INFINITE, VALUE_INFINITE)
        };

        let mut fail_high_count = 0;
        loop {
            let adjusted_depth = (depth - fail_high_count).max(1);
            let score = self.node::<true>(alpha, beta, adjusted_depth, 0, false, tt);
            if self.shared.stopped() {
                return score;
            }

            self.root_moves[self.pv_index..].sort();

            if score <= alpha && alpha > -VALUE_INFINITE {
                beta = (alpha + beta) / 2;
                alpha = (score - delta).max(-VALUE_INFINITE);
                fail_high_count = 0;
            } else if score >= beta && beta < VALUE_INFINITE {
                beta = (score + delta).min(VALUE_INFINITE);
                fail_high_count += 1;
            } else {
                return score;
            }
            delta += delta / 3;
        }
    }

    /// The alpha-beta node.
    ///
    /// `PV` distinguishes a node on the principal variation, which is searched with a real
    /// window and whose children must all be examined, from a zero-window node, where the
    /// first move that beats beta ends the node. Making it a const generic rather than a
    /// runtime flag lets the optimiser drop the PV-only bookkeeping from the zero-window
    /// instantiation, which is the overwhelming majority of nodes.
    fn node<const PV: bool>(
        &mut self,
        mut alpha: Value,
        beta: Value,
        mut depth: i32,
        ply: i32,
        cut_node: bool,
        tt: &TranspositionTable,
    ) -> Value {
        let root = ply == 0;
        if depth <= 0 {
            return self.qsearch::<PV>(alpha, beta, ply, tt);
        }

        self.nodes += 1;
        if self.nodes % 1024 == 0 {
            self.shared.add_nodes(1024);
            self.check_limits();
        }
        if self.shared.stopped() {
            return VALUE_ZERO_SENTINEL;
        }

        let si = STACK_BASE + ply as usize;
        if PV {
            self.sel_depth = self.sel_depth.max(ply + 1);
            self.stack[si].pv.clear();
        }

        if !root {
            // Draws and the ply ceiling end the node before anything else is computed.
            if self.pos.is_draw(ply) || eval::is_material_draw(&self.pos) {
                return VALUE_DRAW;
            }
            if ply >= MAX_PLY as i32 - 1 {
                return self.evaluate();
            }

            // Mate-distance pruning: a mate found closer to the root beats anything this
            // subtree could produce, so the window can be narrowed to the mate range.
            alpha = alpha.max(mated_in(ply));
            let beta = beta.min(VALUE_MATE - ply - 1);
            if alpha >= beta {
                return alpha;
            }
        }
        // Rebind so the mate-distance narrowing above applies below without shadowing
        // confusion at the root, where it does not.
        let beta = if root { beta } else { beta.min(VALUE_MATE - ply - 1) };

        let excluded = self.stack[si].excluded_move;
        let key = self.pos.key();
        let probe = tt.probe(key);
        let tt_hit = probe.hit && excluded.is_none();
        let tt_data = probe.data;
        let tt_move = if root {
            self.root_moves[self.pv_index].pv.first().copied().unwrap_or(Move::NONE)
        } else if tt_hit {
            tt_data.mv
        } else {
            Move::NONE
        };
        let tt_value = if tt_hit {
            value_from_tt(tt_data.value, ply, self.pos.rule50_count())
        } else {
            VALUE_NONE
        };

        // A stored score searched at least as deep as this node, whose bound is on the
        // right side of the window, answers the node outright.
        if !PV
            && tt_hit
            && tt_data.depth >= depth
            && tt_value != VALUE_NONE
            && match tt_data.bound {
                Bound::Exact => true,
                Bound::Lower => tt_value >= beta,
                Bound::Upper => tt_value <= alpha,
                Bound::None => false,
            }
        {
            return tt_value;
        }

        let in_check = self.pos.in_check();
        self.stack[si].in_check = in_check;
        self.stack[si].move_count = 0;

        // The static evaluation. In check there is none: every move is forced, so a
        // standing estimate would be meaningless and every eval-based pruning rule below
        // is skipped.
        let static_eval = if in_check {
            VALUE_NONE
        } else if tt_hit && tt_data.eval != VALUE_NONE {
            tt_data.eval
        } else {
            self.evaluate()
        };
        self.stack[si].static_eval = static_eval;

        // Is the position improving compared with two plies ago? A worsening position
        // deserves a more careful search, so several rules below are relaxed when it is.
        let improving = !in_check
            && ply >= 2
            && self.stack[si - 2].static_eval != VALUE_NONE
            && static_eval > self.stack[si - 2].static_eval;

        if !PV && !in_check && excluded.is_none() {
            // Reverse futility: if the position is already far enough above beta that even
            // giving away material could not bring it below, the opponent would never have
            // allowed this node.
            if depth < 9
                && static_eval - 90 * depth + 20 * i32::from(improving) >= beta
                && static_eval < VALUE_MATE_IN_MAX_PLY
            {
                return static_eval;
            }

            // Razoring: far below alpha at low depth, verify with a quiescence search
            // instead of a full one.
            if depth <= 3 && static_eval + 300 * depth < alpha {
                let v = self.qsearch::<false>(alpha - 1, alpha, ply, tt);
                if v < alpha {
                    return v;
                }
            }

            // Null-move pruning: give the opponent a free move; if the position still
            // beats beta, the real move would too. Skipped in a pawn endgame, where
            // zugzwang makes "pass" a genuinely bad option and the assumption fails.
            if depth >= 3
                && static_eval >= beta
                && self.pos.non_pawn_material(self.pos.side_to_move()) > 0
                && self.stack[si - 1].current_move != Move::NULL
            {
                let r = 4 + depth / 4 + ((static_eval - beta) / 200).min(3);
                self.pos.do_null_move();
                self.stack[si].current_move = Move::NULL;
                let v = -self.node::<false>(-beta, -beta + 1, depth - r, ply + 1, !cut_node, tt);
                self.pos.undo_null_move();
                if v >= beta && v < VALUE_MATE_IN_MAX_PLY {
                    return v;
                }
            }
        }

        // Internal iterative reduction: a PV or cut node with no transposition move has no
        // ordering to work with, so searching it at full depth mostly wastes the depth.
        if (PV || cut_node) && depth >= 6 && tt_move.is_none() {
            depth -= 2;
        }

        let continuations = self.continuation_keys(ply);
        let killers = self.stack[si].killers;
        let counter = Move::NONE;
        let mut picker =
            MovePicker::new(&self.pos, continuations, tt_move, killers, counter, depth);

        let mut best_value = -VALUE_INFINITE;
        let mut best_move = Move::NONE;
        let mut move_count = 0;
        let mut searched_quiets: Vec<Move> = Vec::new();
        let mut searched_captures: Vec<Move> = Vec::new();
        let mut skip_quiets = false;
        let original_alpha = alpha;

        while let Some(mv) = picker.next(&self.pos, &self.histories, skip_quiets) {
            if mv == excluded {
                continue;
            }
            if !self.pos.legal(mv) {
                continue;
            }
            if root && !self.root_moves[self.pv_index..].iter().any(|rm| rm.mv == mv) {
                continue;
            }
            move_count += 1;
            self.stack[si].move_count = move_count;

            let capture = self.pos.is_capture_stage(mv);
            let gives_check = self.pos.gives_check(mv);
            let moved = self.pos.piece_on(mv.from());

            // Late move pruning: past a depth-dependent count, the remaining quiet moves
            // are unlikely enough to matter that generating and searching them costs more
            // than it finds.
            if !root && !PV && best_value > VALUE_MATED_IN_MAX_PLY && !capture && !gives_check {
                if move_count >= (3 + depth * depth) / (2 - i32::from(improving)) {
                    skip_quiets = true;
                }
                // Futility: a quiet move at shallow depth that cannot lift the static
                // evaluation to alpha is not worth a node.
                if depth < 8
                    && !in_check
                    && static_eval != VALUE_NONE
                    && static_eval + 120 + 130 * depth <= alpha
                {
                    skip_quiets = true;
                    continue;
                }
                // A quiet move that loses material outright.
                if depth < 8 && !self.pos.see_ge(mv, -25 * depth * depth) {
                    continue;
                }
            }
            if !root && capture && depth < 7 && !self.pos.see_ge(mv, -180 * depth) {
                continue;
            }

            self.stack[si].current_move = mv;
            self.stack[si].moved_piece = moved;
            self.pos.do_move_checked(mv, gives_check);

            // Late move reductions: search a late, quiet move shallower, and re-search at
            // full depth only if it turns out to beat alpha. The reduction is what makes a
            // depth-30 search possible at all.
            let mut score;
            let new_depth = depth - 1;
            if depth >= 2 && move_count > 1 + u32::from(root) as i32 && !capture {
                let mut r = reduction(depth, move_count);
                if !PV {
                    r += 1;
                }
                if cut_node {
                    r += 1;
                }
                if improving {
                    r -= 1;
                }
                if gives_check {
                    r -= 1;
                }
                let d = (new_depth - r).clamp(1, new_depth);
                score = -self.node::<false>(-alpha - 1, -alpha, d, ply + 1, true, tt);
                if score > alpha && d < new_depth {
                    score =
                        -self.node::<false>(-alpha - 1, -alpha, new_depth, ply + 1, !cut_node, tt);
                }
            } else if !PV || move_count > 1 {
                score = -self.node::<false>(-alpha - 1, -alpha, new_depth, ply + 1, !cut_node, tt);
            } else {
                score = VALUE_ZERO_SENTINEL;
            }

            // A PV node re-searches with the real window whenever the zero-window probe
            // came back inside it: the zero window can prove a bound but not a value.
            if PV && (move_count == 1 || (score > alpha && (root || score < beta))) {
                score = -self.node::<true>(-beta, -alpha, new_depth, ply + 1, false, tt);
            }

            self.pos.undo_move(mv);

            if self.shared.stopped() {
                return VALUE_ZERO_SENTINEL;
            }

            if root {
                let idx = self
                    .root_moves
                    .iter()
                    .position(|rm| rm.mv == mv)
                    .expect("a root move that was searched is in the list");
                if move_count == 1 || score > alpha {
                    self.root_moves[idx].score = score;
                    self.root_moves[idx].sel_depth = self.sel_depth;
                    self.root_moves[idx].average_score =
                        if self.root_moves[idx].average_score == -VALUE_INFINITE {
                            score
                        } else {
                            (self.root_moves[idx].average_score + score) / 2
                        };
                    let child = std::mem::take(&mut self.stack[si + 1].pv);
                    self.root_moves[idx].pv =
                        core::iter::once(mv).chain(child.iter().copied()).collect();
                    self.stack[si + 1].pv = child;
                } else {
                    self.root_moves[idx].score = -VALUE_INFINITE;
                }
            }

            if score > best_value {
                best_value = score;
                if score > alpha {
                    best_move = mv;
                    if PV && !root {
                        let child = std::mem::take(&mut self.stack[si + 1].pv);
                        self.stack[si].pv.clear();
                        self.stack[si].pv.push(mv);
                        self.stack[si].pv.extend_from_slice(&child);
                        self.stack[si + 1].pv = child;
                    }
                    if score >= beta {
                        break;
                    }
                    alpha = score;
                }
            }

            if capture {
                searched_captures.push(mv);
            } else {
                searched_quiets.push(mv);
            }
        }

        if move_count == 0 {
            // No legal move: mate if in check, stalemate otherwise. An excluded move means
            // the singular search found nothing else, which is not a terminal position.
            return if !excluded.is_none() {
                alpha
            } else if in_check {
                mated_in(ply)
            } else {
                VALUE_DRAW
            };
        }

        if !best_move.is_none() && best_value >= beta {
            self.update_histories(best_move, depth, ply, &searched_quiets, &searched_captures);
        }

        if excluded.is_none() {
            let bound = if best_value >= beta {
                Bound::Lower
            } else if PV && !best_move.is_none() {
                Bound::Exact
            } else {
                Bound::Upper
            };
            let _ = original_alpha;
            tt.store(probe, best_move, value_to_tt(best_value, ply), static_eval, depth, bound, PV);
        }

        best_value
    }

    /// The quiescence search: play out the forcing moves so the evaluation is not called on
    /// a position where a piece is hanging.
    fn qsearch<const PV: bool>(
        &mut self,
        mut alpha: Value,
        beta: Value,
        ply: i32,
        tt: &TranspositionTable,
    ) -> Value {
        self.nodes += 1;
        if self.nodes % 1024 == 0 {
            self.shared.add_nodes(1024);
            self.check_limits();
        }
        if self.shared.stopped() {
            return VALUE_ZERO_SENTINEL;
        }
        if PV {
            self.sel_depth = self.sel_depth.max(ply + 1);
        }
        if self.pos.is_draw(ply) || eval::is_material_draw(&self.pos) {
            return VALUE_DRAW;
        }
        if ply >= MAX_PLY as i32 - 1 {
            return self.evaluate();
        }

        let in_check = self.pos.in_check();
        let probe = tt.probe(self.pos.key());
        let tt_move = if probe.hit { probe.data.mv } else { Move::NONE };
        if !PV
            && probe.hit
            && probe.data.depth >= 0
            && match probe.data.bound {
                Bound::Exact => true,
                Bound::Lower => {
                    value_from_tt(probe.data.value, ply, self.pos.rule50_count()) >= beta
                }
                Bound::Upper => {
                    value_from_tt(probe.data.value, ply, self.pos.rule50_count()) <= alpha
                }
                Bound::None => false,
            }
        {
            return value_from_tt(probe.data.value, ply, self.pos.rule50_count());
        }

        // Standing pat: the side to move is not obliged to capture, so the static
        // evaluation is a lower bound on what it can get. In check there is no such option.
        let mut best_value = if in_check {
            -VALUE_INFINITE
        } else {
            let v = self.evaluate();
            if v >= beta {
                return v;
            }
            alpha = alpha.max(v);
            v
        };

        let continuations = self.continuation_keys(ply);
        let mut picker = MovePicker::new_qsearch(&self.pos, continuations, tt_move);
        let mut best_move = Move::NONE;
        let mut move_count = 0;

        while let Some(mv) = picker.next(&self.pos, &self.histories, false) {
            if !self.pos.legal(mv) {
                continue;
            }
            move_count += 1;

            // Delta pruning: even winning the captured piece outright would not reach
            // alpha, so the capture cannot help.
            if !in_check && best_value > VALUE_MATED_IN_MAX_PLY {
                let gain = piece_value(self.pos.piece_on(mv.to()));
                if best_value + gain + 200 < alpha {
                    continue;
                }
                if !self.pos.see_ge(mv, 0) {
                    continue;
                }
            }

            let gives_check = self.pos.gives_check(mv);
            self.stack[STACK_BASE + ply as usize].current_move = mv;
            self.stack[STACK_BASE + ply as usize].moved_piece = self.pos.piece_on(mv.from());
            self.pos.do_move_checked(mv, gives_check);
            let score = -self.qsearch::<PV>(-beta, -alpha, ply + 1, tt);
            self.pos.undo_move(mv);

            if self.shared.stopped() {
                return VALUE_ZERO_SENTINEL;
            }
            if score > best_value {
                best_value = score;
                if score > alpha {
                    best_move = mv;
                    if score >= beta {
                        break;
                    }
                    alpha = score;
                }
            }
        }

        if in_check && move_count == 0 {
            return mated_in(ply);
        }

        let bound = if best_value >= beta { Bound::Lower } else { Bound::Upper };
        tt.store(probe, best_move, value_to_tt(best_value, ply), VALUE_NONE, 0, bound, PV);
        best_value
    }

    /// The four continuation planes a node reads: one, two, four and six plies back.
    ///
    /// Returned as plain keys rather than references, so the caller can still take `&mut
    /// self` while the picker is alive. See [`ContKey`].
    fn continuation_keys(&self, ply: i32) -> [Option<ContKey>; 4] {
        let mut out = [None; 4];
        for (slot, back) in [1usize, 2, 4, 6].into_iter().enumerate() {
            if ply < back as i32 {
                continue;
            }
            let e = &self.stack[STACK_BASE + ply as usize - back];
            if e.current_move.is_ok() && !e.moved_piece.is_none() {
                out[slot] = Some(ContKey {
                    in_check: e.in_check,
                    capture: false,
                    pc: e.moved_piece,
                    to: continuation_to(e.current_move),
                });
            }
        }
        out
    }

    /// Reward the move that caused a cutoff and punish the ones that did not.
    ///
    /// Punishing the failures is as important as rewarding the winner: without it every
    /// move that ever appeared in a cutting node drifts upward and the ordering degrades
    /// into the generation order.
    fn update_histories(
        &mut self,
        best: Move,
        depth: i32,
        ply: i32,
        quiets: &[Move],
        captures: &[Move],
    ) {
        let us = self.pos.side_to_move();
        let bonus = stat_bonus(depth);
        let malus = stat_malus(depth);
        let pawn_row = super::history::PawnHistory::row(self.pos.st().pawn_key);
        let si = STACK_BASE + ply as usize;

        if self.pos.is_capture_stage(best) {
            let pc = self.pos.piece_on(best.from());
            let victim = self.pos.piece_on(best.to()).piece_type();
            self.histories.captures.update(pc, best.to(), victim, bonus);
        } else {
            let pc = self.pos.piece_on(best.from());
            self.histories.main.update(us, best.from_to(), bonus);
            self.histories.pawn.update(pawn_row, pc, best.to(), bonus);
            self.update_continuations(ply, pc, best, bonus);

            // The killer slots hold two quiet moves that cut off at this ply in a sibling
            // node. Shifting rather than overwriting keeps the previous one available.
            if self.stack[si].killers[0] != best {
                self.stack[si].killers[1] = self.stack[si].killers[0];
                self.stack[si].killers[0] = best;
            }

            for &m in quiets {
                if m == best {
                    continue;
                }
                let pc = self.pos.piece_on(m.from());
                self.histories.main.update(us, m.from_to(), -malus);
                self.histories.pawn.update(pawn_row, pc, m.to(), -malus);
                self.update_continuations(ply, pc, m, -malus);
            }
        }

        for &m in captures {
            if m == best {
                continue;
            }
            let pc = self.pos.piece_on(m.from());
            let victim = self.pos.piece_on(m.to()).piece_type();
            self.histories.captures.update(pc, m.to(), victim, -malus);
        }
    }

    fn update_continuations(
        &mut self,
        ply: i32,
        pc: crate::board::types::Piece,
        m: Move,
        bonus: i32,
    ) {
        let to = continuation_to(m);
        for back in [1usize, 2, 4, 6] {
            if ply < back as i32 {
                break;
            }
            let e = &self.stack[STACK_BASE + ply as usize - back];
            if !e.current_move.is_ok() || e.moved_piece.is_none() {
                continue;
            }
            let (in_check, parent_pc, parent_to) =
                (e.in_check, e.moved_piece, continuation_to(e.current_move));
            self.histories
                .continuation
                .plane_mut(in_check, false, parent_pc, parent_to)
                .update(pc, to, bonus);
        }
    }

    /// Stop the search when a limit has been reached.
    ///
    /// Called every 1024 nodes rather than every node: reading a clock is a syscall on some
    /// platforms, and at a few million nodes per second the granularity is well under a
    /// millisecond either way.
    fn check_limits(&self) {
        if self.shared.stopped() {
            return;
        }
        if self.limits.infinite {
            return;
        }
        if let Some(n) = self.limits.nodes
            && self.shared.node_count() >= n
        {
            self.shared.request_stop();
            return;
        }
        if (self.limits.uses_time_management() || self.limits.move_time.is_some())
            && self.budget.out_of_time()
        {
            self.shared.request_stop();
        }
    }

    /// Emit one `info` line for the current PV slot.
    fn report(
        &self,
        depth: i32,
        score: Value,
        bound: Option<bool>,
        tt: &TranspositionTable,
        sink: &mut dyn InfoSink,
    ) {
        let rm = &self.root_moves[self.pv_index];
        let elapsed = self.budget.elapsed().as_millis().max(1) as u64;
        let nodes = self.shared.node_count().max(self.nodes);
        // Render the PV against a copy, because Chess960 castling notation depends on the
        // position each move is played in.
        let mut walk = self.pos.clone();
        let mut pv = Vec::with_capacity(rm.pv.len());
        for &m in &rm.pv {
            if !walk.pseudo_legal(m) || !walk.legal(m) {
                break;
            }
            pv.push(move_to_uci(&walk, m));
            walk.do_move(m);
        }

        let report = DepthReport {
            depth,
            sel_depth: self.sel_depth,
            multi_pv: self.pv_index + 1,
            score,
            bound,
            nodes,
            nps: nodes * 1000 / elapsed,
            hashfull: tt.hashfull(),
            tb_hits: 0,
            time_ms: elapsed,
            pv: &pv,
        };
        sink.depth_finished(&report);
    }
}

/// The score a node returns when the search was aborted underneath it.
///
/// Zero, so an aborted subtree cannot look like a win or a loss and poison the root move
/// it was under. The caller discards it in any case, because it checks the stop flag.
const VALUE_ZERO_SENTINEL: Value = 0;

/// The late-move reduction table.
///
/// Reductions grow with the log of both depth and move number: the hundredth move at depth
/// 20 is far less likely to matter than the third at depth 4, and a linear rule cannot
/// express that without being wrong at one end.
fn reduction(depth: i32, move_count: i32) -> i32 {
    static TABLE: std::sync::LazyLock<Vec<Vec<i32>>> = std::sync::LazyLock::new(|| {
        (0..64)
            .map(|d| {
                (0..64)
                    .map(|m| {
                        if d == 0 || m == 0 {
                            0
                        } else {
                            let r = (f64::from(d)).ln() * (f64::from(m)).ln() / 2.1;
                            r as i32
                        }
                    })
                    .collect()
            })
            .collect()
    });
    TABLE[depth.clamp(0, 63) as usize][move_count.clamp(0, 63) as usize]
}

/// Mate score to `mate in N` in full moves, as the UCI `score mate` field wants it.
#[must_use]
pub fn score_to_uci(v: Value) -> String {
    if v >= VALUE_MATE_IN_MAX_PLY {
        format!("mate {}", (VALUE_MATE - v + 1) / 2)
    } else if v <= VALUE_MATED_IN_MAX_PLY {
        format!("mate {}", (-VALUE_MATE - v) / 2)
    } else {
        // Upstream normalises the centipawn scale so that "100" means "about one pawn" for
        // the current network. The classical scaffolding is already in pawn units, so the
        // scale factor is 1 until NNUE lands.
        format!("cp {v}")
    }
}

/// The colour whose clock a search reads.
#[must_use]
pub fn searching_side(pos: &Position) -> Color {
    pos.side_to_move()
}

#[cfg(test)]
mod tests {
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
        w.search(&pos, &limits, &tt, 1, 10, &mut SilentSink)
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
        assert_eq!(score_to_uci(r.score), "mate 1");
    }

    #[test]
    fn a_forced_mate_several_moves_deep_is_found() {
        // Rf6-a6! and mate follows in three. The search must see past two intermediate
        // replies, which a shallow tactical trick cannot fake.
        let r = search_fen("r5rk/5p1p/5R2/4B3/8/8/7P/7K w - - 0 1", 12);
        assert!(r.score >= VALUE_MATE - 6, "score {} is not a forced mate", r.score);
        assert_eq!(format!("{:?}", r.best_move), "f6a6");
    }

    #[test]
    fn a_hanging_queen_is_taken() {
        let r = search_fen("4k3/8/8/3q4/4P3/8/8/4K3 w - - 0 1", 6);
        assert_eq!(format!("{:?}", r.best_move), "e4d5");
    }

    #[test]
    fn a_stalemated_side_reports_no_move_rather_than_panicking() {
        let pos = Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1", false).expect("valid");
        let shared = SharedState::new();
        let tt = TranspositionTable::new(1);
        let mut w = SearchWorker::new(0, shared);
        let limits = Limits { depth: Some(4), start: Some(Instant::now()), ..Limits::default() };
        let r = w.search(&pos, &limits, &tt, 1, 10, &mut SilentSink);
        assert!(r.best_move.is_none());
        assert_eq!(r.score, VALUE_DRAW);
    }

    #[test]
    fn a_node_limit_is_honoured() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let shared = SharedState::new();
        shared.reset();
        let tt = TranspositionTable::new(8);
        let mut w = SearchWorker::new(0, Arc::clone(&shared));
        let limits =
            Limits { nodes: Some(20_000), start: Some(Instant::now()), ..Limits::default() };
        let r = w.search(&pos, &limits, &tt, 1, 10, &mut SilentSink);
        assert!(r.best_move.is_ok());
        // The check runs every 1024 nodes, so allow that much slack over the limit.
        assert!(shared.node_count() < 20_000 + 4096 + 1024 * 40, "{}", shared.node_count());
    }

    #[test]
    fn searchmoves_restricts_the_root() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let only = generate_legal(&pos).as_slice()[3];
        let shared = SharedState::new();
        let tt = TranspositionTable::new(1);
        let mut w = SearchWorker::new(0, shared);
        let limits = Limits {
            depth: Some(4),
            search_moves: vec![only],
            start: Some(Instant::now()),
            ..Limits::default()
        };
        let r = w.search(&pos, &limits, &tt, 1, 10, &mut SilentSink);
        assert_eq!(r.best_move, only);
    }

    #[test]
    fn deeper_searches_never_return_an_illegal_move() {
        for fen in [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        ] {
            let pos = Position::from_fen(fen, false).expect("valid");
            let r = search_fen(fen, 7);
            assert!(generate_legal(&pos).contains(&r.best_move), "{fen}");
        }
    }

    #[test]
    fn the_reported_pv_is_playable_from_the_root() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let shared = SharedState::new();
        let tt = TranspositionTable::new(16);
        let mut w = SearchWorker::new(0, Arc::clone(&shared));
        let limits = Limits { depth: Some(8), start: Some(Instant::now()), ..Limits::default() };
        w.search(&pos, &limits, &tt, 1, 10, &mut SilentSink);

        let mut walk = pos.clone();
        for &m in &w.root_moves[0].pv {
            assert!(generate_legal(&walk).contains(&m), "PV move {m:?} is not legal");
            walk.do_move(m);
        }
    }

    #[test]
    fn multipv_reports_distinct_root_moves() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let shared = SharedState::new();
        let tt = TranspositionTable::new(8);
        let mut w = SearchWorker::new(0, Arc::clone(&shared));
        let limits = Limits { depth: Some(5), start: Some(Instant::now()), ..Limits::default() };
        w.search(&pos, &limits, &tt, 3, 10, &mut SilentSink);
        let top: Vec<Move> = w.root_moves[..3].iter().map(|rm| rm.mv).collect();
        let mut sorted = top.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn score_rendering_matches_the_uci_conventions() {
        assert_eq!(score_to_uci(0), "cp 0");
        assert_eq!(score_to_uci(123), "cp 123");
        assert_eq!(score_to_uci(VALUE_MATE - 1), "mate 1");
        assert_eq!(score_to_uci(VALUE_MATE - 3), "mate 2");
        assert_eq!(score_to_uci(-VALUE_MATE + 2), "mate -1");
    }
}
