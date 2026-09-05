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
            optimum: TimeManagement::NO_BOUND,
            maximum: TimeManagement::NO_BOUND,
            use_nodes_time: false,
            available_nodes: -1,
            original_time_adjust: -1.0,
        }
    }
}

impl TimeManagement {
    /// What a move with no clock of its own is held to.
    ///
    /// Not zero, and not "unset". [`TimeManagement::budget`] hands both bounds to the
    /// workers unconditionally — `uses_time_management` is true whenever EITHER side has a
    /// clock — so every path out of [`TimeManagement::init`] has to leave a number the
    /// readers can act on, and zero is the number that means "stop on the first check".
    ///
    /// Half of `i64::MAX`, as upstream's `NoBound` is, so that a reader adding to it cannot
    /// overflow.
    const NO_BOUND: i64 = i64::MAX / 2;

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
    /// increment, the overhead and `movetime` are all converted into node counts, and
    /// everything downstream then compares node counts against node counts. That conversion
    /// is upstream's, and doing it anywhere else would leave two units in play at once.
    pub fn init(&mut self, limits: &mut Limits, us: Color, ply: GamePly, opts: &SearchOptions) {
        let npmsec = opts.nodestime as i64;

        // With no clock there is nothing to divide up. `start` and `use_nodes_time` still
        // have to be right: `movetime` and the node limit are measured against them.
        self.start = limits.start.unwrap_or_else(Instant::now);
        self.use_nodes_time = npmsec != 0;

        // `movetime` joins the clock, the increment and the overhead in the budget's unit.
        // ABOVE the no-clock return, because `go movetime N` needs no clock at all: the path
        // that skips the rest of this function is the one where `movetime` is the only bound
        // there is.
        //
        // Saturating, and it cannot saturate on anything the shell accepts: the parse clamps
        // to `Limits::MAX_CLOCK_MS` and `nodestime`'s own maximum is four digits, so the
        // product stays well inside the `i64` that `check_time` casts it back to. Nothing in
        // the TYPE says so -- `Limits` is public and an embedder writes it directly -- and a
        // `u64` that wrapped here would cast to a NEGATIVE bound, which stops the search on
        // its first clock check.
        if self.use_nodes_time {
            limits.move_time = limits.move_time.map(|mt| mt.saturating_mul(opts.nodestime));
        }

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
            // WRITTEN, not left. This path is reached with a clock for the OTHER side --
            // `go depth 3 btime 1000` while White is to move -- and `uses_time_management`
            // is true for it, so the workers read both bounds on every clock check. A bound
            // left unwritten holds the previous move's budget, or the default, and any value
            // below the elapsed count stops the search on its first check.
            self.optimum = TimeManagement::NO_BOUND;
            self.maximum = TimeManagement::NO_BOUND;
            return;
        };

        let mut move_overhead = opts.move_overhead as i64;
        let mut inc = limits.inc[side] as i64;

