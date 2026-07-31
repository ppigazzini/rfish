//! Position evaluation.
//!
//! # Where this zone stands
//!
//! Upstream evaluates with an NNUE network and nothing else, and so does rfish whenever a
//! net is on disk: [`nnue`] holds the file format, the loader and the forward pass, and
//! `cargo xtask nnue-check` proves the output equals a pristine upstream build's position
//! by position. [`classical`] is the **scaffolding** that stands in for a run with NO net,
//! so that a fresh clone can still play before `cargo xtask net` has run.
//!
//! The classical term is not a feature and must not be treated as one. Do not tune it, do
//! not extend it, and do not let it acquire callers that NNUE will not satisfy — it exists
//! so the search has a score to order by while the network is being ported, and it is
//! deleted when M3 completes. See `__DEV/PORTING.md`.
//!
//! Golden: `Stockfish/src/evaluate.cpp`, `Stockfish/src/nnue/`.

pub mod classical;
pub mod nnue;

use crate::board::position::Position;
use crate::board::types::{Color, PieceType, VALUE_DRAW, Value};
#[cfg(not(feature = "eval-material"))]
use crate::board::types::{VALUE_TB_LOSS_IN_MAX_PLY, VALUE_TB_WIN_IN_MAX_PLY};

/// The static evaluation of `pos`, from the side to move's point of view.
///
/// `optimism` is the search's per-colour optimism term, and it is not a decoration: the
/// blend below scales it by how much the network's two heads DISAGREE, so a position the
/// network is unsure about lets the search's own expectation weigh more.
///
/// With no network resident this falls back to [`classical`], which ignores both.
#[cfg(not(feature = "eval-material"))]
#[must_use]
pub fn evaluate(
    pos: &Position,
    network: Option<&nnue::Network>,
    scratch: &mut nnue::Scratch,
    optimism: Value,
) -> Value {
    let v = match network {
        Some(net) => nnue_value(pos, net, scratch, optimism),
        None => classical::evaluate(pos),
    };

    // Damp the score as the fifty-move counter runs out: a winning position that cannot be
    // converted in the remaining plies is not worth its material.
    let v = v - v * pos.rule50_count() / 199;
    v.clamp(VALUE_TB_LOSS_IN_MAX_PLY + 1, VALUE_TB_WIN_IN_MAX_PLY - 1)
}

/// The spine-isolation stand-in for [`evaluate`], with the same signature.
#[cfg(feature = "eval-material")]
#[must_use]
pub fn evaluate(
    pos: &Position,
    network: Option<&nnue::Network>,
    scratch: &mut nnue::Scratch,
    optimism: Value,
) -> Value {
    // The network, the scratch buffers and the optimism term are all deliberately unused:
    // taking them out of the per-node cost is the entire point of this build.
    let _ = (network, scratch, optimism);
    material_only(pos)
}

/// Score the position by material alone, ignoring optimism, the clock and every network.
///
/// MEASUREMENT HARNESS, compiled only under the `eval-material` feature and off in every
/// shipped build. It isolates the search spine from the evaluation, so a differential
/// against an oracle patched with the SAME formula measures the search and nothing else.
///
/// The weights are written here rather than read from the engine's own tables precisely so
/// the two sides cannot drift: the trees must come out identical or the differential is
/// void, and the harness asserts that before reporting. No rule50 damping and no clamp
/// either, for the same reason -- the oracle's patched evaluation has neither.
#[cfg(feature = "eval-material")]
#[must_use]
fn material_only(pos: &Position) -> Value {
    const WEIGHT: [i32; 6] = [0, 100, 320, 330, 500, 900];
    let us = pos.side_to_move();
    let them = !us;
    let mut v = 0;
    for pt in
        [PieceType::Pawn, PieceType::Knight, PieceType::Bishop, PieceType::Rook, PieceType::Queen]
    {
        v += WEIGHT[pt.index()] * (pos.count(us, pt) - pos.count(them, pt));
    }
    v
}

#[cfg(not(feature = "eval-material"))]
/// Upstream's blend of the two network heads with the search's optimism.
///
/// Every constant is upstream's and every one was fitted against this network; changing one
/// is a strength change, not a refactor.
fn nnue_value(
    pos: &Position,
    net: &nnue::Network,
    scratch: &mut nnue::Scratch,
    optimism: Value,
) -> Value {
    let out = net.evaluate(pos, scratch);
    let mut nnue = i64::from(out.psqt + out.positional);
    let mut optimism = i64::from(optimism);

    // How far apart the material head and the positional head are. A large gap means the
    // position is sharp, and both terms below react to it.
    let complexity = i64::from((out.psqt - out.positional).abs());
    optimism += optimism * complexity / 476;
    nnue -= nnue * complexity / 18236;

    let material =
        534 * i64::from(pos.count_both(PieceType::Pawn)) + i64::from(pos.non_pawn_material_total());
    ((nnue * (77871 + material) + optimism * (7191 + material)) / 77871) as Value
}

