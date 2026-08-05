//! Occupancy-dependent slider attacks, by magic bitboard lookup.
//!
//! A magic bitboard turns "which squares does a rook on `sq` attack given occupancy
//! `occ`?" into one multiply, one shift and one indexed load. The tables are built once,
//! on first use, by [`std::sync::LazyLock`] — not by a startup hook the caller has to
//! remember to run, and not by a `static mut` that a second thread could observe half
//! written.
//!
//! Upstream builds the same tables in `Bitboards::init()` and reaches them through a raw
//! pointer per square. Here the table is one owned array and a `Magic` carries an offset
//! into it, so every lookup is a bounds-checked slice index. The bound is the only
//! difference in the generated code, and it is what lets the whole engine forbid
//! `unsafe`.
//!
//! Golden: `Stockfish/src/attacks.cpp`, `Stockfish/src/bitboard.cpp`.

use std::sync::LazyLock;

use super::bitboard::{Bitboard, KING_ATTACKS, KNIGHT_ATTACKS, pawn_attacks_from};
use super::types::{Color, Direction, PieceType, SQUARE_NB, Square};

/// Entries in the rook attack table, summed over all 64 squares.
const ROOK_TABLE_SIZE: usize = 0x1_9000;
/// Entries in the bishop attack table, summed over all 64 squares.
const BISHOP_TABLE_SIZE: usize = 0x1480;

/// The per-square magic parameters: everything needed to turn an occupancy into an index.
#[derive(Clone, Copy, Debug, Default)]
struct Magic {
    /// The relevant-occupancy mask: the squares whose contents can block this slider,
    /// with the board edges excluded because a blocker there cannot hide anything behind
    /// it.
    mask: u64,
    /// The multiplier that maps every masked occupancy onto a distinct index.
    multiplier: u64,
    /// Where this square's block starts in the shared attack table.
    offset: usize,
    /// `64 - popcount(mask)`: how far to shift the product down.
    shift: u32,
}

impl Magic {
    /// The index into the shared table for occupancy `occ`.
    #[inline(always)]
    fn index(self, occ: Bitboard) -> usize {
        self.offset
            + (((occ.bits() & self.mask).wrapping_mul(self.multiplier) >> self.shift) as usize)
    }
}

/// Both slider tables, built together because they share the same construction.
struct SliderTables {
    rook_magics: [Magic; SQUARE_NB],
    bishop_magics: [Magic; SQUARE_NB],
    rook_attacks: Box<[Bitboard; ROOK_TABLE_SIZE]>,
    bishop_attacks: Box<[Bitboard; BISHOP_TABLE_SIZE]>,
}

/// The rook's four ray directions.
const ROOK_DIRS: [Direction; 4] =
    [Direction::North, Direction::East, Direction::South, Direction::West];
/// The bishop's four ray directions.
const BISHOP_DIRS: [Direction; 4] =
    [Direction::NorthEast, Direction::SouthEast, Direction::SouthWest, Direction::NorthWest];

/// Walk `dirs` out from `sq`, stopping on (and including) the first occupied square.
///
/// This is the definition the magic tables encode. It is also the fallback the tests
/// check every magic lookup against: a magic table is a fast index into these answers, so
/// a disagreement means the index is wrong, never that the geometry is.
#[must_use]
fn sliding_attacks(dirs: [Direction; 4], sq: Square, occupied: Bitboard) -> Bitboard {
    let mut attacks = Bitboard::EMPTY;
    for d in dirs {
        let mut s = sq;
        loop {
            let next = s.shift(d);
            // Stop at the board edge. `Square::shift` wraps, so the guard is that the
            // step stayed on the board AND moved exactly one square in Chebyshev terms.
            if !next.is_ok() || next.distance(s) != 1 {
                break;
            }
            attacks |= next;
            if occupied.contains(next) {
                break;
            }
            s = next;
        }
    }
    attacks
}

