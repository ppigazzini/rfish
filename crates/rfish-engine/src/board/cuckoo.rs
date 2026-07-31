//! Marcel van Kervinck's cuckoo tables: which single reversible move would repeat a
//! position already on the board.
//!
//! The search asks a question the repetition counter cannot answer: not "have I been here
//! before" but "can I get back there in one move". Answering it by trying every move would
//! cost a movegen at every node. Instead every reversible move's key delta — `psq[pc][s1] ^
//! psq[pc][s2] ^ side` — is precomputed into a cuckoo hash, so the question becomes two
//! table probes against the XOR of two position keys.
//!
//! Two hash functions and displacement on collision are what make it exact: a key that
//! finds no slot under `H1` is looked for under `H2`, and an insert that lands on an
//! occupied slot evicts the sitting tenant to ITS alternative slot rather than chaining.
//!
//! The table cannot be `const`: it is keyed by sliding-piece attacks, and those come from
//! magics that are searched at first use. It is built once, on first read.
//!
//! Golden: `Stockfish/src/position.cpp`, `Position::init` and `upcoming_repetition`.

use std::sync::LazyLock;

use super::attacks::piece_attacks;
use super::bitboard::Bitboard;
use super::types::{Key, Move, Piece, PieceType, SQUARE_NB, Square};
use super::zobrist;

/// Slots in each table. A power of two, so the hash is a mask.
const SIZE: usize = 8192;

/// How many reversible moves exist. Upstream asserts this exact count, and so does the
/// test below: a miscount means the piece loop or the attack tables are wrong, and the
/// symptom would otherwise be a rare missed draw many depths down.
const MOVE_COUNT: usize = 3668;

/// The twelve real pieces, in upstream's order. The encoding gaps at 7 and 8 are skipped.
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

/// The first hash: the low bits of the key.
#[inline(always)]
fn h1(key: Key) -> usize {
    (key & 0x1fff) as usize
}

/// The second hash: bits 16 upward. Disjoint from [`h1`]'s bits, so a collision under one
/// is uncorrelated with a collision under the other.
#[inline(always)]
fn h2(key: Key) -> usize {
    ((key >> 16) & 0x1fff) as usize
}

/// The two parallel tables: a key and the move that produces it.
///
/// Boxed slices rather than arrays: 64 KiB each is more than belongs in a stack frame on
/// the way to the heap.
struct Tables {
    key: Box<[Key]>,
    mv: Box<[Move]>,
}

/// Insert every reversible move, displacing tenants as needed.
///
/// Pawns are excluded: a pawn move is irreversible, so it can never close a cycle. Each
/// unordered pair `(s1, s2)` is inserted once — the move is its own inverse as far as the
/// key delta is concerned, so recording both directions would double the table for nothing.
fn build() -> Tables {
    let mut t = Tables {
        key: vec![0; SIZE].into_boxed_slice(),
        mv: vec![Move::NONE; SIZE].into_boxed_slice(),
    };
    let mut count = 0usize;

    for pc in PIECES {
        let pt = pc.piece_type();
        if pt == PieceType::Pawn {
            continue;
        }
        for a in 0..SQUARE_NB {
            let s1 = Square::new(a);
            for b in a + 1..SQUARE_NB {
                let s2 = Square::new(b);
                if !piece_attacks(pt, s1, Bitboard::EMPTY).contains(s2) {
                    continue;
                }
                let mut mv = Move::new(s1, s2);
                let mut key = zobrist::psq(pc, s1) ^ zobrist::psq(pc, s2) ^ zobrist::side();
                let mut i = h1(key);
                loop {
                    core::mem::swap(&mut t.key[i], &mut key);
                    core::mem::swap(&mut t.mv[i], &mut mv);
                    if mv.is_none() {
                        // The slot was empty, so nothing was displaced and the walk ends.
                        break;
                    }
                    // Push the evicted tenant to whichever of its two slots is not the one
                    // it just came from.
                    i = if i == h1(key) { h2(key) } else { h1(key) };
                }
                count += 1;
            }
        }
    }

    debug_assert_eq!(count, MOVE_COUNT, "the reversible-move count is a fact about chess");
    let _ = count;
    t
}

static TABLES: LazyLock<Tables> = LazyLock::new(build);

/// The move whose key delta is `key`, if the tables hold one.
///
/// Returns `None` when neither slot matches, which is the overwhelmingly common answer:
/// most key deltas correspond to no single reversible move at all.
#[inline]
#[must_use]
pub fn lookup(key: Key) -> Option<Move> {
    let t = &*TABLES;
    let i = h1(key);
    if t.key[i] == key {
        return Some(t.mv[i]);
    }
    let i = h2(key);
    if t.key[i] == key {
        return Some(t.mv[i]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The count is a fact about chess, not a golden: every unordered pair of squares one
    /// non-pawn piece can travel between on an empty board.
    #[test]
    fn the_table_holds_every_reversible_move() {
        let t = build();
        let filled = t.mv.iter().filter(|m| !m.is_none()).count();
        assert_eq!(filled, MOVE_COUNT);
    }

    /// Every inserted key must be findable under one of its two hashes. A displacement
    /// walk that ended in the wrong slot would silently lose moves.
    #[test]
    fn every_stored_key_is_reachable() {
        let t = &*TABLES;
        for i in 0..SIZE {
            if t.mv[i].is_none() {
                continue;
            }
            let key = t.key[i];
            assert!(h1(key) == i || h2(key) == i, "slot {i} is reachable by neither hash");
            assert_eq!(lookup(key), Some(t.mv[i]));
        }
    }

    /// A knight's key delta must resolve back to the knight move that produced it.
    #[test]
    fn a_known_move_round_trips() {
        let (s1, s2) = (Square::make(1, 0), Square::make(2, 2));
        let key =
            zobrist::psq(Piece::W_KNIGHT, s1) ^ zobrist::psq(Piece::W_KNIGHT, s2) ^ zobrist::side();
        let m = lookup(key).expect("b1-c3 is a reversible knight move");
        assert_eq!((m.from(), m.to()), (s1, s2));
    }
}
