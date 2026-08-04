//! The classical evaluation — **scaffolding, to be deleted when NNUE lands**.
//!
//! Upstream has no classical evaluation. This one exists so the search has something to
//! order by while [`super::nnue`] is being ported, and so every gate above the evaluation
//! (perft, the move picker, the search's own invariants) can be built and run before the
//! network is.
//!
//! It is deliberately SMALL and deliberately untuned: material, a piece-square table
//! interpolated between the two game phases, mobility, and a handful of pawn-structure
//! terms. Anything more would be effort spent on code with a scheduled deletion date, and
//! worse, would make the engine's strength look like a property of this file.
//!
//! Its deletion is tracked with the port's other known limitations in AGENTS.md.

use crate::board::attacks::piece_attacks;
use crate::board::bitboard::{Bitboard, file_bb, pawn_attacks_from};
use crate::board::position::Position;
use crate::board::types::{
    BISHOP_VALUE, Color, KNIGHT_VALUE, PAWN_VALUE, PieceType, QUEEN_VALUE, ROOK_VALUE, SQUARE_NB,
    Square, Value,
};

/// A midgame/endgame score pair, interpolated at the end by the game phase.
#[derive(Clone, Copy, Default, Debug)]
struct Score {
    mg: i32,
    eg: i32,
}

impl Score {
    const fn new(mg: i32, eg: i32) -> Score {
        Score { mg, eg }
    }
}

impl core::ops::AddAssign for Score {
    fn add_assign(&mut self, rhs: Score) {
        self.mg += rhs.mg;
        self.eg += rhs.eg;
    }
}

impl core::ops::SubAssign for Score {
    fn sub_assign(&mut self, rhs: Score) {
        self.mg -= rhs.mg;
        self.eg -= rhs.eg;
    }
}

/// How much each piece type contributes to the game phase.
///
/// The phase runs from 24 (every piece on the board) down to 0 (bare kings), and is what
/// interpolates between the two halves of every score below.
const PHASE_WEIGHT: [i32; 7] = [0, 0, 1, 1, 2, 4, 0];
const MAX_PHASE: i32 = 24;

/// Piece-square tables, white's point of view, a1 first.
///
/// Written as a compact per-rank sketch and mirrored for Black at lookup time, so the
/// table cannot disagree with itself between the colours.
const PSQT_MG: [[i32; SQUARE_NB]; 7] = [
    [0; 64],
    // Pawn: reward advancing, and centre control on the middle ranks.
    [
        0, 0, 0, 0, 0, 0, 0, 0, //
        -6, -4, 1, 1, 1, 1, -4, -6, //
        -6, -4, 1, 6, 6, 1, -4, -6, //
        -6, -4, 5, 16, 16, 5, -4, -6, //
        -6, -4, 9, 21, 21, 9, -4, -6, //
        -6, -4, 5, 11, 11, 5, -4, -6, //
        -6, -4, 1, 1, 1, 1, -4, -6, //
        0, 0, 0, 0, 0, 0, 0, 0,
    ],
    // Knight: strongly centralised, rim is grim.
    [
        -50, -40, -30, -30, -30, -30, -40, -50, //
        -40, -20, 0, 5, 5, 0, -20, -40, //
        -30, 5, 10, 15, 15, 10, 5, -30, //
        -30, 0, 15, 20, 20, 15, 0, -30, //
        -30, 5, 15, 20, 20, 15, 5, -30, //
        -30, 0, 10, 15, 15, 10, 0, -30, //
        -40, -20, 0, 0, 0, 0, -20, -40, //
        -50, -40, -30, -30, -30, -30, -40, -50,
    ],
    // Bishop: long diagonals, off the back rank.
    [
        -20, -10, -10, -10, -10, -10, -10, -20, //
        -10, 5, 0, 0, 0, 0, 5, -10, //
        -10, 10, 10, 10, 10, 10, 10, -10, //
        -10, 0, 10, 10, 10, 10, 0, -10, //
        -10, 5, 5, 10, 10, 5, 5, -10, //
        -10, 0, 5, 10, 10, 5, 0, -10, //
        -10, 0, 0, 0, 0, 0, 0, -10, //
        -20, -10, -10, -10, -10, -10, -10, -20,
    ],
    // Rook: the seventh rank and the centre files.
    [
        0, 0, 0, 5, 5, 0, 0, 0, //
        -5, 0, 0, 0, 0, 0, 0, -5, //
        -5, 0, 0, 0, 0, 0, 0, -5, //
        -5, 0, 0, 0, 0, 0, 0, -5, //
        -5, 0, 0, 0, 0, 0, 0, -5, //
        -5, 0, 0, 0, 0, 0, 0, -5, //
        5, 10, 10, 10, 10, 10, 10, 5, //
        0, 0, 0, 0, 0, 0, 0, 0,
    ],
    // Queen: mild centralisation, no early sorties.
    [
        -20, -10, -10, -5, -5, -10, -10, -20, //
        -10, 0, 5, 0, 0, 0, 0, -10, //
        -10, 5, 5, 5, 5, 5, 0, -10, //
        0, 0, 5, 5, 5, 5, 0, -5, //
        -5, 0, 5, 5, 5, 5, 0, -5, //
        -10, 0, 5, 5, 5, 5, 0, -10, //
        -10, 0, 0, 0, 0, 0, 0, -10, //
        -20, -10, -10, -5, -5, -10, -10, -20,
    ],
    // King, midgame: castled and behind pawns.
    [
        20, 30, 10, 0, 0, 10, 30, 20, //
        20, 20, 0, 0, 0, 0, 20, 20, //
        -10, -20, -20, -20, -20, -20, -20, -10, //
        -20, -30, -30, -40, -40, -30, -30, -20, //
        -30, -40, -40, -50, -50, -40, -40, -30, //
        -30, -40, -40, -50, -50, -40, -40, -30, //
        -30, -40, -40, -50, -50, -40, -40, -30, //
        -30, -40, -40, -50, -50, -40, -40, -30,
    ],
];

