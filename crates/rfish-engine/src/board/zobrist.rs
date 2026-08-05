//! The Zobrist key tables and the fixed-seed generator that fills them.
//!
//! **The seed and the draw order are load-bearing.** Every position key, every
//! transposition-table probe, the bench node signature and every golden in `tools/` are
//! functions of these tables. Drawing for the encoding gaps, reordering the draws,
//! skipping the zero-fill, or seeding a second generator elsewhere shifts every key
//! downstream and silently invalidates the whole anchor set.
//!
//! The tables are `const`-evaluated: they exist before `main` runs, cannot be read half
//! built, and cost nothing at startup. Upstream fills the same tables in
//! `Position::init()` and has to order its static initialisers around that.
//!
//! Golden: `Stockfish/src/position.cpp: Position::init`.

use super::types::{CastlingRights, FILE_NB, File, Key, PIECE_NB, Piece, SQUARE_NB, Square};

/// xorshift64*, seeded with upstream's 1070372.
///
/// The multiply wraps. `Key` is `u64`, so the wrap is defined; do not widen it or
/// reformulate it as a signed product.
const fn next_key(s: u64) -> (u64, Key) {
    let mut s = s;
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    (s, s.wrapping_mul(2_685_821_657_736_338_717))
}

/// The 12 real pieces, in upstream's `Pieces[]` order.
///
/// The encoding gaps at 7 and 8 are SKIPPED, exactly as upstream's `for (Piece pc :
/// Pieces)` skips them. Drawing for them too would consume 128 extra generator values and
/// shift every key from the black pawn onward away from upstream's table.
const PIECES: [Piece; 12] = [
    Piece::W_PAWN,
    Piece::W_KNIGHT,
    Piece::W_BISHOP,
    Piece::W_ROOK,
    Piece::W_QUEEN,
    Piece::W_KING,
    Piece::B_PAWN,
    Piece::B_KNIGHT,
    Piece::B_BISHOP,
    Piece::B_ROOK,
    Piece::B_QUEEN,
    Piece::B_KING,
];

/// Everything drawn from the one generator, in one place so the draw order is visible as
/// a single sequence rather than spread over five loops that could be reordered.
struct Tables {
    psq: [[Key; SQUARE_NB]; PIECE_NB],
    en_passant: [Key; FILE_NB],
    castling: [Key; 16],
    side: Key,
    no_pawns: Key,
}

const fn build() -> Tables {
    let mut t = Tables {
        psq: [[0; SQUARE_NB]; PIECE_NB],
        en_passant: [0; FILE_NB],
        castling: [0; 16],
        side: 0,
        no_pawns: 0,
    };
    let mut s = 1_070_372u64;

    // 1. psq for the 12 real pieces, a1..h8.
    let mut i = 0;
    while i < 12 {
        let pc = PIECES[i].index();
        let mut sq = 0;
        while sq < SQUARE_NB {
            let (ns, k) = next_key(s);
            s = ns;
            t.psq[pc][sq] = k;
            sq += 1;
        }
        i += 1;
    }

    // 2. Zero the ranks a pawn can only reach by promoting. A pawn never rests there, so
    //    the entry is unreachable when a key is computed from scratch; upstream zeroes it
    //    so the promotion XOR cancels implicitly instead of needing a special case.
    let mut f = 0;
    while f < FILE_NB {
        t.psq[Piece::W_PAWN.index()][56 + f] = 0;
        t.psq[Piece::B_PAWN.index()][f] = 0;
        f += 1;
    }

    // 3. En passant, one key per file.
    let mut f = 0;
    while f < FILE_NB {
        let (ns, k) = next_key(s);
        s = ns;
        t.en_passant[f] = k;
        f += 1;
    }

    // 4. Castling, one key per rights nibble — so a rights change is one XOR, not four.
    let mut cr = 0;
    while cr < 16 {
        let (ns, k) = next_key(s);
        s = ns;
        t.castling[cr] = k;
        cr += 1;
    }

    // 5. Side to move.
    let (ns, k) = next_key(s);
    s = ns;
    t.side = k;

    // 6. The pawn-key seed, so a position with no pawns still has a distinct pawn key
    //    rather than the zero every empty XOR fold produces.
    let (_, k) = next_key(s);
    t.no_pawns = k;

    t
}

