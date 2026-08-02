//! The threat feature delta: which threat features one piece placement changes.
//!
//! **This is what rfish is the only one of the three ports not to have.** Upstream, `../zfish`
//! and `../mcfish` all maintain a dirty-threat list through `do_move` and feed it to the
//! accumulator; this port rebuilds the whole threat feature set on every evaluation and diffs
//! the two sets. Measured head to head at an identical tree, the three siblings sit at 1.000,
//! 1.001 and 0.877 of upstream and this port at 1.667 — see `docs/03-engine-eval.md`.
//!
//! Golden: `Stockfish/src/position.cpp` `Position::update_piece_threats`, and
//! `../zfish/src/engine/board/move_do_threats.zig`, which is the same algorithm written
//! without pointers and is the closer model for this file.
//!
//! **The invariant every caller must hold: the piece IS on the board at `s` when this runs.**
//! Upstream's `put_piece` updates the board and then calls in with `put = true`; its
//! `remove_piece` calls in with `put = false` and then clears the board. Both therefore see
//! the same occupancy — the one that includes the piece — and that is what makes a removal
//! the exact mirror of an addition rather than a separate case.

use crate::board::attacks::{both_attacks_bb, piece_attacks, ray_pass_bb};
use crate::board::bitboard::{Bitboard, pawn_attacks_from};
use crate::board::position::Position;
use crate::board::types::{Piece, PieceType, Square};

/// One threat feature that a placement adds or removes.
///
/// Packed into a `u32` the way `../zfish` packs it, because the list is written on the
/// `do_move` path and read on the accumulator path and nothing in between inspects a field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirtyThreat(u32);

impl DirtyThreat {
    const ADD_SHIFT: u32 = 31;
    const ATTACKER_SHIFT: u32 = 20;
    const ATTACKED_SHIFT: u32 = 16;
    const TO_SHIFT: u32 = 8;

    fn new(add: bool, attacker: Piece, attacked: Piece, from: Square, to: Square) -> DirtyThreat {
        DirtyThreat(
            (u32::from(add) << Self::ADD_SHIFT)
                | (u32::from(attacker.raw()) << Self::ATTACKER_SHIFT)
                | (u32::from(attacked.raw()) << Self::ATTACKED_SHIFT)
                | (u32::from(to.raw()) << Self::TO_SHIFT)
                | u32::from(from.raw()),
        )
    }

    /// True when the placement CREATES this threat, false when it destroys one.
    #[must_use]
    pub fn is_add(self) -> bool {
        self.0 >> Self::ADD_SHIFT != 0
    }

    /// The piece doing the threatening.
    #[must_use]
    pub fn attacker(self) -> Piece {
        Piece::from_raw(((self.0 >> Self::ATTACKER_SHIFT) & 0xF) as u8)
    }

    /// The piece being threatened.
    #[must_use]
    pub fn attacked(self) -> Piece {
        Piece::from_raw(((self.0 >> Self::ATTACKED_SHIFT) & 0xF) as u8)
    }

    /// The square the attacker stands on.
    #[must_use]
    pub fn from(self) -> Square {
        Square::new((self.0 & 0x3F) as usize)
    }

    /// The square the threatened piece stands on.
    #[must_use]
    pub fn to(self) -> Square {
        Square::new(((self.0 >> Self::TO_SHIFT) & 0x3F) as usize)
    }
}

/// Count a threatened queen only when the slider is itself a queen.
///
/// Upstream's `can_slider_threat`. The pairs this rejects are exactly the ones the feature
/// indexer maps out of range and the accumulator then discards, so recording them would be
/// pure work.
fn can_slider_threat(pc: Piece, slider: Piece) -> bool {
    pc.piece_type() != PieceType::Queen || slider.piece_type() == PieceType::Queen
}

/// Walk the sliders bearing on `s` and record what they gain or lose.
///
/// Two separate effects, and only the first applies when a king is placed:
///
/// - **Discovered.** A man at `s` blocks the ray; take it away and the slider reaches the
///   next man along, so a placement REMOVES that threat and a removal creates it — hence
///   `!put`. `ray_pass_bb` is what names the man beyond `s`.
/// - **Direct.** The slider threatens the placed piece itself, which follows `put`.
#[allow(clippy::too_many_arguments)]
fn process_sliders(
    pos: &Position,
    out: &mut Vec<DirtyThreat>,
    mut sliders: Bitboard,
    s: Square,
    pc: Piece,
    put: bool,
    no_rays: Bitboard,
    r_attacks: Bitboard,
    b_attacks: Bitboard,
    occupied_no_k: Bitboard,
    add_direct: bool,
) {
    while !sliders.is_empty() {
        let slider_sq = sliders.pop_lsb();
        let slider = pos.piece_on(slider_sq);
        let ray = ray_pass_bb(slider_sq, s);
        let discovered = ray & (r_attacks | b_attacks) & occupied_no_k;

        // At most one man can be discovered: anything between the slider and `s` would stop
        // the slider bearing on `s` at all, so it would not be in this set.
        debug_assert!(discovered.count() <= 1, "more than one discovered man on one ray");
        if !discovered.is_empty() && (ray & no_rays) != no_rays {
            let tsq = discovered.lsb();
            let tpc = pos.piece_on(tsq);
            if can_slider_threat(tpc, slider) {
                out.push(DirtyThreat::new(!put, slider, tpc, slider_sq, tsq));
            }
        }

        if add_direct && can_slider_threat(pc, slider) {
            out.push(DirtyThreat::new(put, slider, pc, slider_sq, s));
        }
    }
}

