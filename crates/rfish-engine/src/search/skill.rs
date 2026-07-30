//! Playing below full strength.
//!
//! A weakened engine must not simply search less deeply — that produces an opponent which
//! blunders at random and is no easier to plan against. Upstream instead searches at full
//! strength, keeps several candidate moves, and picks among them with a bias that widens as
//! the level drops. The result is an opponent that plays a coherent game and is beatable.
//!
//! Golden: `Stockfish/src/search.cpp`, `struct Skill` and `Skill::pick_best`.

use crate::board::types::{Move, PAWN_VALUE, VALUE_INFINITE};
use crate::state::RootMove;

/// The Elo the weakest level is calibrated to.
pub const LOWEST_ELO: i32 = 1320;
/// The Elo the strongest limited level is calibrated to.
pub const HIGHEST_ELO: i32 = 3190;

/// The strength handicap in force for one search.
#[derive(Clone, Copy, Debug)]
pub struct Skill {
    /// Zero (weakest) to 20 (no handicap), on a continuous scale.
    level: f64,
    /// The move the handicap settled on, or [`Move::NONE`] until it has picked.
    best: Move,
}

impl Skill {
    /// Read the handicap out of the two options that can set it.
    ///
    /// `uci_elo` wins when the GUI set `UCI_LimitStrength`, because a GUI that asks for a
    /// rating has asked a more specific question than one that asks for a level. The
    /// polynomial is a fit to games played between Stockfish at each level and versions of
    /// the Stash engine, so the levels correspond to real CCRL Blitz ratings rather than to
    /// a scale of the engine's own invention.
    #[must_use]
    pub fn new(skill_level: i32, uci_elo: Option<i32>) -> Skill {
        let level = match uci_elo {
            Some(elo) => {
                let e = f64::from(elo - LOWEST_ELO) / f64::from(HIGHEST_ELO - LOWEST_ELO);
                (((37.2473 * e - 40.8525) * e + 22.2943) * e - 0.311_438).clamp(0.0, 19.0)
            }
            None => f64::from(skill_level),
        };
        Skill { level, best: Move::NONE }
    }

    /// True when the handicap is doing anything at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.level < 20.0
    }

    /// The move already picked, if the handicap has picked one.
    #[must_use]
    pub fn best(&self) -> Move {
        self.best
    }

    /// True at the depth this level is meant to stop looking at.
    ///
    /// The pick happens once, at a depth set by the level, and later iterations do not
    /// revise it — that is what keeps a weak level weak even when it has time to spare.
    #[must_use]
    pub fn time_to_pick(&self, depth: i32) -> bool {
        depth == 1 + self.level as i32
    }

    /// Choose a move from the top `multi_pv` root moves, biased by the handicap.
    ///
    /// Each move gets two pushes, both scaled by weakness: one deterministic, larger the
    /// further the move is behind the best, and one random. A weak level therefore picks a
    /// move that was genuinely considered rather than a move nobody would play. Idea by
    /// Heinz van Saanen.
    pub fn pick_best(&mut self, root_moves: &[RootMove], multi_pv: usize, rng: &mut Prng) -> Move {
        let n = multi_pv.clamp(1, root_moves.len());

        // With tablebases at the root the moves are ordered by rank rather than by score,
        // so take the range explicitly instead of assuming the first is the highest.
        let mut top_score = root_moves[0].score;
        let mut min_score = root_moves[0].score;
        for rm in &root_moves[1..n] {
            top_score = top_score.max(rm.score);
            min_score = min_score.min(rm.score);
        }

        let delta = i64::from((top_score - min_score).min(PAWN_VALUE));
        let weakness = 120.0 - 2.0 * self.level;
        let mut max_score = -i64::from(VALUE_INFINITE);

        for rm in &root_moves[..n] {
            // The magic formula.
            let push = (weakness * f64::from(top_score - rm.score)) as i64
                + delta * i64::from(rng.next_u32() % weakness as u32);
            let scored = i64::from(rm.score) + push / 128;

            if scored >= max_score {
                max_score = scored;
                self.best = rm.pv[0];
            }
        }

        self.best
    }
}

/// The xorshift64* generator upstream picks weakened moves with.
///
/// Seeded from the wall clock rather than a constant: two games at the same level against
/// the same opening should not follow the same script, which is the one place in this
/// engine where reproducibility is the wrong property.
#[derive(Clone, Copy, Debug)]
pub struct Prng {
    s: u64,
}