/// The endgame table differs from the midgame one only for pawns and the king, which are
/// the two pieces whose ideal square actually changes with the phase.
const PSQT_EG_PAWN: [i32; SQUARE_NB] = [
    0, 0, 0, 0, 0, 0, 0, 0, //
    2, 2, 2, 2, 2, 2, 2, 2, //
    8, 8, 8, 8, 8, 8, 8, 8, //
    18, 18, 18, 18, 18, 18, 18, 18, //
    35, 35, 35, 35, 35, 35, 35, 35, //
    62, 62, 62, 62, 62, 62, 62, 62, //
    95, 95, 95, 95, 95, 95, 95, 95, //
    0, 0, 0, 0, 0, 0, 0, 0,
];

/// In the endgame the king walks to the centre.
const PSQT_EG_KING: [i32; SQUARE_NB] = [
    -50, -30, -30, -30, -30, -30, -30, -50, //
    -30, -30, 0, 0, 0, 0, -30, -30, //
    -30, -10, 20, 30, 30, 20, -10, -30, //
    -30, -10, 30, 40, 40, 30, -10, -30, //
    -30, -10, 30, 40, 40, 30, -10, -30, //
    -30, -10, 20, 30, 30, 20, -10, -30, //
    -30, -20, -10, 0, 0, -10, -20, -30, //
    -50, -40, -30, -20, -20, -30, -40, -50,
];

/// Mobility bonus per attacked square, by piece type.
const MOBILITY: [Score; 7] = [
    Score::new(0, 0),
    Score::new(0, 0),
    Score::new(4, 4),
    Score::new(5, 5),
    Score::new(2, 4),
    Score::new(1, 2),
    Score::new(0, 0),
];

/// The static evaluation, from the side to move's point of view.
#[must_use]
pub fn evaluate(pos: &Position) -> Value {
    let mut score = Score::default();
    let mut phase = 0;

    for c in Color::ALL {
        let mut side = Score::default();
        for pt in PieceType::REAL {
            let material = match pt {
                PieceType::Pawn => PAWN_VALUE,
                PieceType::Knight => KNIGHT_VALUE,
                PieceType::Bishop => BISHOP_VALUE,
                PieceType::Rook => ROOK_VALUE,
                PieceType::Queen => QUEEN_VALUE,
                _ => 0,
            };
            for sq in pos.pieces_of(c, pt) {
                phase += PHASE_WEIGHT[pt.index()];
                // Mirror the square for Black so one table serves both colours.
                let s = sq.relative(c).index();
                let (mg_extra, eg_extra) = match pt {
                    PieceType::Pawn => (PSQT_MG[1][s], PSQT_EG_PAWN[s]),
                    PieceType::King => (PSQT_MG[6][s], PSQT_EG_KING[s]),
                    _ => (PSQT_MG[pt.index()][s], PSQT_MG[pt.index()][s]),
                };
                side += Score::new(material + mg_extra, material + eg_extra);

                if !matches!(pt, PieceType::Pawn | PieceType::King) {
                    // Mobility counts squares not occupied by our own pieces and not
                    // attacked by an enemy pawn: a square a pawn covers is not usable.
                    let enemy_pawn_attacks = pos.pieces_of(!c, PieceType::Pawn).pawn_attacks(!c);
                    let reach = piece_attacks(pt, sq, pos.occupied())
                        & !pos.colored(c)
                        & !enemy_pawn_attacks;
                    let n = reach.count() as i32;
                    side += Score::new(MOBILITY[pt.index()].mg * n, MOBILITY[pt.index()].eg * n);
                }
            }
        }

        side += pawn_structure(pos, c);

        // The bishop pair is worth more than the sum of its bishops.
        if pos.count(c, PieceType::Bishop) >= 2 {
            side += Score::new(30, 50);
        }

        if c == Color::White {
            score += side;
        } else {
            score -= side;
        }
    }

    // Interpolate: a midgame score dominates while the pieces are on, an endgame score as
    // they come off.
    let phase = phase.min(MAX_PHASE);
    let v = (score.mg * phase + score.eg * (MAX_PHASE - phase)) / MAX_PHASE;

    // Take the side-to-move's point of view FIRST, then add the tempo bonus. Adding it
    // before the negation would make a position and its colour-flipped mirror score
    // differently by twice the bonus, and the search would see a different game depending
    // on which side it happened to be.
    let v = if pos.side_to_move() == Color::White { v } else { -v };
    v + 15
}

