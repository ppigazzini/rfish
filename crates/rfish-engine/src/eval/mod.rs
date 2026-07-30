//! Position evaluation.
//!
//! # Where this zone stands
//!
//! Upstream evaluates with an NNUE network and nothing else. rfish's NNUE forward pass is
//! not written yet: [`nnue`] holds the network file's structure and its loader, and
//! [`classical`] is the **scaffolding** that stands in until the forward pass lands.
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
use crate::board::types::{
    Color, VALUE_DRAW, VALUE_TB_LOSS_IN_MAX_PLY, VALUE_TB_WIN_IN_MAX_PLY, Value,
};

/// The static evaluation of `pos`, from the side to move's point of view.
///
/// `optimism` is the search's per-colour optimism term. It is threaded through even though
/// the classical scaffolding ignores it, because the NNUE path blends it into the final
/// score and the call sites must not have to change when that path lands.
#[must_use]
pub fn evaluate(pos: &Position, optimism: [Value; 2]) -> Value {
    let _ = optimism;
    let v = classical::evaluate(pos);

    // Damp the score as the fifty-move counter runs out: a winning position that cannot be
    // converted in the remaining plies is not worth its material.
    let v = v * (200 - pos.rule50_count()) / 200;
    v.clamp(VALUE_TB_LOSS_IN_MAX_PLY + 1, VALUE_TB_WIN_IN_MAX_PLY - 1)
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

    #[test]
    fn the_start_position_is_close_to_equal() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let v = evaluate(&pos, [0, 0]);
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
            assert_eq!(evaluate(&pa, [0, 0]), evaluate(&pb, [0, 0]), "{a} vs {b}");
        }
    }

    #[test]
    fn an_extra_queen_is_worth_more_than_an_extra_pawn() {
        let pawn = Position::from_fen("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", false).expect("valid");
        let queen = Position::from_fen("4k3/8/8/8/8/8/3Q4/4K3 w - - 0 1", false).expect("valid");
        assert!(evaluate(&queen, [0, 0]) > evaluate(&pawn, [0, 0]));
        assert!(evaluate(&pawn, [0, 0]) > 0);
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
        assert!(evaluate(&stale, [0, 0]) < evaluate(&fresh, [0, 0]));
    }
}