impl Prng {
    /// A generator seeded with `seed`, which must be non-zero.
    #[must_use]
    pub fn new(seed: u64) -> Prng {
        Prng { s: if seed == 0 { 1 } else { seed } }
    }

    /// A generator seeded from the clock.
    #[must_use]
    pub fn from_clock() -> Prng {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Prng::new(nanos)
    }

    fn next_u64(&mut self) -> u64 {
        self.s ^= self.s >> 12;
        self.s ^= self.s << 25;
        self.s ^= self.s >> 27;
        self.s.wrapping_mul(2_685_821_657_736_338_717)
    }

    /// The low 32 bits of the next value, which is what the pick uses.
    pub fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_moves(scores: &[crate::board::types::Value]) -> Vec<RootMove> {
        scores
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                // Distinct moves, so the pick can be identified by which one came back.
                let mut rm = RootMove::new(Move::new(
                    crate::board::types::Square::new(i),
                    crate::board::types::Square::new(i + 8),
                ));
                rm.score = s;
                rm.pv = vec![rm.mv];
                rm
            })
            .collect()
    }

    #[test]
    fn full_strength_is_the_disabled_state() {
        assert!(!Skill::new(20, None).enabled());
        assert!(Skill::new(19, None).enabled());
        assert!(Skill::new(0, None).enabled());
    }

    #[test]
    fn the_elo_scale_spans_the_declared_range() {
        // The option's own bounds must land inside the level range, or a GUI can ask for a
        // rating the engine silently cannot play at.
        let weakest = Skill::new(20, Some(LOWEST_ELO));
        let strongest = Skill::new(20, Some(HIGHEST_ELO));
        assert!(weakest.level >= 0.0 && weakest.level < 1.0);
        assert!(strongest.level > 18.0 && strongest.level <= 19.0);
        assert!(strongest.enabled(), "a limited strength must stay limited at the top Elo");
    }

    #[test]
    fn the_elo_scale_is_monotonic() {
        let mut previous = -1.0;
        for elo in (LOWEST_ELO..=HIGHEST_ELO).step_by(10) {
            let level = Skill::new(20, Some(elo)).level;
            assert!(level >= previous, "level fell at {elo} Elo");
            previous = level;
        }
    }

    #[test]
    fn limit_strength_overrides_the_level() {
        // A GUI that sets both has asked the more specific question with the Elo.
        let a = Skill::new(0, Some(HIGHEST_ELO)).level;
        let b = Skill::new(20, Some(HIGHEST_ELO)).level;
        assert!((a - b).abs() < f64::EPSILON);
    }

    #[test]
    fn the_pick_depth_tracks_the_level() {
        assert!(Skill::new(0, None).time_to_pick(1));
        assert!(Skill::new(7, None).time_to_pick(8));
        assert!(!Skill::new(7, None).time_to_pick(9));
    }

    #[test]
    fn the_pick_stays_within_the_multi_pv_window() {
        let moves = root_moves(&[100, 90, 80, -900]);
        let mut rng = Prng::new(0xDEAD_BEEF);
        let mut skill = Skill::new(0, None);
        for _ in 0..200 {
            let picked = skill.pick_best(&moves, 3, &mut rng);
            assert!(
                moves[..3].iter().any(|rm| rm.mv == picked),
                "the handicap reached outside the window"
            );
        }
    }

    #[test]
    fn a_strong_level_almost_always_picks_the_best_move() {
        let moves = root_moves(&[100, 40, 20]);
        let mut rng = Prng::new(12345);
        let mut strong = Skill::new(19, None);
        let mut weak = Skill::new(0, None);
        let hits = |s: &mut Skill, rng: &mut Prng| {
            (0..500).filter(|_| s.pick_best(&moves, 3, rng) == moves[0].mv).count()
        };
        let strong_hits = hits(&mut strong, &mut rng);
        let weak_hits = hits(&mut weak, &mut rng);
        assert!(strong_hits > weak_hits, "a stronger level must pick the best move more often");
    }

    #[test]
    fn the_generator_does_not_collapse() {
        let mut rng = Prng::new(1);
        let first: Vec<u32> = (0..8).map(|_| rng.next_u32()).collect();
        assert!(first.windows(2).any(|w| w[0] != w[1]));
        // A zero seed would lock xorshift at zero forever, so it is redirected.
        let mut zero = Prng::new(0);
        assert_ne!(zero.next_u32(), 0);
    }
}
