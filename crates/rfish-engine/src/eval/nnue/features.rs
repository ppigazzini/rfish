//! The three feature sets the network reads.
//!
//! A feature is an index into the transformer's weight table. The network sees a position
//! only as the SET of indices that are active, so an index computed one off from upstream's
//! is not a small error — it reads a different column of weights, and the evaluation is
//! wrong in a way no assertion catches.
//!
//! Three sets are concatenated:
//!
//! | set | dimensions | what it encodes |
//! |---|---|---|
//! | [`HalfKAv2_hm`](halfka_index) | 22528 | own king square × (piece, square) |
//! | [`FullThreats`](threat_index) | 59808 | (attacker, from) attacking (attacked, to) |
//! | [`PP_3Wide`](pawn_pair_index) | 4560 | pairs of pawns within one file of each other |
//!
//! The threat and pawn-pair indices share one weight array, with the pawn-pair block
//! starting at the threat set's dimension count — so [`PP_INDEX_BASE`] equals
//! [`THREAT_DIMENSIONS`] by construction, and a test asserts it.
//!
//! Golden: `Stockfish/src/nnue/features/half_ka_v2_hm.cpp`, `full_threats.cpp`,
//! `pp_3wide.cpp`.

use std::sync::LazyLock;

use crate::board::attacks::piece_attacks;
use crate::board::bitboard::{
    Bitboard, KING_ATTACKS, KNIGHT_ATTACKS, RANK_1, RANK_8, file_bb, pawn_attacks_from,
};
use crate::board::position::Position;
use crate::board::threats::{DirtyPawnPairs, DirtyThreat};
use crate::board::types::{
    COLOR_NB, Color, Direction, PIECE_NB, Piece, PieceType, SQUARE_NB, Square,
};

// ---------------------------------------------------------------------------
// HalfKAv2_hm
// ---------------------------------------------------------------------------

/// Distinct (piece, square) slots: ten coloured piece kinds plus a colourless king.
const PS_NB: usize = 11 * SQUARE_NB;

/// Feature count for the king-piece set.
pub const HALFKA_DIMENSIONS: usize = SQUARE_NB * PS_NB / 2;

/// The per-piece base offset, from each perspective.
///
/// The convention is "W = us, B = them", so the second row is the first with the colours
/// swapped. Both kings share one slot: the network already knows where the friendly king
/// is from the bucket, and the enemy king needs no colour distinction.
const PIECE_SQUARE_INDEX: [[u16; PIECE_NB]; 2] = {
    const PS_W_PAWN: u16 = 0;
    const PS_B_PAWN: u16 = 64;
    const PS_W_KNIGHT: u16 = 2 * 64;
    const PS_B_KNIGHT: u16 = 3 * 64;
    const PS_W_BISHOP: u16 = 4 * 64;
    const PS_B_BISHOP: u16 = 5 * 64;
    const PS_W_ROOK: u16 = 6 * 64;
    const PS_B_ROOK: u16 = 7 * 64;
    const PS_W_QUEEN: u16 = 8 * 64;
    const PS_B_QUEEN: u16 = 9 * 64;
    const PS_KING: u16 = 10 * 64;
    [
        [
            0,
            PS_W_PAWN,
            PS_W_KNIGHT,
            PS_W_BISHOP,
            PS_W_ROOK,
            PS_W_QUEEN,
            PS_KING,
            0, //
            0,
            PS_B_PAWN,
            PS_B_KNIGHT,
            PS_B_BISHOP,
            PS_B_ROOK,
            PS_B_QUEEN,
            PS_KING,
            0,
        ],
        [
            0,
            PS_B_PAWN,
            PS_B_KNIGHT,
            PS_B_BISHOP,
            PS_B_ROOK,
            PS_B_QUEEN,
            PS_KING,
            0, //
            0,
            PS_W_PAWN,
            PS_W_KNIGHT,
            PS_W_BISHOP,
            PS_W_ROOK,
            PS_W_QUEEN,
            PS_KING,
            0,
        ],
    ]
};