/// The relevant-occupancy mask for `sq`: the ray squares, minus the edges the ray ends on.
///
/// A blocker on the far edge blocks nothing beyond itself, so its bit carries no
/// information and excluding it halves the table.
#[must_use]
fn relevant_mask(dirs: [Direction; 4], sq: Square) -> Bitboard {
    let edges = ((super::bitboard::RANK_1 | super::bitboard::RANK_8)
        & !super::bitboard::rank_bb(sq))
        | ((super::bitboard::FILE_A | super::bitboard::FILE_H) & !super::bitboard::file_bb(sq));
    sliding_attacks(dirs, sq, Bitboard::EMPTY) & !edges
}

/// A xorshift64* generator, so table construction is deterministic.
///
/// Upstream seeds a PRNG per rank and searches for magics that happen to work. The search
/// has to be reproducible or two builds of the same source get different tables, so the
/// generator is written out here rather than taken from a crate whose algorithm could
/// change under a version bump.
struct Prng(u64);

impl Prng {
    const fn new(seed: u64) -> Prng {
        Prng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(2_685_821_657_736_338_717)
    }

    /// A word with few bits set. A magic must scatter the masked occupancy across the
    /// high bits, and a sparse multiplier does that far more often than a dense one — the
    /// search finds one in tens of tries instead of millions.
    fn sparse_u64(&mut self) -> u64 {
        self.next_u64() & self.next_u64() & self.next_u64()
    }
}

/// Upstream's per-rank seeds, so the search takes the same path on every build.
const MAGIC_SEEDS: [u64; 8] = [728, 10316, 55013, 32803, 12281, 15100, 16645, 255];

/// Enumerate every subset of `mask` in Carry-Rippler order.
///
/// The classic trick: `sub = (sub - mask) & mask` walks all `2^popcount(mask)` subsets,
/// ending back at zero.
fn subsets(mask: u64) -> impl Iterator<Item = u64> {
    let mut sub = 0u64;
    let mut done = false;
    core::iter::from_fn(move || {
        if done {
            return None;
        }
        let cur = sub;
        sub = sub.wrapping_sub(mask) & mask;
        done = sub == 0;
        Some(cur)
    })
}

/// Build one slider's magics and fill its block of the shared attack table.
fn build_magics(
    dirs: [Direction; 4],
    table: &mut [Bitboard],
) -> Result<[Magic; SQUARE_NB], &'static str> {
    let mut magics = [Magic::default(); SQUARE_NB];
    let mut offset = 0usize;
    // Scratch, reused per square: `epoch` records which attempt last wrote a slot, so a
    // failed candidate costs nothing to undo.
    let mut epoch = vec![0u32; 1 << 12];
    let mut attempt = 0u32;

    for sq in Square::all() {
        let mask = relevant_mask(dirs, sq).bits();
        let bits = mask.count_ones();
        let size = 1usize << bits;

        // Precompute the occupancy/attack pairs this square must satisfy.
        let occupancies: Vec<u64> = subsets(mask).collect();
        debug_assert_eq!(occupancies.len(), size);
        let references: Vec<Bitboard> = occupancies
            .iter()
            .map(|&o| sliding_attacks(dirs, sq, Bitboard::from_bits(o)))
            .collect();

        let mut rng = Prng::new(MAGIC_SEEDS[sq.rank()]);
        let shift = 64 - bits;

        // Both scratch views are EXACTLY `size` long, taken once per square. The search
        // below already tests `idx >= size` before it touches either, so that one test is
        // what proves every index under it in bounds -- and against a slice of length
        // `size` LLVM can see that and drop the checks.
        //
        // Indexed as `table[offset + idx]` and `epoch[idx]` against their whole runtime
        // lengths instead, the store alone cost 57.4M and the epoch pair another 34.9M, on
        // an inner loop whose actual work is a multiply and a shift. `epoch` keeps its
        // full-length storage: `attempt` is what invalidates a stale slot, and reslicing it
        // per square does not disturb that.
        let slot = &mut table[offset..offset + size];
        let ep = &mut epoch[..size];

        // Search for a multiplier that is injective on the reference set. A collision is
        // acceptable only when both occupancies produce the SAME attack set, which is why
        // the check compares attacks rather than indices.
        let multiplier = 'search: loop {
            let candidate = loop {
                let c = rng.sparse_u64();
                // A magic must move at least 6 bits of the mask into the top byte, or the
                // index it produces is too clustered to ever be injective. Upstream uses
                // the same filter to skip hopeless candidates cheaply.
                if (mask.wrapping_mul(c) >> 56).count_ones() >= 6 {
                    break c;
                }
            };

            attempt += 1;
            // Walked as a pair rather than by index, for the same reason: `i` indexed two
            // more runtime-length slices to reach values a zip hands over unchecked.
            for (&occ, &reference) in occupancies.iter().zip(references.iter()) {
                let idx = ((occ & mask).wrapping_mul(candidate) >> shift) as usize;
                if idx >= size {
                    continue 'search;
                }
                if ep[idx] < attempt {
                    ep[idx] = attempt;
                    slot[idx] = reference;
                } else if slot[idx] != reference {
                    continue 'search;
                }
            }
            break candidate;
        };

        magics[sq.index()] = Magic { mask, multiplier, offset, shift };
        offset += size;
    }

    if offset == table.len() { Ok(magics) } else { Err("slider table size does not match") }
}

