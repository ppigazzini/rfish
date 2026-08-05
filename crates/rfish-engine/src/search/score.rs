//! What a score means, and how it reaches the GUI.
//!
//! The search works in internal units whose scale is a property of the network, not of
//! chess. Reporting them raw would make "cp 200" mean something different after every net
//! change, so upstream converts through a fitted win-rate model: the reported centipawn is
//! defined so that the same number implies the same winning chances whatever the net.
//!
//! Three kinds of score are NOT centipawns at all, and flattening them into one number
//! would lose the distinction a GUI needs:
//!
//! - a **mate** is a distance, reported as `mate N`;
//! - a **tablebase** verdict is a fact, reported at a fixed magnitude so it cannot be
//!   confused with an evaluation that merely looks large;
//! - everything else is an estimate, and only that is run through the model.
//!
//! Golden: `Stockfish/src/score.cpp`, `Stockfish/src/uci.cpp`.

use crate::board::position::Position;
use crate::board::types::{PieceType, VALUE_INFINITE, VALUE_MATE, VALUE_TB, VALUE_ZERO, Value};

/// The centipawn magnitude a tablebase verdict is reported at.
///
/// Far above any real evaluation and far below a mate score, so a GUI ordering by score
/// puts proven results above estimates and forced mates above proven results.
const TB_CP: i32 = 20000;

/// A score, in the form the protocol distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Score {
    /// Plies to mate. Negative when the side to move is the one being mated.
    Mate { plies: i32 },
    /// A tablebase result, with the distance that qualifies it.
    Tablebase { plies: i32, win: bool },
    /// An ordinary evaluation, already converted to normalised centipawns.
    InternalUnits { value: i32 },
}

impl Score {
    /// Classify `v` for the position it was produced in.
    #[must_use]
    pub fn new(v: Value, pos: &Position) -> Score {
        debug_assert!(-VALUE_INFINITE < v && v < VALUE_INFINITE);

        if !is_decisive(v) {
            Score::InternalUnits { value: to_cp(v, pos) }
        } else if v.abs() <= VALUE_TB {
            let distance = VALUE_TB - v.abs();
            if v > VALUE_ZERO {
                Score::Tablebase { plies: distance, win: true }
            } else {
                Score::Tablebase { plies: -distance, win: false }
            }
        } else {
            let distance = VALUE_MATE - v.abs();
            Score::Mate { plies: if v > VALUE_ZERO { distance } else { -distance } }
        }
    }

    /// The score as a UCI `score ...` payload.
    #[must_use]
    pub fn to_uci(self) -> String {
        match self {
            // Plies to mate, reported as MOVES: the half-ply is rounded up for the side
            // delivering it, because "mate 1" must mean a move exists now.
            Score::Mate { plies } => {
                let m = if plies > 0 { plies + 1 } else { plies } / 2;
                format!("mate {m}")
            }
            Score::Tablebase { plies, win } => {
                format!("cp {}", (if win { TB_CP } else { -TB_CP }) - plies)
            }
            Score::InternalUnits { value } => format!("cp {value}"),
        }
    }
}

/// True when the score claims a result rather than an advantage.
#[must_use]
pub fn is_decisive(v: Value) -> bool {
    v.abs() >= crate::board::types::VALUE_TB_WIN_IN_MAX_PLY
}

/// The two parameters of the win-rate model for this position's material.
///
/// Both are polynomials in the material count, fitted to long-time-control results. The
/// material dependence is what makes the same evaluation mean less in an endgame than in a
/// middlegame — there is simply less left to convert it with.
fn win_rate_params(pos: &Position) -> (f64, f64) {
    const AS: [f64; 4] = [-72.325_658_36, 185.938_320_38, -144.588_621_93, 416.449_504_46];
    const BS: [f64; 4] = [83.867_940_42, -136.061_129_97, 69.988_208_87, 47.629_014_33];

    let material = pos.count_both(PieceType::Pawn)
        + 3 * pos.count_both(PieceType::Knight)
        + 3 * pos.count_both(PieceType::Bishop)
        + 5 * pos.count_both(PieceType::Rook)
        + 9 * pos.count_both(PieceType::Queen);

    // The fit only used counts in 17..=78, and is anchored at 58.
    let m = f64::from(material.clamp(17, 78)) / 58.0;

    let a = ((AS[0] * m + AS[1]) * m + AS[2]) * m + AS[3];
    let b = ((BS[0] * m + BS[1]) * m + BS[2]) * m + BS[3];
    (a, b)
}