/// Which of the 32 king buckets a king square selects, pre-multiplied by [`PS_NB`].
///
/// The board is mirrored so the king is always on the e-h files, which halves the table;
/// the bucket then encodes the king's square within that half.
const KING_BUCKETS: [u32; SQUARE_NB] = {
    const fn b(v: u32) -> u32 {
        v * PS_NB as u32
    }
    [
        b(28),
        b(29),
        b(30),
        b(31),
        b(31),
        b(30),
        b(29),
        b(28), //
        b(24),
        b(25),
        b(26),
        b(27),
        b(27),
        b(26),
        b(25),
        b(24), //
        b(20),
        b(21),
        b(22),
        b(23),
        b(23),
        b(22),
        b(21),
        b(20), //
        b(16),
        b(17),
        b(18),
        b(19),
        b(19),
        b(18),
        b(17),
        b(16), //
        b(12),
        b(13),
        b(14),
        b(15),
        b(15),
        b(14),
        b(13),
        b(12), //
        b(8),
        b(9),
        b(10),
        b(11),
        b(11),
        b(10),
        b(9),
        b(8), //
        b(4),
        b(5),
        b(6),
        b(7),
        b(7),
        b(6),
        b(5),
        b(4), //
        b(0),
        b(1),
        b(2),
        b(3),
        b(3),
        b(2),
        b(1),
        b(0),
    ]
};

/// The horizontal mirror to apply for a king on this square: 7 (mirror) on the a-d files,
/// 0 (identity) on the e-h files, so the king always ends on the e-h half.
const HALFKA_ORIENT: [u8; SQUARE_NB] = {
    let mut t = [0u8; SQUARE_NB];
    let mut s = 0;
    while s < SQUARE_NB {
        t[s] = if s % 8 < 4 { 7 } else { 0 };
        s += 1;
    }
    t
};

/// The feature index for `pc` on `s`, seen by `perspective` whose king is on `ksq`.
#[inline]
#[must_use]
pub fn halfka_index(perspective: Color, s: Square, pc: Piece, ksq: Square) -> u32 {
    let flip = 56 * perspective as u8;
    u32::from(s.raw() ^ HALFKA_ORIENT[ksq.index()] ^ flip)
        + u32::from(PIECE_SQUARE_INDEX[perspective.index()][pc.index()])
        + KING_BUCKETS[(ksq.raw() ^ flip) as usize]
}

/// The king-piece features that differ between two board states, as adds and subtracts.
///
/// A DIFF OF THE BOARD, not of the move. Nothing here reads what move was played, so there
/// is no case analysis to get wrong and no dependency on the board zone recording anything:
/// two placements differ on some set of squares, and each such square contributes at most
/// one feature to remove and one to add. It is the same "correct by construction" argument
/// the whole accumulator rests on, one level lower down, and it replaces recomputing the
/// active set and sorting it.
///
/// Both states must share `ksq`. When the king itself has moved every feature is re-indexed
/// and this cannot express it — the caller rebuilds from a base that has the right king
/// square, which is what the refresh cache is for.
///
/// ../mcfish 4dcdec6b and ../zfish 63078a65 each arrived at the same board diff.
pub fn halfka_delta(
    was: &[Piece; SQUARE_NB],
    now: &[Piece; SQUARE_NB],
    perspective: Color,
    ksq: Square,
    adds: &mut Vec<u32>,
    subs: &mut Vec<u32>,
) {
    for (i, (&before, &after)) in was.iter().zip(now.iter()).enumerate() {
        if before == after {
            continue;
        }
        let sq = Square::new(i);
        if before != Piece::NONE {
            subs.push(halfka_index(perspective, sq, before, ksq));
        }
        if after != Piece::NONE {
            adds.push(halfka_index(perspective, sq, after, ksq));
        }
    }
}

/// Every active king-piece feature, for one perspective.
pub fn halfka_active(pos: &Position, perspective: Color, out: &mut Vec<u32>) {
    let ksq = pos.king_square(perspective);
    for sq in pos.occupied() {
        out.push(halfka_index(perspective, sq, pos.piece_on(sq), ksq));
    }
}

// ---------------------------------------------------------------------------
// FullThreats
// ---------------------------------------------------------------------------

/// Feature count for the threat set.
pub const THREAT_DIMENSIONS: u32 = 59808;

/// The mirror for the threat and pawn-pair sets: the OPPOSITE of the king-piece set's.
const THREAT_ORIENT: [u8; SQUARE_NB] = {
    let mut t = [0u8; SQUARE_NB];
    let mut s = 0;
    while s < SQUARE_NB {
        t[s] = if s % 8 < 4 { 0 } else { 7 };
        s += 1;
    }
    t
};