impl SliderTables {
    fn build() -> SliderTables {
        let mut rook_attacks = vec![Bitboard::EMPTY; ROOK_TABLE_SIZE];
        let mut bishop_attacks = vec![Bitboard::EMPTY; BISHOP_TABLE_SIZE];
        let rook_magics =
            build_magics(ROOK_DIRS, &mut rook_attacks).expect("rook table size is 0x19000");
        let bishop_magics =
            build_magics(BISHOP_DIRS, &mut bishop_attacks).expect("bishop table size is 0x1480");

        // `try_into` on a boxed slice of the right length is the safe equivalent of the
        // C++ `new Bitboard[N]` plus a pointer cast; it cannot silently accept a short
        // buffer.
        let rook_attacks: Box<[Bitboard; ROOK_TABLE_SIZE]> =
            rook_attacks.into_boxed_slice().try_into().expect("rook table has ROOK_TABLE_SIZE");
        let bishop_attacks: Box<[Bitboard; BISHOP_TABLE_SIZE]> = bishop_attacks
            .into_boxed_slice()
            .try_into()
            .expect("bishop table has BISHOP_TABLE_SIZE");

        SliderTables { rook_magics, bishop_magics, rook_attacks, bishop_attacks }
    }
}

/// The slider tables, built on first use.
///
/// Every access is one relaxed load of an already-initialised flag, which the branch
/// predictor resolves for free after the first call. Making it a `LazyLock` rather than a
/// startup hook removes the whole class of "used before init" bugs the C++ orders its
/// static initialisers to avoid.
static SLIDERS: LazyLock<SliderTables> = LazyLock::new(SliderTables::build);

/// A borrow of the slider tables, taken once and read many times.
///
/// A `LazyLock` costs its check per DEREF, not once per program: the acquire load of the
/// `Once` state is a real load that LLVM may not hoist across the loads around it, and a
/// caller like [`crate::board::threats::update_piece_threats`] derefs dozens of times.
/// Measured on a bench: 17.7M instructions -- 0.9% of the whole run -- sat in `Once`.
///
/// Take this at the top of such a caller and read through it. The free functions below stay
/// for callers that ask once.
#[derive(Clone, Copy)]
pub struct Sliders(&'static SliderTables);

impl core::fmt::Debug for Sliders {
    /// The tables themselves are 840 KiB and say nothing a reader wants; the borrow is the
    /// whole value.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Sliders")
    }
}