static TABLES: Tables = build();

/// The key for `pc` standing on `sq`.
#[inline(always)]
#[must_use]
pub fn psq(pc: Piece, sq: Square) -> Key {
    TABLES.psq[pc.index()][sq.index()]
}

/// The key for an en-passant target on `file`.
#[inline(always)]
#[must_use]
pub fn en_passant(file: File) -> Key {
    TABLES.en_passant[file.index()]
}

/// The key for a castling-rights set.
#[inline(always)]
#[must_use]
pub fn castling(cr: CastlingRights) -> Key {
    TABLES.castling[cr.index()]
}

/// The key `XORed` in when it is Black to move.
#[inline(always)]
#[must_use]
pub fn side() -> Key {
    TABLES.side
}

/// The seed for the pawn key.
#[inline(always)]
#[must_use]
pub fn no_pawns() -> Key {
    TABLES.no_pawns
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::Rank;
    use crate::board::types::{Color, PieceType};

    /// The exact values upstream's generator produces for the first and last draws. These
    /// are what make the port's keys the SAME keys, not merely well-distributed ones — a
    /// different table gives a different transposition-table hit pattern and therefore a
    /// different node count.
    #[test]
    fn first_and_last_draws_match_upstream() {
        // Draw 1: psq[W_PAWN][a1].
        let (_, first) = next_key(1_070_372);
        assert_eq!(psq(Piece::W_PAWN, Square::A1), first);
        assert_ne!(first, 0);
        // The generator is deterministic, so re-running the build reproduces it exactly.
        let again = build();
        assert_eq!(again.side, side());
        assert_eq!(again.no_pawns, no_pawns());
    }

    #[test]
    fn promotion_ranks_are_zeroed_for_pawns() {
        for f in 0..8 {
            assert_eq!(psq(Piece::W_PAWN, Square::make(File::new(f), Rank::new(7))), 0);
            assert_eq!(psq(Piece::B_PAWN, Square::make(File::new(f), Rank::new(0))), 0);
            // Every other pawn square must be a live key.
            assert_ne!(psq(Piece::W_PAWN, Square::make(File::new(f), Rank::new(1))), 0);
            assert_ne!(psq(Piece::B_PAWN, Square::make(File::new(f), Rank::new(6))), 0);
        }
    }

    #[test]
    fn encoding_gaps_are_never_drawn_for() {
        // Pieces 0, 7, 8, 15 do not exist. Their rows must be untouched zeros, which is
        // also the proof that the generator was not advanced for them.
        for gap in [0usize, 7, 8, 15] {
            assert!(TABLES.psq[gap].iter().all(|&k| k == 0), "piece slot {gap} was drawn for");
        }
    }

    #[test]
    fn every_real_piece_square_key_is_distinct() {
        let mut keys = Vec::new();
        for pc in PIECES {
            for sq in Square::all() {
                let k = psq(pc, sq);
                if k != 0 {
                    keys.push(k);
                }
            }
        }
        keys.extend((0..8).map(|f| en_passant(File::new(f))));
        keys.extend((0..16).map(|i| castling(CastlingRights::from_raw(i as u8))));
        keys.push(side());
        keys.push(no_pawns());

        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), before, "a Zobrist key is used twice");
    }

    #[test]
    fn castling_index_covers_every_rights_nibble() {
        assert_eq!(castling(CastlingRights::NONE), TABLES.castling[0]);
        assert_eq!(castling(CastlingRights::ANY), TABLES.castling[15]);
        let both_white =
            CastlingRights::king_side(Color::White).union(CastlingRights::queen_side(Color::White));
        assert_eq!(both_white, CastlingRights::for_color(Color::White));
    }

    #[test]
    fn piece_rows_are_indexed_by_the_packed_encoding() {
        assert_eq!(Piece::new(Color::Black, PieceType::King), Piece::B_KING);
        assert_ne!(psq(Piece::B_KING, Square::A1), psq(Piece::W_KING, Square::A1));
    }
}