/// How many (attacker colour, attacked kind) target classes each attacker has.
///
/// A pawn's diagonal threats only target knights and rooks — pawn-to-pawn relationships are
/// the pawn-pair set's job — so a pawn has 4 rather than 10. Kings threaten nothing the
/// network records.
const NUM_VALID_TARGETS: [u32; PIECE_NB] = [0, 4, 10, 8, 8, 10, 0, 0, 0, 4, 10, 8, 8, 10, 0, 0];

/// Which target class an (attacker kind, attacked kind) pair falls into, or `-1` for a pair
/// the network does not encode. Indexed by `kind - 1`, so pawn is row 0.
const THREAT_MAP: [[i32; 6]; 6] = [
    [-1, 0, -1, 1, -1, -1],
    [0, 1, 2, 3, 4, -1],
    [0, 1, 2, 3, -1, -1],
    [0, 1, 2, 3, -1, -1],
    [0, 1, 2, 3, 4, -1],
    [-1, -1, -1, -1, -1, -1],
];

/// The empty-board attack set of a piece, which is what the threat indices are ranked
/// within — NOT the occupancy-aware set the position actually has.
///
/// Two pieces of the same kind on the same square always produce the same numbering, which
/// is what makes an index depend on the position only through which attacks EXIST.
fn pseudo_attacks(pc: Piece, from: Square) -> Bitboard {
    match pc.piece_type() {
        PieceType::Pawn => pawn_attacks_from(pc.color(), from),
        PieceType::Knight => KNIGHT_ATTACKS[from.index()],
        PieceType::King => KING_ATTACKS[from.index()],
        pt => piece_attacks(pt, from, Bitboard::EMPTY),
    }
}

/// Everything derived from the empty-board attack sets, built once.
struct ThreatTables {
    /// For each piece, origin and destination, the destination's slot within the piece's
    /// block: how many attack slots every earlier origin used, PLUS the destination's rank
    /// within this origin's empty-board attack set.
    ///
    /// The two were separate tables, and a threat index added one to the other at every
    /// lookup — three loads behind three separately scaled bases. They are constants of each
    /// other, so they are summed once here instead. ../zfish 662d82ef made the same merge.
    /// `u16` holds it comfortably: the largest sum is a queen's, around 1400 against a
    /// ceiling of 65535, and the builder asserts it rather than trusting the arithmetic.
    slot: Box<[[[u16; SQUARE_NB]; SQUARE_NB]; PIECE_NB]>,
    /// For each (attacker, attacked) and whether `from < to`, the class base — or
    /// [`THREAT_DIMENSIONS`] when the pair is not encoded, which the caller drops.
    class_base: [[[u32; 2]; PIECE_NB]; PIECE_NB],
}