/// Borrow the slider tables, paying the initialisation check once.
#[inline(always)]
#[must_use]
pub fn sliders() -> Sliders {
    Sliders(&SLIDERS)
}

impl Sliders {
    /// Squares a rook on `sq` attacks through occupancy `occ`.
    #[inline(always)]
    #[must_use]
    pub fn rook(self, sq: Square, occ: Bitboard) -> Bitboard {
        self.0.rook_attacks[self.0.rook_magics[sq.index()].index(occ)]
    }

    /// Squares a bishop on `sq` attacks through occupancy `occ`.
    #[inline(always)]
    #[must_use]
    pub fn bishop(self, sq: Square, occ: Bitboard) -> Bitboard {
        self.0.bishop_attacks[self.0.bishop_magics[sq.index()].index(occ)]
    }

    /// Squares a queen on `sq` attacks through occupancy `occ`.
    #[inline(always)]
    #[must_use]
    pub fn queen(self, sq: Square, occ: Bitboard) -> Bitboard {
        self.rook(sq, occ) | self.bishop(sq, occ)
    }

    /// Bishop and rook attacks from one square in a single borrow.
    #[inline(always)]
    #[must_use]
    pub fn both(self, sq: Square, occ: Bitboard) -> (Bitboard, Bitboard) {
        (self.bishop(sq, occ), self.rook(sq, occ))
    }

    /// The attack set of `pt` from `sq`, through this borrow. Mirrors [`attacks_from`].
    ///
    /// # Panics
    /// Panics for [`PieceType::None`], as [`attacks_from`] does.
    #[inline(always)]
    #[must_use]
    pub fn from(self, c: Color, pt: PieceType, sq: Square, occ: Bitboard) -> Bitboard {
        match pt {
            PieceType::Pawn => pawn_attacks_from(c, sq),
            PieceType::Knight => KNIGHT_ATTACKS[sq.index()],
            PieceType::Bishop => self.bishop(sq, occ),
            PieceType::Rook => self.rook(sq, occ),
            PieceType::Queen => self.queen(sq, occ),
            PieceType::King => KING_ATTACKS[sq.index()],
            PieceType::None => panic!("no attack set for PieceType::None"),
        }
    }

    /// The colourless form of [`Sliders::from`], for the sites that have excluded pawns.
    #[inline(always)]
    #[must_use]
    pub fn piece(self, pt: PieceType, sq: Square, occ: Bitboard) -> Bitboard {
        debug_assert!(pt != PieceType::Pawn && pt != PieceType::None);
        self.from(Color::White, pt, sq, occ)
    }
}

/// Squares a rook on `sq` attacks through occupancy `occ`.
#[inline(always)]
#[must_use]
pub fn rook_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    let t = &*SLIDERS;
    t.rook_attacks[t.rook_magics[sq.index()].index(occ)]
}

/// Squares a bishop on `sq` attacks through occupancy `occ`.
#[inline(always)]
#[must_use]
pub fn bishop_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    let t = &*SLIDERS;
    t.bishop_attacks[t.bishop_magics[sq.index()].index(occ)]
}

/// Squares a queen on `sq` attacks through occupancy `occ`.
#[inline(always)]
#[must_use]
pub fn queen_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    rook_attacks(sq, occ) | bishop_attacks(sq, occ)
}

/// Squares a piece of type `pt` and colour `c` on `sq` attacks through occupancy `occ`.
///
/// Upstream's `attacks_bb(pt, s, occupied)`, with pawns folded in: the C++ asserts pawns
/// never reach it because their attacks are colour-dependent, and the colour argument
/// here removes the need for that assertion.
///
/// # Panics
/// Panics for [`PieceType::None`], which has no attack set.
#[inline]
#[must_use]
pub fn attacks_from(c: Color, pt: PieceType, sq: Square, occ: Bitboard) -> Bitboard {
    match pt {
        PieceType::Pawn => pawn_attacks_from(c, sq),
        PieceType::Knight => KNIGHT_ATTACKS[sq.index()],
        PieceType::Bishop => bishop_attacks(sq, occ),
        PieceType::Rook => rook_attacks(sq, occ),
        PieceType::Queen => queen_attacks(sq, occ),
        PieceType::King => KING_ATTACKS[sq.index()],
        PieceType::None => panic!("no attack set for PieceType::None"),
    }
}

