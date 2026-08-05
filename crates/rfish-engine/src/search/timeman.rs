//! The time manager: how long one move gets.
//!
//! Two numbers come out of it. The *optimum* is the point past which a new iteration
//! should not be STARTED — stopping between iterations is free, because the last completed
//! one already has a best move. The *maximum* is the point past which the search stops
//! wherever it is, and losing on time is worse than any move it could still find.
//!
//! The manager outlives one move. `available_nodes` is a whole-game budget under
//! `nodestime`, and `original_time_adjust` is fixed on the first move of the game and
//! reused, so both live here and are cleared by [`TimeManagement::clear`] on `ucinewgame`
//! rather than being recomputed per `go`.
//!
//! Golden: `Stockfish/src/timeman.cpp`.

use std::time::Instant;

use crate::board::types::{Color, GamePly};
use crate::state::{Budget, Elapsed, Limits, SearchOptions, TimeBudget};

/// Milliseconds reserved for the move to reach the GUI.
///
/// Upstream calls this `Move Overhead` and exposes it as an option; the default is the
/// same here. A network GUI needs more, and a value that is too small loses games on time
/// through no fault of the search.
pub const DEFAULT_MOVE_OVERHEAD: u64 = 10;

/// The clock model for one game.
#[derive(Clone, Copy, Debug)]
pub struct TimeManagement {
    start: Instant,
    optimum: i64,
    maximum: i64,
    use_nodes_time: bool,
    /// Nodes left in the game under `nodestime`; negative means "not yet initialised".
    available_nodes: i64,
    /// The first move's time-left factor, held for the rest of the game; negative means
    /// "not yet computed".
    original_time_adjust: f64,
}

impl Default for TimeManagement {
    fn default() -> TimeManagement {
        TimeManagement {
            start: Instant::now(),
            optimum: 0,
            maximum: 0,
            use_nodes_time: false,
            available_nodes: -1,
            original_time_adjust: -1.0,
        }
    }
}

impl TimeManagement {
    /// Forget the whole-game state. Called on `ucinewgame`.
    pub fn clear(&mut self) {
        self.available_nodes = -1;
        self.original_time_adjust = -1.0;
    }

    /// Spend `nodes` of the game-long node budget.
    pub fn advance_nodes_time(&mut self, nodes: i64) {
        debug_assert!(self.use_nodes_time);
        self.available_nodes = (self.available_nodes - nodes).max(0);
    }

    /// The budget the workers enforce.
    #[must_use]
    pub fn budget(&self) -> TimeBudget {
        let (optimum, maximum) = (Elapsed::new(self.optimum), Elapsed::new(self.maximum));
        // The one place the flag becomes the bounds' container. Everything downstream reads
        // the pair through the arm, so a bound cannot be had without its unit.
        let budget = if self.use_nodes_time {
            Budget::Nodes { optimum, maximum }
        } else {
            Budget::Wall { optimum, maximum }
        };
        TimeBudget::new(self.start, budget)
    }

    /// True when the clock is being counted in nodes rather than milliseconds.
    #[must_use]
    pub fn uses_nodes_time(&self) -> bool {
        self.use_nodes_time
    }