/// The pieces the tables are built for, in the order that decides their cumulative offsets.
const ALL_PIECES: [Piece; 12] = [
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

impl ThreatTables {
    fn build() -> ThreatTables {
        let mut slot = vec![[[0u16; SQUARE_NB]; SQUARE_NB]; PIECE_NB].into_boxed_slice();
        // Per piece: how many attack slots it uses in total, and where its block starts.
        let mut slots_per_piece = [0u32; PIECE_NB];
        let mut block_start = [0u32; PIECE_NB];

        let mut cumulative = 0u32;
        for pc in ALL_PIECES {
            let i = pc.index();
            let mut used = 0u32;
            for from in Square::all() {
                let attacks = pseudo_attacks(pc, from);
                for to in Square::all() {
                    // The rank of `to` is how many attacked squares come before it, and the
                    // origin's own base is what every earlier origin already used.
                    let below = Bitboard((1u64 << to.index()) - 1);
                    let rank = (attacks & below).count();
                    let combined = u16::try_from(used + rank).expect("a threat slot fits u16");
                    slot[i][from.index()][to.index()] = combined;
                }
                // A pawn on the first or last rank cannot exist, so it contributes nothing
                // and its slots are not reserved.
                let counts =
                    pc.piece_type() != PieceType::Pawn || (from.rank() >= 1 && from.rank() <= 6);
                if counts {
                    used += attacks.count();
                }
            }
            slots_per_piece[i] = used;
            block_start[i] = cumulative;
            cumulative += NUM_VALID_TARGETS[i] * used;
        }
        debug_assert_eq!(cumulative, THREAT_DIMENSIONS);

        let mut class_base = [[[0u32; 2]; PIECE_NB]; PIECE_NB];
        for attacker in ALL_PIECES {
            for attacked in ALL_PIECES {
                let a = attacker.index();
                let d = attacked.index();
                let enemy = (attacker.raw() ^ attacked.raw()) == 8;
                let at = attacker.piece_type();
                let dt = attacked.piece_type();
                let class = THREAT_MAP[at.index() - 1][dt.index() - 1];

                // Two pieces of the same kind attack each other symmetrically, so only one
                // direction is recorded: the `from > to` one. Same-colour pawns are handled
                // by the pawn-pair set instead and are excluded outright.
                let semi_excluded = at == dt && (enemy || at != PieceType::Pawn);
                let excluded = class < 0;
                let base = block_start[a]
                    + ((attacked.color().index() as u32 * (NUM_VALID_TARGETS[a] / 2))
                        + class.max(0) as u32)
                        * slots_per_piece[a];

                class_base[a][d][0] = if excluded { THREAT_DIMENSIONS } else { base };
                class_base[a][d][1] =
                    if excluded || semi_excluded { THREAT_DIMENSIONS } else { base };
            }
        }

        ThreatTables { slot: slot.try_into().expect("PIECE_NB rows"), class_base }
    }
}

static THREATS: LazyLock<ThreatTables> = LazyLock::new(ThreatTables::build);

/// The feature index for `attacker` on `from` attacking `attacked` on `to`.
///
/// Returns a value `>= THREAT_DIMENSIONS` for a pair the network does not encode; the
/// caller drops those rather than branching per piece kind, which is upstream's shape too.
#[inline]
#[must_use]
pub fn threat_index(
    perspective: Color,
    attacker: Piece,
    from: Square,
    to: Square,
    attacked: Piece,
    ksq: Square,
) -> u32 {
    let t = &*THREATS;
    let orientation = THREAT_ORIENT[ksq.index()] ^ (56 * perspective as u8);
    let from_o = (from.raw() ^ orientation) as usize;
    let to_o = (to.raw() ^ orientation) as usize;
    // Swapping the colour bit turns "White's threat" into "our threat" for either side.
    let swap = 8 * perspective as u8;
    let attacker_o = (attacker.raw() ^ swap) as usize;
    let attacked_o = (attacked.raw() ^ swap) as usize;

    t.class_base[attacker_o][attacked_o][usize::from(from_o < to_o)]
        + u32::from(t.slot[attacker_o][from_o][to_o])
}

/// Every active threat feature, for one perspective.
pub fn threat_active(pos: &Position, out: &mut [Vec<u32>; COLOR_NB]) {
    let ksq = [pos.king_square(Color::White), pos.king_square(Color::Black)];
    let occupied = pos.occupied();
    let pawn_targets = pos.pieces(PieceType::Knight) | pos.pieces(PieceType::Rook);
    let minor_slider_targets =
        pawn_targets | pos.pieces(PieceType::Pawn) | pos.pieces(PieceType::Bishop);
    let queen_targets = minor_slider_targets | pos.pieces(PieceType::Queen);

    // ONE scan, both perspectives. Which squares attack which is a fact about the position,
    // not about who is looking: only the INDEX a threat is filed under depends on the
    // perspective, through its own king square and orientation. Scanning twice recomputed
    // every attack set twice to file the same threats under two numbers.
    //
    // That also drops the old "own colour first" iteration order, which existed to match
    // upstream's index stream. Nothing depends on it here: the sets are sorted before they
    // are diffed, and rfish has no 256-entry cap for an order to decide the contents of.
    macro_rules! emit {
        ($attacker:expr, $from:expr, $to:expr, $attacked:expr) => {{
            for p in [Color::White, Color::Black] {
                let i = p.index();
                let index = threat_index(p, $attacker, $from, $to, $attacked, ksq[i]);
                if index < THREAT_DIMENSIONS {
                    out[i].push(index);
                }
            }
        }};
    }

    for c in [Color::White, Color::Black] {
        let attacker = Piece::new(c, PieceType::Pawn);
        let our_pawns = pos.pieces_of(c, PieceType::Pawn);
        let (right, left) = if c == Color::White {
            (Direction::NorthEast, Direction::NorthWest)
        } else {
            (Direction::SouthWest, Direction::SouthEast)
        };
        for dir in [right, left] {
            for to in our_pawns.shift(dir) & pawn_targets {
                let from = to.shift(reverse(dir));
                emit!(attacker, from, to, pos.piece_on(to));
            }
        }

        // Written out per piece type rather than looped over one, so `piece_attacks`
        // resolves to that type's own kernel at each site. Through a runtime piece type it
        // is an indirect branch taken once per attacker, and the predictor misses it --
        // ../zfish 24883582 measured 193K misses over its bench from this one call.
        // A knight and a queen may threaten a queen; a bishop and a rook may not.
        macro_rules! scan {
            ($pt:expr, $targets:expr) => {{
                let attacker = Piece::new(c, $pt);
                for from in pos.pieces_of(c, $pt) {
                    for to in piece_attacks($pt, from, occupied) & $targets {
                        emit!(attacker, from, to, pos.piece_on(to));
                    }
                }
            }};
        }
        scan!(PieceType::Knight, queen_targets);
        scan!(PieceType::Bishop, minor_slider_targets);
        scan!(PieceType::Rook, minor_slider_targets);
        scan!(PieceType::Queen, queen_targets);
    }
}

/// The reverse of a one-square step.
#[inline]
const fn reverse(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::South,
        Direction::South => Direction::North,
        Direction::East => Direction::West,
        Direction::West => Direction::East,
        Direction::NorthEast => Direction::SouthWest,
        Direction::SouthWest => Direction::NorthEast,
        Direction::NorthWest => Direction::SouthEast,
        Direction::SouthEast => Direction::NorthWest,
    }
}