/// Append every threat feature that putting or removing `pc` at `s` changes.
///
/// `compute_ray` is upstream's `ComputeRay`: false for the two halves of a promotion swap,
/// where the ray effects cancel between them and computing either is wasted.
///
/// `no_rays` is upstream's `noRaysContaining`, the from-to mask of a moving piece: a slider
/// looking along the line the piece moves ALONG neither gains nor loses a discovery, and the
/// test `(ray & no_rays) != no_rays` suppresses that double count.
///
/// **Its neutral value is [`Bitboard::ALL`], not [`Bitboard::EMPTY`]** — upstream's default is
/// `-1ULL`. The test asks whether `no_rays` is NOT a subset of the ray, and the empty set is a
/// subset of every ray, so passing `EMPTY` silently suppresses EVERY discovered threat rather
/// than none of them. That reads as a delta that is merely incomplete, and the differential
/// test below caught it as one extra surviving feature.
pub fn update_piece_threats(
    pos: &Position,
    pc: Piece,
    put: bool,
    s: Square,
    out: &mut Vec<DirtyThreat>,
    no_rays: Bitboard,
    compute_ray: bool,
) {
    let occupied = pos.occupied();
    let rook_queens = pos.pieces(PieceType::Rook) | pos.pieces(PieceType::Queen);
    let bishop_queens = pos.pieces(PieceType::Bishop) | pos.pieces(PieceType::Queen);
    let (b_attacks, r_attacks) = both_attacks_bb(s, occupied);
    let occupied_no_k = occupied ^ pos.pieces(PieceType::King);
    let sliders = (rook_queens & r_attacks) | (bishop_queens & b_attacks);

    // `can_slider_threat` in bitboard form for the direct half: a threatened queen counts
    // only against a queen.
    let direct_sliders = if pc.piece_type() == PieceType::Queen {
        sliders & pos.pieces(PieceType::Queen)
    } else {
        sliders
    };

    if pc.piece_type() == PieceType::King {
        // A king is never a threat feature's attacker OR its target, so only the discovered
        // half survives.
        if compute_ray {
            process_sliders(
                pos,
                out,
                sliders,
                s,
                pc,
                put,
                no_rays,
                r_attacks,
                b_attacks,
                occupied_no_k,
                false,
            );
        }
        return;
    }

    let knights = pos.pieces(PieceType::Knight);
    let white_pawns = pos.pieces_of(crate::board::types::Color::White, PieceType::Pawn);
    let black_pawns = pos.pieces_of(crate::board::types::Color::Black, PieceType::Pawn);

    // Written out per type rather than through a runtime piece type, so each site resolves
    // to that type's own kernel: through a variable it is an indirect branch per attacker,
    // which is the shape `docs/03-engine-eval.md` records closing.
    let raw_threatened = match pc.piece_type() {
        PieceType::Pawn => pawn_attacks_from(pc.color(), s),
        PieceType::Knight => piece_attacks(PieceType::Knight, s, occupied),
        PieceType::Bishop => piece_attacks(PieceType::Bishop, s, occupied),
        PieceType::Rook => piece_attacks(PieceType::Rook, s, occupied),
        PieceType::Queen => piece_attacks(PieceType::Queen, s, occupied),
        PieceType::King | PieceType::None => Bitboard::EMPTY,
    };
    let mut threatened = raw_threatened & occupied_no_k;
    let mut incoming = piece_attacks(PieceType::Knight, s, Bitboard::EMPTY) & knights;

    // Restrict both directions to the (attacker, attacked) pairs the feature set encodes.
    // Upstream rejects the rest here rather than letting the indexer drop them later, and a
    // pawn is not a threat TARGET at this architecture — pawn-pawn moved to the pawn-pair
    // set — so incoming pawn threats are recorded only against knights and rooks.
    if matches!(pc.piece_type(), PieceType::Knight | PieceType::Rook) {
        incoming |= (pawn_attacks_from(crate::board::types::Color::White, s) & black_pawns)
            | (pawn_attacks_from(crate::board::types::Color::Black, s) & white_pawns);
    }

    threatened &= match pc.piece_type() {
        PieceType::Pawn => pos.pieces(PieceType::Knight) | pos.pieces(PieceType::Rook),
        PieceType::Bishop | PieceType::Rook => {
            pos.pieces(PieceType::Pawn)
                | pos.pieces(PieceType::Knight)
                | pos.pieces(PieceType::Bishop)
                | pos.pieces(PieceType::Rook)
        }
        _ => occupied_no_k,
    };

    while !threatened.is_empty() {
        let tsq = threatened.pop_lsb();
        let tpc = pos.piece_on(tsq);
        debug_assert_ne!(tsq, s);
        out.push(DirtyThreat::new(put, pc, tpc, s, tsq));
    }

    // The direct slider threats are emitted by exactly ONE of these two arms, never both.
    // With the rays computed, `process_sliders` is already walking the slider set and emits
    // them there; without, they fold in with the knights and pawns below. Doing both records
    // every slider threat twice, and doing neither drops them.
    if compute_ray {
        process_sliders(
            pos,
            out,
            sliders,
            s,
            pc,
            put,
            no_rays,
            r_attacks,
            b_attacks,
            occupied_no_k,
            true,
        );
    } else {
        incoming |= direct_sliders;
    }

    while !incoming.is_empty() {
        let asq = incoming.pop_lsb();
        let apc = pos.piece_on(asq);
        debug_assert_ne!(asq, s);
        out.push(DirtyThreat::new(put, apc, pc, asq, s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::Color;
    use crate::eval::nnue::features::{THREAT_DIMENSIONS, threat_active, threat_index};

    /// The threat feature indices active in `pos`, one sorted list per perspective.
    fn active(pos: &Position) -> [Vec<u32>; 2] {
        let mut out = [Vec::new(), Vec::new()];
        threat_active(pos, &mut out);
        for v in &mut out {
            v.sort_unstable();
        }
        out
    }

    /// `pos` with the man on `s` taken off, through a FEN round trip.
    ///
    /// Castling rights and the en-passant square are dropped rather than preserved: neither
    /// is an input to a threat feature, and keeping them can make the reduced position fail
    /// its own legality checks.
    fn without(pos: &Position, s: Square) -> Option<Position> {
        let mut rows = Vec::new();
        for rank in (0..8).rev() {
            let mut row = String::new();
            let mut gap = 0;
            for file in 0..8 {
                let sq = Square::make(file, rank);
                let pc = pos.piece_on(sq);
                if pc == Piece::NONE || sq == s {
                    gap += 1;
                    continue;
                }
                if gap > 0 {
                    row.push_str(&gap.to_string());
                    gap = 0;
                }
                row.push(pc.to_char() as char);
            }
            if gap > 0 {
                row.push_str(&gap.to_string());
            }
            rows.push(row);
        }
        let stm = if pos.side_to_move() == Color::White { "w" } else { "b" };
        Position::from_fen(&format!("{} {stm} - - 0 1", rows.join("/")), false).ok()
    }

    /// Applying one placement's delta to the reduced position must rebuild the full set.
    ///
    /// This is the property the whole per-move delta rests on: what
    /// [`update_piece_threats`] emits for a man standing on `s` is exactly the difference
    /// between the threat feature set with that man and the set without it. If it holds for
    /// every man of every type over a spread of positions, the case analysis is complete —
    /// which is the thing a node count cannot check and two earlier attempts got wrong.
    ///
    /// Kings are excluded, and not because the king path is untested: taking a king off is
    /// not a position, and every index is oriented by its own king square, so the two sides
    /// of the comparison would not be in the same coordinate system. The king branch emits
    /// discovered threats only and is covered where a king MOVES, in the `do_move` wiring.
    #[test]
    fn a_placement_delta_equals_the_difference_of_the_two_full_sets() {
        const FENS: [&str; 6] = [
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
            "2r3k1/1q1nbppp/r3p3/3pP3/pPpP4/P1Q2N2/2RN1PPP/2R4K b - b3 0 1",
            "8/8/8/2q5/3B4/8/5K1k/8 w - - 0 1",
        ];

        let mut checked = 0usize;
        for fen in FENS {
            let pos = Position::from_fen(fen, false).expect("valid fen");
            let full = active(&pos);
            let ksq = [pos.king_square(Color::White), pos.king_square(Color::Black)];

            for s in pos.occupied() {
                let pc = pos.piece_on(s);
                if pc.piece_type() == PieceType::King {
                    continue;
                }
                let Some(reduced) = without(&pos, s) else { continue };
                let base = active(&reduced);

                let mut delta = Vec::new();
                update_piece_threats(&pos, pc, true, s, &mut delta, Bitboard::ALL, true);

                for p in [Color::White, Color::Black] {
                    let i = p.index();
                    let mut got = base[i].clone();
                    for d in &delta {
                        let idx =
                            threat_index(p, d.attacker(), d.from(), d.to(), d.attacked(), ksq[i]);
                        if idx >= THREAT_DIMENSIONS {
                            continue;
                        }
                        if d.is_add() {
                            got.push(idx);
                        } else {
                            let at = got
                                .iter()
                                .position(|&x| x == idx)
                                .unwrap_or_else(|| panic!("{fen}: {s:?} removes an absent {idx}"));
                            got.remove(at);
                        }
                    }
                    got.sort_unstable();
                    assert_eq!(
                        got, full[i],
                        "{fen}: {pc:?} on {s:?}, perspective {p:?}: delta does not rebuild \
                         the full set"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} placements exercised");
    }
}