        // WARNING: to avoid losing on time, the `nodestime` value must be well BELOW the
        // engine's real speed. It is a budget the search is held to, not a measurement.
        if self.use_nodes_time {
            if self.available_nodes < 0 {
                // Once, at the start of the game.
                self.available_nodes = npmsec.saturating_mul(time);
            }
            time = self.available_nodes;
            inc = inc.saturating_mul(npmsec);
            move_overhead = move_overhead.saturating_mul(npmsec);
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
        //
        // Only where the horizon is this engine's own guess, though. A cyclic control names
        // its remaining moves, so keep the count it gave rather than tapering to a horizon
        // shorter than the moves that must actually be played out of this clock -- tapering
        // there is what loses the game on time, at 40/1 by 49 Elo and 557 timeouts to none.
        if scaled_time < 1000 && movestogo == 0 {
            mtg = (scaled_time as f64 * 0.05) as i32;
        }

        // Used as a divisor below, so it must stay positive.
        //
        // SATURATING, and that is a bound rather than a style. `wtime` and `winc` are parsed
        // as a full `i64` of milliseconds — upstream's `TimePoint`, with no range on it — so
        // `go wtime 4e18 winc 4e18` reaches `inc * 49` here, which is twenty times what the
        // type holds. Upstream's C++ is undefined there; a release build of this port would
        // WRAP and budget from a negative horizon, and the gate profile's overflow checks
        // turn it into a panic reachable from one `go` line. Saturating is identical for
        // every value in range, so no gated number moves, and the shell clamps the clock at
        // its own boundary as well — see `Limits::MAX_CLOCK_MS`.
        //
        // The two `mtg` terms are formed AT `i64`, not converted after. `mtg` is an `i32`
        // and both edges of that type are reachable from one `go` line: a `movestogo` off
        // the wire arrives unbounded — upstream accepts a negative one and searches — and
        // the sub-second taper above casts `scaled_time * 0.05`, which a negative clock
        // inside `MAX_CLOCK_MS` drives past `i32::MIN`, where Rust's float cast saturates.
        // `mtg - 1` then panicked under the gate profile on both. Widening the subtraction
        // costs nothing — the conversion happens either way — and is EXACTLY equal for
        // every `i32`, so upstream's behaviour on both inputs is kept rather than corrected.
        let time_left = time
            .saturating_add(inc.saturating_mul(i64::from(mtg) - 1))
            .saturating_sub(move_overhead.saturating_mul(2 + i64::from(mtg)))
            .max(1);

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

    /// A clock for the OTHER side still has to leave both bounds readable.
    ///
    /// `go depth 3 btime 1000` with White to move takes init's no-clock path, and
    /// `uses_time_management` is true for it — a clock on either side is the whole test — so
    /// `check_time` reads both bounds on every call. A bound this path does not write is the
    /// previous move's, and one below the elapsed count stops the search at once.
    ///
    /// A pure function, as every reproducer in this class must be.
    #[test]
    fn a_clock_for_the_other_side_leaves_both_bounds_unbounded() {
        let mut limits = Limits {
            time: [None, Some(1000)],
            depth: Some(3),
            start: Some(Instant::now()),
            ..Limits::default()
        };
        assert!(limits.uses_time_management(), "the readers below are live for this go line");

        let mut tm = TimeManagement::default();
        tm.init(&mut limits, Color::White, GamePly::new(1), &SearchOptions::default());
        let b = tm.budget();
        assert_eq!(b.optimum(), Elapsed::new(TimeManagement::NO_BOUND));
        assert_eq!(b.maximum(), Elapsed::new(TimeManagement::NO_BOUND));

        // And a bound left over from a move that DID have a clock is not inherited.
        let mut clocked = limits_with_clock(60_000, 0, None);
        tm.init(&mut clocked, Color::White, GamePly::new(20), &SearchOptions::default());
        assert!(tm.budget().maximum() < Elapsed::new(TimeManagement::NO_BOUND));
        tm.init(&mut limits, Color::White, GamePly::new(21), &SearchOptions::default());
        assert_eq!(tm.budget().maximum(), Elapsed::new(TimeManagement::NO_BOUND));
    }

    /// A cyclic control keeps the horizon it named, where a sudden-death clock tapers.
    ///
    /// Under a second the taper overwrote `mtg` with a count derived from the CLOCK, so the
    /// horizon a cyclic control stated was discarded and every `movestogo` budgeted the
    /// same. That is the defect in one line: with 400 ms left, five moves to play and forty
    /// moves to play were planned identically, and the forty-move case times out.
    ///
    /// A pure function, as every reproducer in this class must be.
    #[test]
    fn a_sub_second_cyclic_control_keeps_its_stated_horizon() {
        // Sub-second, so the taper is live, and far enough under that it undercuts both
        // horizons: 400 ms tapers to 20, between the five and the forty.
        let few = budget(400, 0, Some(5), GamePly::new(20));
        let many = budget(400, 0, Some(40), GamePly::new(20));
        assert_ne!(few.optimum(), many.optimum(), "the stated horizon must reach the budget");
        assert!(
            many.optimum() < few.optimum(),
            "forty moves to pay for must budget less per move than five: {many:?} {few:?}"
        );

        // The taper still applies where no horizon was stated -- this narrows the branch
        // rather than removing it.
        let tapered = budget(400, 0, None, GamePly::new(20));
        let ample = budget(60_000, 0, None, GamePly::new(20));
        assert!(tapered.optimum() < ample.optimum(), "{tapered:?} {ample:?}");
    }

    /// A negative clock, and an extreme `movestogo`, reach the same subtraction.
    ///
    /// Pure functions both: the reproducer is one `go` line, and driving the binary with it
    /// is the class of test that has taken this box down. Red before the widening — the
    /// gate profile panicked at `mtg - 1`, where a release build wrapped and budgeted from
    /// a horizon that means nothing.
    #[test]
    fn an_extreme_horizon_does_not_panic_the_time_manager() {
        // `movestogo` is parsed as an `i32` and upstream accepts every value of one.
        for mtg in [i32::MIN, -5, 0, 1, 50, i32::MAX] {
            let b = budget(60_000, 0, Some(mtg as u32), GamePly::new(20));
            assert!(b.optimum().get() >= 1, "mtg {mtg} budgeted {b:?}");
        }

        // A negative clock is a state `clamp_clock` deliberately keeps, and the sub-second
        // taper turns a large one into an `mtg` at the bottom of the type.
        for ms in [-100_000_000_000i64, -1_000, -1] {
            let mut l = Limits {
                time: [Some(ms as u64), Some(ms as u64)],
                start: Some(Instant::now()),
                ..Limits::default()
            };
            let mut tm = TimeManagement::default();
            tm.init(&mut l, Color::White, GamePly::new(20), &SearchOptions::default());
            assert!(tm.budget().optimum().get() >= 1, "clock {ms} budgeted nothing");
        }
    }

    #[test]
    fn no_clock_leaves_the_bounds_unbounded() {
        let mut l = Limits { depth: Some(10), start: Some(Instant::now()), ..Limits::default() };
        let mut tm = TimeManagement::default();
        tm.init(&mut l, Color::White, GamePly::new(0), &SearchOptions::default());
        // `go depth` is not bounded by the clock, so no reader consults these -- and they
        // still say "no bound" rather than "stop now", because the one thing a reader must
        // never find here is a budget that has already run out.
        assert!(!l.uses_time_management());
        assert_eq!(tm.budget().optimum().get(), TimeManagement::NO_BOUND);
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

    /// A clock a GUI can send and the arithmetic cannot hold.
    ///
    /// `wtime` and `winc` are parsed as a full unbounded `i64` of milliseconds, exactly as
    /// upstream parses its `TimePoint` — so `go wtime 4e18 winc 4e18` reaches `time + inc *
    /// (mtg - 1)` below with `mtg` at 50, and `4e18 * 49` is twenty times what an `i64`
    /// holds. In release that WRAPS and the budget is computed from a negative `time_left`;
    /// under the gate profile's `overflow-checks` it is a panic, which is what this asserts
    /// against. Driven as a pure function rather than through the engine, because a
    /// reproducer that reaches option parsing is how this box has been taken down twice.
    #[test]
    fn an_enormous_clock_does_not_overflow_the_horizon() {
        let huge = 4_000_000_000_000_000_000u64;
        for inc in [0u64, 1000, huge] {
            for mtg in [None, Some(1u32), Some(50)] {
                let b = budget(huge, inc, mtg, GamePly::new(20));
                assert!(
                    b.optimum().get() >= 1 && b.maximum().get() >= b.optimum().get(),
                    "clock {huge} inc {inc} mtg {mtg:?} produced {b:?}"
                );
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

    /// `movetime` is measured against the same clock as everything else, so it converts too.
    ///
    /// `check_time` compares `movetime` against an elapsed count that IS nodes under
    /// `nodestime`, so an unconverted millisecond figure would make `go movetime 1000` stop
    /// after a thousand nodes.
    #[test]
    fn nodestime_converts_movetime_as_well() {
        let opts = SearchOptions { nodestime: 600, ..SearchOptions::default() };

        let mut l = limits_with_clock(60_000, 100, None);
        l.move_time = Some(1000);
        TimeManagement::default().init(&mut l, Color::White, GamePly::new(20), &opts);
        assert_eq!(l.move_time, Some(600 * 1000));

        // And on the path that takes the no-clock return, where `movetime` is the only
        // bound the search has.
        let mut bare =
            Limits { move_time: Some(1000), start: Some(Instant::now()), ..Limits::default() };
        TimeManagement::default().init(&mut bare, Color::White, GamePly::new(20), &opts);
        assert_eq!(bare.move_time, Some(600 * 1000));

        // Without `nodestime` it stays milliseconds.
        let mut plain =
            Limits { move_time: Some(1000), start: Some(Instant::now()), ..Limits::default() };
        TimeManagement::default().init(
            &mut plain,
            Color::White,
            GamePly::new(20),
            &SearchOptions::default(),
        );
        assert_eq!(plain.move_time, Some(1000));
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