// ---------------------------------------------------------------------------
// PP_3Wide
// ---------------------------------------------------------------------------

/// Distinct pawn slots: 48 reachable squares per colour.
const PAWN_IDS: u32 = 2 * 48;
/// Feature count for the pawn-pair set: every unordered pair of pawn slots.
pub const PP_DIMENSIONS: u32 = PAWN_IDS * (PAWN_IDS - 1) / 2;
/// Where the pawn-pair block starts in the shared threat weight array.
pub const PP_INDEX_BASE: u32 = THREAT_DIMENSIONS;

/// Squares that can host a pawn forming a pair with a pawn on `s`: its own file plus the
/// two adjacent ones, restricted to ranks 2..7, excluding `s`.
///
/// The geometry is colour-independent, which is why one table serves both.
static PAWN_PAIR_BB: LazyLock<[Bitboard; SQUARE_NB]> = LazyLock::new(|| {
    let mut t = [Bitboard::EMPTY; SQUARE_NB];
    for s in Square::all() {
        let file = file_bb(s);
        let files = file | file.shift(Direction::East) | file.shift(Direction::West);
        t[s.index()] = files & !(RANK_1 | RANK_8) & !Bitboard::from_square(s);
    }
    t
});

/// The pawn slot for a pawn of `color` on `square`.
#[inline]
const fn pawn_id(color: Color, square: u8) -> u32 {
    48 * color as u32 + square as u32 - 8
}

/// The feature index for a pawn of `color` on `from` paired with a pawn of `paired_color`
/// on `to`.
///
/// Unordered: the pair is keyed by the two slots sorted, so `(a, b)` and `(b, a)` land on
/// the same feature.
#[inline]
#[must_use]
pub fn pawn_pair_index(
    perspective: Color,
    color: Color,
    from: Square,
    to: Square,
    paired_color: Color,
    ksq: Square,
) -> u32 {
    let orientation = THREAT_ORIENT[ksq.index()] ^ (56 * perspective as u8);
    let from_o = from.raw() ^ orientation;
    let to_o = to.raw() ^ orientation;
    let color_o = Color::from_index((color as usize) ^ (perspective as usize));
    let paired_o = Color::from_index((paired_color as usize) ^ (perspective as usize));

    let id_a = pawn_id(color_o, from_o);
    let id_b = pawn_id(paired_o, to_o);
    let (hi, lo) = if id_a > id_b { (id_a, id_b) } else { (id_b, id_a) };
    hi * (hi - 1) / 2 + lo + PP_INDEX_BASE
}

/// Every active pawn-pair feature, for one perspective.
pub fn pawn_pair_active(pos: &Position, out: &mut [Vec<u32>; COLOR_NB]) {
    let ksq = [pos.king_square(Color::White), pos.king_square(Color::Black)];
    pawn_pairs_of(
        pos.pieces_of(Color::White, PieceType::Pawn),
        pos.pieces_of(Color::Black, PieceType::Pawn),
        ksq,
        out,
    );
}