/// Squares a non-pawn piece of type `pt` on `sq` attacks through occupancy `occ`.
///
/// The colourless form, for the many call sites that have already excluded pawns.
///
/// # Panics
/// Panics for [`PieceType::None`] and [`PieceType::Pawn`].
#[inline(always)]
#[must_use]
pub fn piece_attacks(pt: PieceType, sq: Square, occ: Bitboard) -> Bitboard {
    debug_assert!(pt != PieceType::Pawn && pt != PieceType::None);
    attacks_from(Color::White, pt, sq, occ)
}

// ---------------------------------------------------------------------------
// Ray geometry: between and line
// ---------------------------------------------------------------------------

/// Both ray tables, sharing one build because `line` is `between` extended to the edges.
struct RayTables {
    /// The squares strictly between two squares on a common ray, PLUS the destination.
    ///
    /// The trailing destination bit is upstream's convention and load-bearing: `between_bb`
    /// is used to build the set a checked king may block or capture on, and the checking
    /// piece's own square must be in it.
    between: Box<[[Bitboard; SQUARE_NB]; SQUARE_NB]>,
    /// The full line through two squares, empty when they share no rank, file or diagonal.
    line: Box<[[Bitboard; SQUARE_NB]; SQUARE_NB]>,
    /// The ray from `s1` THROUGH `s2` and on to the edge, on an otherwise empty board.
    ///
    /// Upstream's `RayPassBB`, and what the threat delta needs that `between` cannot give:
    /// when a piece leaves `s2`, the first man further along this ray is the one a slider on
    /// `s1` newly attacks. `between` stops at `s2` and so cannot name it.
    ray_pass: Box<[[Bitboard; SQUARE_NB]; SQUARE_NB]>,
}

impl RayTables {
    fn build() -> RayTables {
        let mut between = vec![[Bitboard::EMPTY; SQUARE_NB]; SQUARE_NB];
        let mut line = vec![[Bitboard::EMPTY; SQUARE_NB]; SQUARE_NB];
        let mut ray_pass = vec![[Bitboard::EMPTY; SQUARE_NB]; SQUARE_NB];

        for s1 in Square::all() {
            for s2 in Square::all() {
                for dirs in [ROOK_DIRS, BISHOP_DIRS] {
                    if sliding_attacks(dirs, s1, Bitboard::EMPTY).contains(s2) {
                        line[s1.index()][s2.index()] = (sliding_attacks(dirs, s1, Bitboard::EMPTY)
                            & sliding_attacks(dirs, s2, Bitboard::EMPTY))
                            | s1
                            | s2;
                        between[s1.index()][s2.index()] =
                            sliding_attacks(dirs, s1, Bitboard::from_square(s2))
                                & sliding_attacks(dirs, s2, Bitboard::from_square(s1));
                        // Upstream `attacks_bb(pt, s1, 0) & (attacks_bb(pt, s2, s1) | s2)`:
                        // everything s1 sees on an empty board, intersected with what s2
                        // sees once s1 blocks it -- which is the far side of s2, plus s2.
                        ray_pass[s1.index()][s2.index()] =
                            sliding_attacks(dirs, s1, Bitboard::EMPTY)
                                & (sliding_attacks(dirs, s2, Bitboard::from_square(s1)) | s2);
                    }
                }
                // Upstream includes s2 unconditionally, so a knight check -- which shares
                // no ray with the king -- yields exactly the checker's square.
                between[s1.index()][s2.index()] |= s2;
            }
        }

        RayTables {
            between: between.into_boxed_slice().try_into().expect("64 rows"),
            line: line.into_boxed_slice().try_into().expect("64 rows"),
            ray_pass: ray_pass.into_boxed_slice().try_into().expect("64 rows"),
        }
    }
}