    /// Work out the bounds for the current move.
    ///
    /// `limits` is taken by `&mut` because nodes-as-time REWRITES it: the clock, the
    /// increment and the overhead are all converted into node counts, and everything
    /// downstream then compares node counts against node counts. That conversion is
    /// upstream's, and doing it anywhere else would leave two units in play at once.
    pub fn init(&mut self, limits: &mut Limits, us: Color, ply: GamePly, opts: &SearchOptions) {
        let npmsec = opts.nodestime as i64;

        // With no clock there is nothing to divide up. `start` and `use_nodes_time` still
        // have to be right: `movetime` and the node limit are measured against them.
        self.start = limits.start.unwrap_or_else(Instant::now);
        self.use_nodes_time = npmsec != 0;

        let side = us.index();
        // ZERO is the only clock that skips the rest, exactly as upstream's
        // `if (limits.time[us] == 0) return;`. A NEGATIVE clock is a real state -- a GUI whose
        // engine has overstepped sends one -- and upstream budgets from it, which produces a
        // tiny allowance and a move played almost at once. Treating it as "no clock" instead
        // took the unmanaged path and searched on: `go wtime -100 btime 100` went a full ply
        // deeper here than upstream, every time.
        //
        // The value is signed on the way in for the same reason: `TimePoint` is `int64`, and
        // the sign is the whole content of this case.
        let Some(mut time) = limits.time[side].map(|t| t as i64).filter(|&t| t != 0) else {
            return;
        };

        let mut move_overhead = opts.move_overhead as i64;
        let mut inc = limits.inc[side] as i64;

        // WARNING: to avoid losing on time, the `nodestime` value must be well BELOW the
        // engine's real speed. It is a budget the search is held to, not a measurement.
        if self.use_nodes_time {
            if self.available_nodes < 0 {
                // Once, at the start of the game.
                self.available_nodes = npmsec * time;
            }
            time = self.available_nodes;
            inc *= npmsec;
            move_overhead *= npmsec;
            limits.time[side] = Some(time as u64);
            limits.inc[side] = inc as u64;
            limits.npmsec = npmsec as u64;
        }

        // Every constant below is calibrated against milliseconds, so scale back into them
        // before comparing and scale the result out again.
        let scale_factor = if self.use_nodes_time { npmsec } else { 1 };
        let scaled_time = time / scale_factor;

        let movestogo = limits.moves_to_go.unwrap_or(0) as i32;
        let mut mtg = if movestogo != 0 { movestogo.min(50) } else { 50 };

        // Under a second, taper the horizon: planning fifty more moves out of 800 ms
        // allocates a budget too small to complete even one iteration.
        if scaled_time < 1000 {
            mtg = (scaled_time as f64 * 0.05) as i32;
        }

        // Used as a divisor below, so it must stay positive.
        let time_left =
            (time + inc * i64::from(mtg - 1) - move_overhead * i64::from(2 + mtg)).max(1);

        let (opt_scale, max_scale);
        if movestogo == 0 {
            // x basetime (+ z increment).
            if self.original_time_adjust < 0.0 {
                self.original_time_adjust = 0.3272 * (time_left as f64).log10() - 0.4141;
            }

            let log_time_in_sec = (scaled_time as f64 / 1000.0).log10();
            let opt_constant = (0.002_986_9 + 0.000_335_54 * log_time_in_sec).min(0.004_905);
            let max_constant = (3.3744 + 3.0608 * log_time_in_sec).max(3.1441);

            // A healthy increment can push `time_left` past the time actually available for
            // this move, so cap against the clock as well as against the formula.
            opt_scale = (0.012_112 + (f64::from(ply.get()) + 3.22713).powf(0.46866) * opt_constant)
                .min(0.19404 * time as f64 / time_left as f64)
                * self.original_time_adjust;
            max_scale = 6.873f64.min(max_constant + f64::from(ply.get()) / 12.352);
        } else {
            // x moves in y seconds (+ z increment).
            opt_scale = ((0.88 + f64::from(ply.get()) / 116.4) / f64::from(mtg))
                .min(0.88 * time as f64 / time_left as f64);
            max_scale = 1.3 + 0.11 * f64::from(mtg);
        }

        self.optimum = (opt_scale * time_left as f64).max(1.0) as i64;
        self.maximum = (self.optimum as f64)
            .max((0.8097 * time as f64 - move_overhead as f64).min(max_scale * self.optimum as f64))
            as i64;

        // A ponder hit inherits the thinking already done, so the move can afford to plan
        // for longer than it would if every millisecond had to come out of its own clock.
        if opts.ponder {
            self.optimum += self.optimum / 4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits_with_clock(ms: u64, inc: u64, mtg: Option<u32>) -> Limits {
        Limits {
            time: [Some(ms), Some(ms)],
            inc: [inc, inc],
            moves_to_go: mtg,
            start: Some(Instant::now()),
            ..Limits::default()
        }
    }

    fn budget(ms: u64, inc: u64, mtg: Option<u32>, ply: GamePly) -> TimeBudget {
        let mut l = limits_with_clock(ms, inc, mtg);
        let mut tm = TimeManagement::default();
        tm.init(&mut l, Color::White, ply, &SearchOptions::default());
        tm.budget()
    }

    #[test]
    fn no_clock_leaves_the_bounds_at_zero() {
        let mut l = Limits { depth: Some(10), start: Some(Instant::now()), ..Limits::default() };
        let mut tm = TimeManagement::default();
        tm.init(&mut l, Color::White, GamePly::new(0), &SearchOptions::default());
        // `go depth` is not bounded by the clock, and the caller checks
        // `uses_time_management` before reading these at all.
        assert!(!l.uses_time_management());
        assert_eq!(tm.budget().optimum().get(), 0);
    }

    #[test]
    fn the_budget_never_exceeds_the_clock() {
        for ms in [10u64, 100, 1000, 60_000, 3_600_000] {
            for inc in [0u64, 100, 1000] {
                let b = budget(ms, inc, None, GamePly::new(20));
                assert!(b.maximum().get() <= ms as i64, "budget {b:?} exceeds a {ms} ms clock");
                assert!(b.optimum().get() <= b.maximum().get());
            }
        }
    }

    #[test]
    fn a_short_clock_still_yields_a_positive_budget() {
        // Under the overhead the budget must not become zero, or the engine forfeits by
        // making no move at all.
        assert!(budget(5, 0, None, GamePly::new(20)).maximum().get() >= 1);
    }

    #[test]
    fn a_known_move_quota_spends_a_larger_share_than_sudden_death() {
        assert!(
            budget(60_000, 0, Some(5), GamePly::new(20)).optimum().get()
                > budget(60_000, 0, None, GamePly::new(20)).optimum().get()
        );
    }

    #[test]
    fn an_increment_raises_the_per_move_budget() {
        assert!(
            budget(60_000, 1000, None, GamePly::new(20)).optimum().get()
                > budget(60_000, 0, None, GamePly::new(20)).optimum().get()
        );
    }

    #[test]
    fn pondering_raises_the_optimum_by_a_quarter() {
        let mut l = limits_with_clock(60_000, 0, None);
        let mut tm = TimeManagement::default();
        let plain = SearchOptions::default();
        tm.init(&mut l, Color::White, GamePly::new(20), &plain);
        let without = tm.budget().optimum().get();

        let mut l = limits_with_clock(60_000, 0, None);
        let mut tm = TimeManagement::default();
        tm.init(&mut l, Color::White, GamePly::new(20), &SearchOptions { ponder: true, ..plain });
        assert_eq!(tm.budget().optimum().get(), without + without / 4);
    }

    #[test]
    fn nodestime_converts_the_whole_clock_into_nodes() {
        let mut l = limits_with_clock(60_000, 100, None);
        let mut tm = TimeManagement::default();
        let opts = SearchOptions { nodestime: 600, ..SearchOptions::default() };
        tm.init(&mut l, Color::White, GamePly::new(20), &opts);

        assert!(tm.uses_nodes_time());
        assert!(tm.budget().counts_nodes());
        // The clock became a node budget at the declared rate, and the increment with it.
        assert_eq!(l.time[Color::White.index()], Some(600 * 60_000));
        assert_eq!(l.inc[Color::White.index()], 600 * 100);
        assert_eq!(l.npmsec, 600);
    }

    #[test]
    fn the_node_budget_is_spent_across_moves() {
        let mut l = limits_with_clock(1000, 0, None);
        let mut tm = TimeManagement::default();
        let opts = SearchOptions { nodestime: 600, ..SearchOptions::default() };
        tm.init(&mut l, Color::White, GamePly::new(0), &opts);
        let first = tm.budget().optimum().get();

        tm.advance_nodes_time(500_000);
        let mut l = limits_with_clock(1000, 0, None);
        tm.init(&mut l, Color::White, GamePly::new(2), &opts);
        // Half the game's nodes are gone, so the next move plans for less.
        assert!(tm.budget().optimum().get() < first, "spending nodes must shrink the next budget");
    }

    #[test]
    fn the_time_adjust_factor_is_fixed_on_the_first_move() {
        let mut tm = TimeManagement::default();
        let opts = SearchOptions::default();
        let mut l = limits_with_clock(600_000, 0, None);
        tm.init(&mut l, Color::White, GamePly::new(0), &opts);
        let fixed = tm.original_time_adjust;

        // A later move with far less clock keeps the factor the game opened with.
        let mut l = limits_with_clock(1_000, 0, None);
        tm.init(&mut l, Color::White, GamePly::new(60), &opts);
        assert!((tm.original_time_adjust - fixed).abs() < f64::EPSILON);

        tm.clear();
        let mut l = limits_with_clock(1_000, 0, None);
        tm.init(&mut l, Color::White, GamePly::new(0), &opts);
        assert!(
            (tm.original_time_adjust - fixed).abs() > f64::EPSILON,
            "ucinewgame must re-derive the factor"
        );
    }
}