/// The chance of winning from `v`, in per mille.
///
/// `1 / (1 + exp((a - v) / b))` — a logistic in the evaluation, with the position's own
/// material setting both the midpoint and the steepness.
#[must_use]
pub fn win_rate_model(v: Value, pos: &Position) -> i32 {
    let (a, b) = win_rate_params(pos);
    (0.5 + 1000.0 / (1.0 + ((a - f64::from(v)) / b).exp())) as i32
}

/// `v` as normalised centipawns.
///
/// Defining the score through the win rate would give
/// `(log(1/L - 1) - log(1/W - 1)) / (log(1/L - 1) + log(1/W - 1))`; under this win-rate
/// model that reduces to `v / a`, so 100 units is the evaluation that wins as often as an
/// extra pawn does.
#[must_use]
pub fn to_cp(v: Value, pos: &Position) -> i32 {
    let (a, _) = win_rate_params(pos);
    (100.0 * f64::from(v) / a).round() as i32
}

/// Win, draw and loss chances in per mille, summing to exactly 1000.
#[must_use]
pub fn wdl(v: Value, pos: &Position) -> [i32; 3] {
    let w = win_rate_model(v, pos);
    let l = win_rate_model(-v, pos);
    [w, 1000 - w - l, l]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::{VALUE_DRAW, VALUE_TB_WIN_IN_MAX_PLY};

    #[test]
    fn a_draw_is_zero_and_the_scale_is_odd_symmetric() {
        let pos = Position::startpos();
        assert_eq!(to_cp(VALUE_DRAW, &pos), 0);
        for v in [1, 50, 137, 800].map(Value::new) {
            assert_eq!(to_cp(v, &pos), -to_cp(-v, &pos));
        }
    }

    #[test]
    fn the_normalised_scale_is_not_the_internal_one() {
        // The whole point of the conversion: the number a GUI sees is NOT the number the
        // search works in, or a net change would silently move every reported score.
        let pos = Position::startpos();
        assert_ne!(to_cp(Value::new(300), &pos), 300);
    }

    #[test]
    fn the_win_rate_is_monotonic_and_bounded() {
        let pos = Position::startpos();
        let mut previous = -1;
        for v in (-2000..=2000).step_by(50).map(Value::new) {
            let w = win_rate_model(v, &pos);
            assert!((0..=1000).contains(&w), "win rate {w} out of range at {v}");
            assert!(w >= previous, "win rate fell at {v}");
            previous = w;
        }
        // The midpoint is NOT at zero. The model is fitted to real results, where a level
        // position at full material is overwhelmingly a draw rather than a coin flip, so an
        // evaluation of zero reports a small win chance and a large draw chance.
        let [w, d, l] = wdl(VALUE_DRAW, &pos);
        assert_eq!(w, l, "a level position must be symmetric");
        assert!(d > 900, "a level opening should be mostly drawn, not {d} per mille");
    }

    #[test]
    fn wdl_always_sums_to_one_thousand() {
        let pos = Position::startpos();
        for v in [-3000, -500, 0, 25, 500, 3000].map(Value::new) {
            assert_eq!(wdl(v, &pos).iter().sum::<i32>(), 1000, "at {v}");
        }
    }

    #[test]
    fn a_mate_is_reported_in_moves_not_plies() {
        let pos = Position::startpos();
        assert_eq!(Score::new(VALUE_MATE - 1, &pos).to_uci(), "mate 1");
        assert_eq!(Score::new(VALUE_MATE - 2, &pos).to_uci(), "mate 1");
        assert_eq!(Score::new(VALUE_MATE - 3, &pos).to_uci(), "mate 2");
        assert_eq!(Score::new(-(VALUE_MATE - 2), &pos).to_uci(), "mate -1");
    }

    #[test]
    fn a_tablebase_verdict_reports_at_its_own_magnitude() {
        let pos = Position::startpos();
        // A proven win outranks any evaluation but stays below a forced mate, so a GUI
        // sorting by score never puts an estimate above a fact.
        let win = Score::new(VALUE_TB - 1, &pos);
        assert_eq!(win, Score::Tablebase { plies: 1, win: true });
        assert_eq!(win.to_uci(), "cp 19999");
        assert_eq!(Score::new(-(VALUE_TB - 1), &pos).to_uci(), "cp -19999");
        assert!(matches!(
            Score::new(VALUE_TB_WIN_IN_MAX_PLY, &pos),
            Score::Tablebase { win: true, .. }
        ));
    }

    #[test]
    fn an_ordinary_score_stays_an_estimate() {
        let pos = Position::startpos();
        assert!(matches!(Score::new(Value::new(58), &pos), Score::InternalUnits { .. }));
        assert!(matches!(Score::new(Value::new(-58), &pos), Score::InternalUnits { .. }));
    }
}