static RAYS: LazyLock<RayTables> = LazyLock::new(RayTables::build);

/// The squares strictly between `a` and `b`, plus `b` itself.
///
/// Empty of everything but `b` when the two share no ray. See [`RayTables::between`] for
/// why `b` is included.
#[inline(always)]
#[must_use]
pub fn between_bb(a: Square, b: Square) -> Bitboard {
    RAYS.between[a.index()][b.index()]
}

/// The whole line through `a` and `b`, or the empty set when they share no ray.
#[inline(always)]
#[must_use]
pub fn line_bb(a: Square, b: Square) -> Bitboard {
    RAYS.line[a.index()][b.index()]
}

/// The ray from `a` through `b` and on to the edge, empty when they share no ray.
///
/// The threat delta's discovered-attack test: what a slider on `a` reaches once the man on
/// `b` is gone. See [`RayTables::ray_pass`].
#[inline(always)]
#[must_use]
pub fn ray_pass_bb(a: Square, b: Square) -> Bitboard {
    RAYS.ray_pass[a.index()][b.index()]
}

/// Bishop and rook attacks from one square in a single lookup.
///
/// Upstream's `both_attacks_bb`. The threat delta needs both sets at the same square and
/// the same occupancy, and asking twice recomputes the occupancy mask twice.
#[inline(always)]
#[must_use]
pub fn both_attacks_bb(sq: Square, occ: Bitboard) -> (Bitboard, Bitboard) {
    (bishop_attacks(sq, occ), rook_attacks(sq, occ))
}

/// True when the three squares are collinear.
///
/// The pin test: a pinned piece may only move along the line joining its king and the
/// pinner.
#[inline(always)]
#[must_use]
pub fn aligned(a: Square, b: Square, c: Square) -> bool {
    line_bb(a, b).contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::bitboard::{FILE_A, RANK_1};

    /// The magic lookup is an index into precomputed answers, so the property that
    /// matters is that it agrees with the ray walk on every occupancy the walk can see.
    /// Checking all 2^n subsets per square is exhaustive for the masked bits, which is
    /// exactly the domain the magic has to be injective over.
    #[test]
    fn magic_lookups_agree_with_the_ray_walk() {
        for sq in Square::all() {
            for (dirs, lookup) in [
                (ROOK_DIRS, rook_attacks as fn(Square, Bitboard) -> Bitboard),
                (BISHOP_DIRS, bishop_attacks as fn(Square, Bitboard) -> Bitboard),
            ] {
                let mask = relevant_mask(dirs, sq).bits();
                for occ in subsets(mask) {
                    let expected = sliding_attacks(dirs, sq, Bitboard::from_bits(occ));
                    assert_eq!(
                        lookup(sq, Bitboard::from_bits(occ)),
                        expected,
                        "square {sq}, occ {occ:#x}"
                    );
                }
            }
        }
    }

    /// Occupancy outside the relevant mask must not change the answer: that is the whole
    /// reason the edges can be dropped from the mask.
    #[test]
    fn irrelevant_occupancy_is_ignored() {
        for sq in Square::all() {
            let mask = relevant_mask(ROOK_DIRS, sq);
            let noise = !mask & !Bitboard::from_square(sq);
            assert_eq!(rook_attacks(sq, Bitboard::EMPTY), rook_attacks(sq, noise & !mask));
        }
    }

    #[test]
    fn empty_board_slider_reach_is_the_full_ray() {
        // A rook anywhere reaches 14 squares on an empty board; a bishop 7 to 13.
        for sq in Square::all() {
            assert_eq!(rook_attacks(sq, Bitboard::EMPTY).count(), 14);
            let b = bishop_attacks(sq, Bitboard::EMPTY).count();
            assert!((7..=13).contains(&b), "bishop on {sq} reaches {b}");
        }
        assert_eq!(queen_attacks(Square::A1, Bitboard::EMPTY).count(), 21);
    }

    #[test]
    fn between_includes_the_destination_and_excludes_the_origin() {
        let a1 = Square::A1;
        let d1 = Square::make(3, 0);
        let b = between_bb(a1, d1);
        assert!(!b.contains(a1));
        assert!(b.contains(d1));
        assert!(b.contains(Square::make(1, 0)) && b.contains(Square::make(2, 0)));
        // Off-ray squares still yield the destination alone, which is what makes the
        // knight-check case fall out without a special path.
        let knight_sq = Square::make(1, 2);
        assert_eq!(between_bb(a1, knight_sq), Bitboard::from_square(knight_sq));
    }

    #[test]
    fn line_spans_the_whole_ray_and_is_symmetric() {
        assert_eq!(line_bb(Square::A1, Square::H1), RANK_1);
        assert_eq!(line_bb(Square::A1, Square::A8), FILE_A);
        assert!(line_bb(Square::A1, Square::make(1, 2)).is_empty());
        for a in Square::all() {
            for b in Square::all() {
                assert_eq!(line_bb(a, b), line_bb(b, a));
            }
        }
        assert!(aligned(Square::A1, Square::make(3, 0), Square::H1));
        assert!(!aligned(Square::A1, Square::make(3, 0), Square::A8));
    }

    #[test]
    fn subsets_enumerates_each_subset_exactly_once() {
        let mask = 0b1011_0110u64;
        let all: Vec<u64> = subsets(mask).collect();
        assert_eq!(all.len(), 1 << mask.count_ones());
        let mut sorted = all.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
        assert!(all.iter().all(|s| s & !mask == 0));
    }
}