/// Every pawn-pair feature of one pair of pawn bitboards, for both perspectives.
///
/// Split out of [`pawn_pair_active`] because the per-move delta has the two bitboards and
/// no position: [`DirtyPawnPairs`] records them before and after, and the pawn-pair set is
/// a pure function of them.
fn pawn_pairs_of(
    white: Bitboard,
    black: Bitboard,
    ksq: [Square; COLOR_NB],
    out: &mut [Vec<u32>; COLOR_NB],
) {
    // Walking `bb` while popping is what deduplicates same-colour pairs: only pawns after
    // `from` in square order are considered, so each unordered pair is seen once.
    let mut bb = white;
    while bb.any() {
        let from = bb.pop_lsb();
        let band = PAWN_PAIR_BB[from.index()];
        for to in band & bb {
            for p in [Color::White, Color::Black] {
                let i = p.index();
                out[i].push(pawn_pair_index(p, Color::White, from, to, Color::White, ksq[i]));
            }
        }
        for to in band & black {
            for p in [Color::White, Color::Black] {
                let i = p.index();
                out[i].push(pawn_pair_index(p, Color::White, from, to, Color::Black, ksq[i]));
            }
        }
    }

    let mut bb = black;
    while bb.any() {
        let from = bb.pop_lsb();
        let band = PAWN_PAIR_BB[from.index()];
        for to in band & bb {
            for p in [Color::White, Color::Black] {
                let i = p.index();
                out[i].push(pawn_pair_index(p, Color::Black, from, to, Color::Black, ksq[i]));
            }
        }
    }
}

/// The threat features one move adds and removes, for both perspectives.
///
/// This is the whole point of the per-move delta: the child's threat set is never
/// MATERIALISED. Upstream's `DirtyThreats` are already the difference, so all that is left
/// is to index each record against each perspective's king square and drop the pairs the
/// network does not encode.
///
/// A move may both destroy and recreate the same threat — a slider leaving a square and
/// another arriving on it — so an index can appear in both lists. That needs no netting:
/// the fold applies the sum of `adds` less the sum of `subs`, and the two rows cancel
/// exactly. It would matter only to a materialised set, which is what this replaces.
pub fn threat_delta(
    dts: &[DirtyThreat],
    ksq: [Square; COLOR_NB],
    adds: &mut [Vec<u32>; COLOR_NB],
    subs: &mut [Vec<u32>; COLOR_NB],
) {
    for dt in dts {
        let (attacker, attacked, from, to) = (dt.attacker(), dt.attacked(), dt.from(), dt.to());
        for p in Color::ALL {
            let i = p.index();
            let index = threat_index(p, attacker, from, to, attacked, ksq[i]);
            if index >= THREAT_DIMENSIONS {
                continue;
            }
            if dt.is_add() { adds[i].push(index) } else { subs[i].push(index) }
        }
    }
}

/// The pawn-pair features one move adds and removes, for both perspectives.
///
/// Unlike the threats there is no record per changed feature, because one pawn moving
/// changes every pair it belongs to. What is recorded is the two bitboards, and the pair set
/// is a pure function of them — so the delta is the set of the `before` boards against the
/// set of the `after` boards. Both are small, and the common case costs nothing at all:
/// most moves leave the pawns alone and [`DirtyPawnPairs::is_unchanged`] returns early.
pub fn pawn_pair_delta(
    dpp: &DirtyPawnPairs,
    ksq: [Square; COLOR_NB],
    adds: &mut [Vec<u32>; COLOR_NB],
    subs: &mut [Vec<u32>; COLOR_NB],
) {
    if dpp.is_unchanged() {
        return;
    }
    pawn_pairs_of(dpp.before[0], dpp.before[1], ksq, subs);
    pawn_pairs_of(dpp.after[0], dpp.after[1], ksq, adds);
}

