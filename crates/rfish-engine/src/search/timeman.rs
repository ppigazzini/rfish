//! The time manager: how long one move gets.
//!
//! Two numbers come out of it. The *optimum* is the point past which a new iteration
//! should not be STARTED — stopping between iterations is free, because the last completed
//! one already has a best move. The *maximum* is the point past which the search stops
//! wherever it is, and losing on time is worse than any move it could still find.
//!
//! Golden: `Stockfish/src/timeman.cpp`.

use std::time::{Duration, Instant};

use crate::board::types::Color;
use crate::state::{Limits, TimeBudget};

/// Milliseconds reserved for the move to reach the GUI.
///
/// Upstream calls this `Move Overhead` and exposes it as an option; the default is the
/// same here. A network GUI needs more, and a value that is too small loses games on time
/// through no fault of the search.
pub const DEFAULT_MOVE_OVERHEAD: u64 = 10;

/// Work out the budget for one move.
///
/// `moves_to_go` of `None` means sudden death, which is the case the whole formula is
/// shaped around: the clock has to last an unknown number of moves, so each move gets a
/// fraction of what remains rather than a share of a known quota.
#[must_use]
pub fn allocate(limits: &Limits, us: Color, move_overhead: u64) -> TimeBudget {
    let start = limits.start.unwrap_or_else(Instant::now);

    // A fixed time per move needs no reasoning: spend it, minus the overhead.
    if let Some(mt) = limits.move_time {
        let d = Duration::from_millis(mt.saturating_sub(move_overhead));
        return TimeBudget { start, optimum: d, maximum: d };
    }

    let Some(remaining) = limits.time[us.index()] else {
        // No clock: `go infinite`, `go depth`, `go nodes`. Nothing bounds the time, so
        // hand back a budget that never expires and let the other limit do the work.
        let forever = Duration::from_secs(86_400);
        return TimeBudget { start, optimum: forever, maximum: forever };
    };

    let inc = limits.inc[us.index()];
    // Never plan to use time that will not exist by the time the move is sent.
    let remaining = remaining.saturating_sub(move_overhead).max(1);

    // How many moves the budget has to cover. With a known quota, use it; without one,
    // assume the game runs about 50 more plies, tapering as the game goes on so the
    // opening does not spend the endgame's time.
    let mtg = match limits.moves_to_go {
        Some(n) => u64::from(n.max(1)).min(50),
        None => 50u64.saturating_sub((limits.ply as u64 / 2).min(30)).max(20),
    };

    // The optimum is the even share plus most of the increment: the increment is money
    // that arrives whether it is spent or not, so holding it back only wastes it.
    let optimum_ms = (remaining / mtg + inc * 3 / 4).min(remaining);

    // The maximum lets a single critical move borrow from the rest of the game, capped so
    // that a bad estimate cannot spend the whole clock at once.
    let maximum_ms = (optimum_ms * 4).min(remaining * 4 / 5).max(optimum_ms);

    TimeBudget {
        start,
        optimum: Duration::from_millis(optimum_ms),
        maximum: Duration::from_millis(maximum_ms.max(1)),
    }
}

/// Should the search start another iteration?
///
/// `instability` scales the optimum: a root move that keeps changing, or a score that
/// keeps falling, is a sign the current answer is not to be trusted, and upstream buys more
/// time rather than reporting it.
#[must_use]
pub fn should_start_iteration(budget: &TimeBudget, instability: f64) -> bool {
    let scaled = budget.optimum.mul_f64(instability.clamp(0.5, 2.5));
    budget.elapsed() < scaled.min(budget.maximum)
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

    #[test]
    fn a_fixed_move_time_is_spent_minus_the_overhead() {
        let l = Limits { move_time: Some(1000), start: Some(Instant::now()), ..Limits::default() };
        let b = allocate(&l, Color::White, 10);
        assert_eq!(b.optimum, Duration::from_millis(990));
        assert_eq!(b.maximum, b.optimum);
    }

    #[test]
    fn no_clock_means_no_time_limit() {
        let l = Limits { depth: Some(10), start: Some(Instant::now()), ..Limits::default() };
        let b = allocate(&l, Color::White, 10);
        assert!(b.optimum > Duration::from_secs(3600));
        assert!(!b.out_of_time());
    }

    #[test]
    fn the_budget_never_exceeds_the_clock() {
        for ms in [10u64, 100, 1000, 60_000, 3_600_000] {
            for inc in [0u64, 100, 1000] {
                let b = allocate(&limits_with_clock(ms, inc, None), Color::White, 10);
                assert!(
                    b.maximum <= Duration::from_millis(ms),
                    "budget {b:?} exceeds a {ms} ms clock"
                );
                assert!(b.optimum <= b.maximum);
            }
        }
    }

    #[test]
    fn a_short_clock_still_yields_a_positive_budget() {
        // Under the overhead, the budget must not become zero or the engine forfeits by
        // making no move at all.
        let b = allocate(&limits_with_clock(5, 0, None), Color::White, 10);
        assert!(b.maximum >= Duration::from_millis(1));
    }

    #[test]
    fn a_known_move_quota_spends_a_larger_share_than_sudden_death() {
        let quota = allocate(&limits_with_clock(60_000, 0, Some(5)), Color::White, 10);
        let sudden = allocate(&limits_with_clock(60_000, 0, None), Color::White, 10);
        assert!(quota.optimum > sudden.optimum);
    }

    #[test]
    fn an_increment_raises_the_per_move_budget() {
        let with = allocate(&limits_with_clock(60_000, 1000, None), Color::White, 10);
        let without = allocate(&limits_with_clock(60_000, 0, None), Color::White, 10);
        assert!(with.optimum > without.optimum);
    }

    #[test]
    fn instability_buys_time_but_never_past_the_maximum() {
        let b = allocate(&limits_with_clock(60_000, 0, None), Color::White, 10);
        assert!(should_start_iteration(&b, 1.0));
        // Even an extreme instability factor is clamped by the hard limit.
        let hard = TimeBudget {
            start: Instant::now().checked_sub(Duration::from_millis(10)).unwrap(),
            optimum: Duration::from_millis(1),
            maximum: Duration::from_millis(2),
        };
        assert!(!should_start_iteration(&hard, 100.0));
    }
}