#[cfg(test)]
mod ray_pass_tests {
    use super::*;

    fn sq(name: &str) -> Square {
        let b = name.as_bytes();
        Square::make((b[0] - b'a') as usize, (b[1] - b'1') as usize)
    }

    fn set(names: &[&str]) -> Bitboard {
        names.iter().fold(Bitboard::EMPTY, |acc, n| acc | sq(n))
    }

    /// The whole ray from `a` through `b` to the edge, `a` excluded.
    ///
    /// It deliberately keeps the squares BETWEEN the two. At the only call site those are
    /// provably empty -- a man between the slider and `s` would stop the slider attacking
    /// `s`, so it would not be in the slider set at all -- which is what lets upstream
    /// assert the discovered set holds at most one piece.
    #[test]
    fn a_ray_passes_through_the_far_square_to_the_edge() {
        let f = |a: &str, b: &str| ray_pass_bb(sq(a), sq(b));
        assert_eq!(f("a1", "a4"), set(&["a2", "a3", "a4", "a5", "a6", "a7", "a8"]), "file");
        assert_eq!(f("a1", "d4"), set(&["b2", "c3", "d4", "e5", "f6", "g7", "h8"]), "diagonal");
        assert_eq!(f("a1", "d1"), set(&["b1", "c1", "d1", "e1", "f1", "g1", "h1"]), "rank");
        assert_eq!(f("h8", "e5"), set(&["g7", "f6", "e5", "d4", "c3", "b2", "a1"]), "reversed");
        assert_eq!(f("a1", "b3"), Bitboard::EMPTY, "not on a shared ray");
        assert_eq!(f("d4", "d4"), Bitboard::EMPTY, "a square shares no ray with itself");
    }

    /// Both halves must agree with the single-piece lookups they replace.
    #[test]
    fn both_attacks_agrees_with_the_separate_lookups() {
        let occ = set(&["d4", "f6"]);
        for s in Square::all() {
            let (b, r) = both_attacks_bb(s, occ);
            assert_eq!(b, bishop_attacks(s, occ), "{s:?} bishop");
            assert_eq!(r, rook_attacks(s, occ), "{s:?} rook");
        }
    }
}