/// The threat and pawn-pair sets share one weight array.
pub const THREAT_AND_PP_DIMENSIONS: usize = (THREAT_DIMENSIONS + PP_DIMENSIONS) as usize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::position::START_FEN;

    /// The delta the accumulator will be fed carries the child's set exactly.
    ///
    /// Stated as COUNTS per index rather than as sets, because that is what the fold applies:
    /// it adds every row in `adds` and subtracts every row in `subs`, so a feature that a
    /// move both destroys and recreates must appear in both and cancel. A set comparison
    /// would call that a bug; the accumulator does not see it at all.
    ///
    /// A perspective whose king moved is skipped: every index it sees is keyed off that
    /// square, so the whole set is re-indexed and the accumulator refreshes rather than
    /// diffing. That is the one case the per-move delta does not serve.
    #[test]
    fn the_recorded_delta_carries_the_child_set_for_both_feature_kinds() {
        use std::collections::HashMap;

        use crate::board::movegen::generate_legal;

        const FENS: [&str; 6] = [
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
            "2r3k1/1q1nbppp/r3p3/3pP3/pPpP4/P1Q2N2/2RN1PPP/2R4K b - b3 0 1",
            "n1n5/PPPk4/8/8/8/8/4Kppp/5N1N b - - 0 1",
        ];

        fn full(pos: &Position) -> [Vec<u32>; COLOR_NB] {
            let mut out = [Vec::new(), Vec::new()];
            threat_active(pos, &mut out);
            pawn_pair_active(pos, &mut out);
            out
        }
        fn counts(v: &[u32]) -> HashMap<u32, i32> {
            let mut m = HashMap::new();
            for &f in v {
                *m.entry(f).or_insert(0) += 1;
            }
            m
        }

        let mut checked = 0usize;
        for fen in FENS {
            let parent = Position::from_fen(fen, false).expect("valid fen");
            let before = full(&parent);

            for &m in generate_legal(&parent).iter() {
                let mut child = parent.clone();
                let mut dts = Vec::new();
                let gives_check = child.gives_check(m);
                let dpp = child.do_move_recording(m, gives_check, Some(&mut dts));

                let ksq = [child.king_square(Color::White), child.king_square(Color::Black)];
                let mut adds = [Vec::new(), Vec::new()];
                let mut subs = [Vec::new(), Vec::new()];
                threat_delta(&dts, ksq, &mut adds, &mut subs);
                pawn_pair_delta(&dpp, ksq, &mut adds, &mut subs);

                let after = full(&child);
                for p in Color::ALL {
                    let i = p.index();
                    if ksq[i] != parent.king_square(p) {
                        continue;
                    }
                    let mut got = counts(&before[i]);
                    for (&f, &n) in &counts(&adds[i]) {
                        *got.entry(f).or_insert(0) += n;
                    }
                    for (&f, &n) in &counts(&subs[i]) {
                        *got.entry(f).or_insert(0) -= n;
                    }
                    got.retain(|_, n| *n != 0);
                    assert_eq!(
                        got,
                        counts(&after[i]),
                        "{fen}: after {m:?}, perspective {p:?}: the delta does not carry the \
                         child set"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} moves exercised");
    }

    #[test]
    fn the_dimension_counts_are_the_ones_the_file_encodes() {
        // Each falls out of the tables rather than being asserted into them: the threat
        // count is the sum over pieces of targets x attack slots, and it must land exactly
        // on upstream's 59808 or every index after the first block is wrong.
        assert_eq!(HALFKA_DIMENSIONS, 22528);
        assert_eq!(THREAT_DIMENSIONS, 59808);
        assert_eq!(PP_DIMENSIONS, 4560);
        // The pawn-pair block starts where the threat block ends -- they share an array.
        assert_eq!(PP_INDEX_BASE, THREAT_DIMENSIONS);
        assert_eq!(THREAT_AND_PP_DIMENSIONS, 64368);
        // Force the table build, which asserts the cumulative offset in a debug build.
        let _ = &*THREATS;
    }

    #[test]
    fn every_halfka_index_is_in_range_for_every_reachable_placement() {
        for ksq in Square::all() {
            for sq in Square::all() {
                for pc in [Piece::W_PAWN, Piece::B_QUEEN, Piece::W_KING, Piece::B_KING] {
                    for p in Color::ALL {
                        let i = halfka_index(p, sq, pc, ksq);
                        assert!(i < HALFKA_DIMENSIONS as u32, "{i} out of range");
                    }
                }
            }
        }
    }

    /// The mirror must put the king on the e-h files for the king-piece set, and on the
    /// a-d files for the threat set. Getting the two the same way round reads a consistent
    /// but wrong column of weights.
    #[test]
    fn the_two_orientations_are_opposites() {
        for s in Square::all() {
            assert_ne!(HALFKA_ORIENT[s.index()], THREAT_ORIENT[s.index()]);
            assert!(HALFKA_ORIENT[s.index()] == 0 || HALFKA_ORIENT[s.index()] == 7);
        }
        // A king on a1 (a-file) is mirrored by the king-piece set and not by the threats.
        assert_eq!(HALFKA_ORIENT[Square::A1.index()], 7);
        assert_eq!(THREAT_ORIENT[Square::A1.index()], 0);
    }

    #[test]
    fn every_threat_index_from_a_real_position_is_in_range() {
        for fen in [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "k7/2n1n3/1nbNbn2/2NbRBn1/1nbRQR2/2NBRBN1/3N1N2/7K w - - 0 1",
            "K7/8/8/BNQNQNB1/N5N1/R1Q1q2r/n5n1/bnqnqnbk w - - 0 1",
        ] {
            let pos = Position::from_fen(fen, false).expect("valid");
            let mut v = [Vec::new(), Vec::new()];
            threat_active(&pos, &mut v);
            let mut w = [Vec::new(), Vec::new()];
            pawn_pair_active(&pos, &mut w);
            for p in Color::ALL {
                for &i in &v[p.index()] {
                    assert!(i < THREAT_DIMENSIONS, "{fen}: threat index {i} out of range");
                }
                for &i in &w[p.index()] {
                    assert!(
                        (PP_INDEX_BASE..PP_INDEX_BASE + PP_DIMENSIONS).contains(&i),
                        "{fen}: pawn-pair index {i} out of range"
                    );
                }
            }
        }
    }

    /// A pawn pair is unordered: the two orderings must produce one index, or the same
    /// relationship is counted twice.
    #[test]
    fn a_pawn_pair_index_is_symmetric() {
        let ksq = Square::make(4, 0);
        let a = Square::make(3, 3);
        let b = Square::make(4, 4);
        for p in Color::ALL {
            assert_eq!(
                pawn_pair_index(p, Color::White, a, b, Color::Black, ksq),
                pawn_pair_index(p, Color::Black, b, a, Color::White, ksq)
            );
        }
    }

    /// The band is the pawn's own file plus its two neighbours, ranks 2..7, minus itself.
    #[test]
    fn the_pawn_pair_band_is_three_files_wide() {
        let d4 = Square::make(3, 3);
        let band = PAWN_PAIR_BB[d4.index()];
        assert!(band.contains(Square::make(2, 4)));
        assert!(band.contains(Square::make(4, 1)));
        assert!(!band.contains(d4));
        assert!(!band.contains(Square::make(3, 0)), "rank 1 is excluded");
        assert!(!band.contains(Square::make(3, 7)), "rank 8 is excluded");
        assert!(!band.contains(Square::make(1, 3)), "two files away is excluded");
        // 3 files x 6 ranks, minus the pawn itself.
        assert_eq!(band.count(), 17);
    }

    /// The start position has no threats a network records — every piece is defended by its
    /// own side only, and pawn-to-pawn is the pawn-pair set's job.
    #[test]
    fn the_start_position_has_the_expected_feature_counts() {
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut halfka = Vec::new();
        halfka_active(&pos, Color::White, &mut halfka);
        assert_eq!(halfka.len(), 32, "one feature per piece on the board");

        let mut pp_both = [Vec::new(), Vec::new()];
        pawn_pair_active(&pos, &mut pp_both);
        let pp = &pp_both[Color::White.index()];
        // The band spans ranks 2..7, so a rank-2 pawn pairs with the rank-7 pawns on its
        // own and adjacent files as well as with its own neighbours: 7 white-white pairs,
        // 22 white-black, 7 black-black.
        assert_eq!(pp.len(), 36);
        // Every pair is recorded once. A band that included the pawn itself, or a loop that
        // did not pop as it went, would double the same relationship.
        let mut sorted = pp.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pp.len());
    }

    /// Same position, both perspectives: the index sets must differ, or the network cannot
    /// tell the two sides apart.
    #[test]
    fn the_two_perspectives_see_different_features() {
        let pos = Position::from_fen(
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            false,
        )
        .expect("valid");
        let mut both = [Vec::new(), Vec::new()];
        threat_active(&pos, &mut both);
        let (w, b) = (&both[Color::White.index()], &both[Color::Black.index()]);
        assert_eq!(w.len(), b.len(), "both sides see the same threats, differently indexed");
        assert_ne!(w, b);
    }
}