/// True when neither side has enough material to force mate.
///
/// Cheap, exact, and independent of the evaluation function: king versus king, king and a
/// single minor versus king, and two same-coloured bishops are dead draws whatever the
/// evaluation says.
#[must_use]
pub fn is_material_draw(pos: &Position) -> bool {
    use crate::board::bitboard::Bitboard;
    use crate::board::types::PieceType;

    if pos.pieces(PieceType::Pawn).any()
        || pos.pieces(PieceType::Rook).any()
        || pos.pieces(PieceType::Queen).any()
    {
        return false;
    }
    let minors = pos.pieces(PieceType::Knight) | pos.pieces(PieceType::Bishop);
    match minors.count() {
        0 | 1 => true,
        2 => {
            // One minor each is a draw only when both are bishops on the same square
            // colour; two knights cannot force mate but that case is handled by the search.
            let white_minors = (pos.colored(Color::White) & minors).count();
            white_minors == 1
                && pos.count_both(PieceType::Bishop) == 2
                && (pos.pieces(PieceType::Bishop) & Bitboard::LIGHT_SQUARES).count() != 1
        }
        _ => false,
    }
}

/// The score of a drawn position.
#[must_use]
pub const fn draw_value() -> Value {
    VALUE_DRAW
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::position::START_FEN;

    /// Evaluate with no network, which is the fallback path.
    fn eval_classical(pos: &Position) -> Value {
        let mut scratch = nnue::Scratch::default();
        evaluate(pos, None, &mut scratch, 0)
    }

    #[test]
    fn the_start_position_is_close_to_equal() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let v = eval_classical(&pos);
        assert!(v.abs() < 100, "start position scored {v}");
    }

    #[test]
    fn evaluation_is_antisymmetric_under_a_colour_flip() {
        // Mirroring the board and swapping the side to move must give the SAME score from
        // each mover's point of view, or the search sees a different game depending on
        // which side it happens to be.
        let pairs = [
            ("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", "4k3/4p3/8/8/8/8/8/4K3 b - - 0 1"),
            ("r3k3/8/8/8/8/8/8/4K3 w - - 0 1", "4k3/8/8/8/8/8/8/R3K3 b - - 0 1"),
        ];
        for (a, b) in pairs {
            let pa = Position::from_fen(a, false).expect("valid");
            let pb = Position::from_fen(b, false).expect("valid");
            assert_eq!(eval_classical(&pa), eval_classical(&pb), "{a} vs {b}");
        }
    }

    #[test]
    fn an_extra_queen_is_worth_more_than_an_extra_pawn() {
        let pawn = Position::from_fen("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", false).expect("valid");
        let queen = Position::from_fen("4k3/8/8/8/8/8/3Q4/4K3 w - - 0 1", false).expect("valid");
        assert!(eval_classical(&queen) > eval_classical(&pawn));
        assert!(eval_classical(&pawn) > 0);
    }

    #[test]
    fn insufficient_material_is_recognised() {
        for fen in [
            "4k3/8/8/8/8/8/8/4K3 w - - 0 1",
            "4k3/8/8/8/8/8/8/4KB2 w - - 0 1",
            "4k3/8/8/8/8/8/8/4KN2 w - - 0 1",
        ] {
            let pos = Position::from_fen(fen, false).expect("valid");
            assert!(is_material_draw(&pos), "{fen}");
        }
        for fen in ["4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", "4k3/8/8/8/8/8/8/4KR2 w - - 0 1"] {
            let pos = Position::from_fen(fen, false).expect("valid");
            assert!(!is_material_draw(&pos), "{fen}");
        }
    }

    #[test]
    fn the_rule50_damping_shrinks_an_advantage() {
        let fresh = Position::from_fen("4k3/8/8/8/8/8/3Q4/4K3 w - - 0 1", false).expect("valid");
        let stale = Position::from_fen("4k3/8/8/8/8/8/3Q4/4K3 w - - 90 60", false).expect("valid");
        assert!(eval_classical(&stale) < eval_classical(&fresh));
    }
}