/// Doubled, isolated and passed pawns.
fn pawn_structure(pos: &Position, c: Color) -> Score {
    let mut s = Score::default();
    let ours = pos.pieces_of(c, PieceType::Pawn);
    let theirs = pos.pieces_of(!c, PieceType::Pawn);

    for sq in ours {
        let file = file_bb(sq);
        // Doubled: another friendly pawn on the same file.
        if (ours & file).more_than_one() {
            s += Score::new(-10, -20);
        }
        // Isolated: no friendly pawn on either neighbouring file.
        let neighbours = file.shift(crate::board::types::Direction::East)
            | file.shift(crate::board::types::Direction::West);
        if (ours & neighbours).is_empty() {
            s += Score::new(-12, -18);
        }
        // Passed: no enemy pawn ahead on this or a neighbouring file.
        if (theirs & front_span(c, sq)).is_empty() {
            let rank = sq.relative_rank(c) as i32;
            s += Score::new(2 * rank * rank, 6 * rank * rank);
        }
    }
    s
}

/// The squares ahead of `sq` on its own and the two neighbouring files, from `c`'s point of
/// view — the region an enemy pawn must be absent from for `sq` to be passed.
fn front_span(c: Color, sq: Square) -> Bitboard {
    let mut span = Bitboard::EMPTY;
    let mut s = sq;
    loop {
        s = s.shift(crate::board::types::Direction::pawn_push(c));
        if !s.is_ok() || s.distance(sq) > 7 {
            break;
        }
        span |= Bitboard::from_square(s);
        span |= pawn_attacks_from(c, s.shift(crate::board::types::Direction::pawn_push(!c)));
        if s.relative_rank(c) == 7 {
            break;
        }
    }
    span & !Bitboard::from_square(sq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::position::START_FEN;

    #[test]
    fn the_start_position_is_symmetric_up_to_the_tempo_bonus() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        // Everything cancels except the side-to-move bonus.
        assert_eq!(evaluate(&pos), 15);
        // And the same from Black's side, which is what makes the bonus a tempo rather
        // than a bias toward White.
        let black =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1", false)
                .expect("valid");
        assert_eq!(evaluate(&black), 15);
    }

    #[test]
    fn material_dominates_everything_else() {
        let up_a_rook = Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", false).expect("valid");
        assert!(evaluate(&up_a_rook) > ROOK_VALUE - 200);
    }

    #[test]
    fn a_passed_pawn_beats_a_blocked_one() {
        let passed = Position::from_fen("4k3/8/8/4P3/8/8/8/4K3 w - - 0 1", false).expect("valid");
        let blocked =
            Position::from_fen("4k3/3ppp2/8/4P3/8/8/8/4K3 w - - 0 1", false).expect("valid");
        // The blocked side is also two pawns down, so compare the pawn's own contribution.
        assert!(evaluate(&passed) > 0);
        assert!(evaluate(&passed) - PAWN_VALUE > evaluate(&blocked) - 3 * PAWN_VALUE);
    }

    #[test]
    fn doubled_pawns_are_worse_than_spread_ones() {
        let doubled =
            Position::from_fen("4k3/8/8/8/4P3/4P3/8/4K3 w - - 0 1", false).expect("valid");
        let spread = Position::from_fen("4k3/8/8/8/3P4/4P3/8/4K3 w - - 0 1", false).expect("valid");
        assert!(evaluate(&spread) > evaluate(&doubled));
    }

    #[test]
    fn the_bishop_pair_is_worth_more_than_two_bishops_apart() {
        let pair = Position::from_fen("4k3/8/8/8/8/8/8/2B1KB2 w - - 0 1", false).expect("valid");
        let split = Position::from_fen("4k3/8/8/8/8/8/8/2B1KN2 w - - 0 1", false).expect("valid");
        assert!(
            evaluate(&pair) - 2 * BISHOP_VALUE > evaluate(&split) - BISHOP_VALUE - KNIGHT_VALUE
        );
    }

    #[test]
    fn the_king_prefers_the_centre_only_in_the_endgame() {
        // Bare kings: the centre is better.
        let centre = Position::from_fen("8/8/8/3K4/8/8/8/7k w - - 0 1", false).expect("valid");
        let corner = Position::from_fen("8/8/8/8/8/8/8/K6k w - - 0 1", false).expect("valid");
        assert!(evaluate(&centre) > evaluate(&corner));
    }

    #[test]
    fn front_span_covers_three_files_ahead() {
        let span = front_span(Color::White, Square::make(3, 1));
        assert!(span.contains(Square::make(3, 4)));
        assert!(span.contains(Square::make(2, 4)));
        assert!(span.contains(Square::make(4, 4)));
        assert!(!span.contains(Square::make(1, 4)));
        assert!(!span.contains(Square::make(3, 0)));
    }
}
