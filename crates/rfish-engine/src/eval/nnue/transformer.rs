//! The feature transformer: the first and by far the largest layer.
//!
//! It turns the position's active feature indices into 1024 accumulated `i16` values per
//! perspective, plus an 8-bucket PSQT head. Ninety-nine percent of the network's weights
//! live here, and the whole design of NNUE is that this layer can be updated incrementally
//! rather than recomputed.
//!
//! # The accumulator is updated by DIFFING FEATURE SETS
//!
//! Upstream patches the accumulator from a per-move delta: `do_move` records which features
//! a move creates and destroys, and the accumulator applies exactly those. That needs the
//! board zone to compute a threat delta from the move's geometry — several hundred lines of
//! dense case analysis, and the reason this was the port's last open item.
//!
//! rfish takes a different route to the same place. The accumulator is a SUM OVER THE ACTIVE
//! SET, so any two positions' accumulators differ by exactly the set difference of their
//! features. Recomputing the active set is cheap — it is bitboard work — while applying a
//! feature is expensive, because each one touches 1024 weights. So rfish recomputes the SET
//! and diffs it against the last one, then applies only the difference.
//!
//! That is correct by construction rather than by case analysis: there is no delta logic to
//! get wrong, and `cargo xtask nnue-check` compares the result against upstream's own
//! evaluation position by position. It also handles the case upstream needs a king-bucket
//! cache for — a king move invalidating every feature — with no special path, because it is
//! simply a large diff.
//!
//! The cost is one set recomputation per evaluation that upstream does not pay. The saving
//! is the 1024-wide weight row for every feature that did NOT change, which is most of them.
//!
//! Golden: `Stockfish/src/nnue/nnue_feature_transformer.h`,
//! `Stockfish/src/nnue/nnue_accumulator.cpp`.

use core::simd::Simd;

use crate::board::position::Position;
use crate::board::threats::{DirtyPawnPairs, DirtyThreat};
use crate::board::types::{COLOR_NB, Color, Key, MAX_PLY, Piece, SQUARE_NB, Square};

use super::common::{Aligned, FT_MAX_VAL, L1, NetError, NetReader, NetWriter, PSQT_BUCKETS};
use super::features::{
    HALFKA_DIMENSIONS, THREAT_AND_PP_DIMENSIONS, halfka_delta, pawn_pair_active, pawn_pair_delta,
    threat_active, threat_delta,
};

/// The transformer's weights.
///
/// Around 112 MiB, all heap-allocated: an array this size cannot live in a stack frame and
/// cannot be a `static` without making the binary that size.
pub struct FeatureTransformer {
    /// One bias per output, shared by both perspectives.
    biases: Aligned<i16>,
    /// `weights[index * L1 + j]` for the king-piece features.
    weights: Aligned<i16>,
    /// `threat_and_pp_weights[index * L1 + j]`, `i8` because these features are many and
    /// their individual contributions small.
    threat_and_pp_weights: Aligned<i8>,
    /// `psqt_weights[index * PSQT_BUCKETS + k]` for the king-piece features.
    psqt_weights: Aligned<i32>,
    /// The same, for the threat and pawn-pair features.
    threat_and_pp_psqt_weights: Aligned<i32>,
}

impl std::fmt::Debug for FeatureTransformer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureTransformer")
            .field("weight_bytes", &self.weight_bytes())
            .finish_non_exhaustive()
    }
}

/// Both perspectives' accumulated state for one position.
#[derive(Clone, Debug)]
pub struct Accumulator {
    /// `accumulation[perspective][j]`.
    pub accumulation: [Vec<i16>; COLOR_NB],
    /// `psqt[perspective][bucket]`.
    pub psqt: [[i32; PSQT_BUCKETS]; COLOR_NB],
}

impl Default for Accumulator {
    fn default() -> Accumulator {
        Accumulator {
            accumulation: [vec![0; L1], vec![0; L1]],
            psqt: [[0; PSQT_BUCKETS]; COLOR_NB],
        }
    }
}

/// The last evaluated position's accumulator and the feature sets that produced it.
///
/// One per worker. Keeping only the most recent is deliberate: in a search, consecutive
/// evaluations are a parent and its child, or two siblings, and those differ by a handful of
/// One perspective's accumulator and the feature sets that produced it.
///
/// Per PERSPECTIVE, not per position, because the two sides go stale independently: every
/// feature a perspective sees is indexed against ITS OWN king square, so White's king moving
/// invalidates everything White sees and nothing Black sees.
#[derive(Clone, Debug)]
struct Side {
    /// The king square these were computed for, or `None` when the slot holds nothing.
    king: Option<Square>,
    /// Left a `Vec` on purpose. A `Box<[i16; L1]>` gives the fold's tile loop a trip count
    /// of eight at compile time and drops the `resize` that says what the type already says
    /// — and it measured 6.1M WORSE on `bench 16 1 8` at avx2, 1,592.2M to 1,598.3M, at an
    /// identical node count. Fully unrolling a loop that was already eight tiles of
    /// register-resident work buys nothing and costs code size, which is the counter this
    /// port is already worst on. Do not re-derive it.
    acc: Vec<i16>,
    psqt: [i32; PSQT_BUCKETS],
    /// The placement that produced the above. The king-piece features are diffed straight
    /// off this rather than recomputed and merged, so the set itself is never materialised.
    board: [Piece; SQUARE_NB],
    /// The position key this was computed for, or [`EMPTY_KEY`] when it holds nothing.
    ///
    /// A ply slot is only a legal base for a roll-forward when this still matches the key
    /// the search reached that ply with — the search may have left and returned, and the
    /// slot would then hold a cousin.
    computed_for: Key,
}

impl Side {
    fn empty() -> Side {
        Side {
            king: None,
            acc: Vec::new(),
            psqt: [0; PSQT_BUCKETS],
            board: [Piece::NONE; SQUARE_NB],
            computed_for: EMPTY_KEY,
        }
    }
}

/// One refresh-cache entry: an accumulator AND the threat set that produced it.
///
/// The set lives here and nowhere else. A ply slot never carries one, because the per-move
/// delta reaches it from records; a cache entry is reached by a DIFF, which needs the other
/// side of the comparison. Confining it to the 64 slots per perspective that a refresh
/// writes is what keeps set construction off the per-node path.
#[derive(Clone, Debug)]
struct CacheSlot {
    side: Side,
    /// The active threat and pawn-pair set, in whatever order it was generated.
    ///
    /// **Unordered on purpose.** The diff is a membership test against a bitmap rather than
    /// a merge walk, so nothing downstream reads an order and sorting would buy nothing.
    threats: Vec<u32>,
}

impl CacheSlot {
    fn empty() -> CacheSlot {
        CacheSlot { side: Side::empty(), threats: Vec::new() }
    }
}

/// One ply of the accumulator stack: the state, and the move that reached it.
#[derive(Clone, Debug)]
struct PlySlot {
    /// This ply's accumulator, per perspective.
    side: [Side; COLOR_NB],
    /// The position key the search reached this ply with, written by `do_move`.
    reached: Key,
    /// The threat records of the move that reached this ply from the one below.
    dts: Vec<DirtyThreat>,
    /// The pawn placement that move changed.
    dpp: DirtyPawnPairs,
    /// Whether the two above describe the hop from the ply below.
    ///
    /// False at the root, and after any path that moved the position without recording.
    recorded: bool,
    /// The placement the search reached this ply with, stamped by `do_move` whether or not
    /// this ply is ever evaluated.
    ///
    /// A roll-forward walks a CHAIN of plies and materialises every one of them, so it needs
    /// each hop's own two placements to diff the king-piece half against. Only a computed ply
    /// carries one in its [`Side`], and the plies a chain steps through are by definition the
    /// ones that were NOT computed.
    board: [Piece; SQUARE_NB],
    /// The two king squares on that placement, so the walk-back can test the chain for a king
    /// move without scanning the board for one.
    king: [Square; COLOR_NB],
}

impl PlySlot {
    fn empty() -> PlySlot {
        PlySlot {
            side: [Side::empty(), Side::empty()],
            reached: EMPTY_KEY,
            dts: Vec::new(),
            dpp: DirtyPawnPairs::default(),
            recorded: false,
            board: [Piece::NONE; SQUARE_NB],
            king: [Square::A1; COLOR_NB],
        }
    }
}

/// Ply slots held, sized so the search can never need one that is not already there.
///
/// The search's ply is `si - STACK_BASE + 1` and its stack is `MAX_PLY + 10` deep, so this
/// bound covers every ply reachable. Sizing once removes the grow check from `record` and
/// `record_threats`, which sit on the per-node `do_move` path; an unused slot holds three
/// EMPTY `Vec`s and costs no allocation, so the whole array is a few hundred KiB of plain
/// fields per worker whether the search descends that far or not.
const PLY_SLOTS: usize = MAX_PLY + 10;

/// Scratch buffers an evaluation reuses, so a search does not allocate per node.
#[derive(Debug)]
pub struct EvalScratch {
    /// The position the live slots hold, or [`EMPTY_KEY`] when they hold nothing.
    key: Key,
    /// One accumulator per ply, upstream's `AccumulatorStack`.
    ///
    /// A single slot cannot serve a delta: after a subtree returns it holds a COUSIN, and no
    /// chain of recorded moves connects a cousin to the current position. Measured at an 11%
    /// one-hop rate. A slot per ply makes the parent always available, which is what turns
    /// the delta from "sometimes applicable" into the steady state.
    ///
    /// Grown on demand, so a worker that never evaluates does not pay for it.
    /// A boxed ARRAY, not a `Vec`: `PLY_SLOTS` is a constant, so every `plies[ply]` bounds
    /// check compares against an immediate rather than loading a length. The same change on
    /// the search stack was worth 16.0M instructions on a bench.
    plies: Box<[PlySlot; PLY_SLOTS]>,
    /// One slot per king square per perspective — upstream's accumulator refresh cache.
    ///
    /// A king move re-indexes EVERY feature that perspective sees, both orientations being
    /// keyed off the king square, so the live slot after one is worth nothing and the diff
    /// against it is the whole set. What this holds is the last accumulator computed for
    /// each king square, which is a far closer starting point than the biases: the diff
    /// against it is only the pieces that moved since, not all of them.
    ///
    /// Grown to 64 slots on first use rather than allocated up front, because a worker that
    /// never evaluates should not pay for it.
    cache: [Vec<CacheSlot>; COLOR_NB],
    /// Freshly computed sets, reused between calls to avoid reallocating.
    ///
    /// Written ONLY on the refresh path. The delta path never builds a set, which is the
    /// whole of what this architecture buys — see [`FeatureTransformer::transform`].
    next_threats: [Vec<u32>; COLOR_NB],
    /// The king-piece features one diff or one delta adds and removes, per perspective.
    adds: [Vec<u32>; COLOR_NB],
    subs: [Vec<u32>; COLOR_NB],
    /// The same for the threat and pawn-pair features, collected SEPARATELY so that both
    /// kinds can be folded in one sweep of the accumulator: they read different weight
    /// tables, so they cannot share a list, but they can share the pass.
    tp_adds: [Vec<u32>; COLOR_NB],
    tp_subs: [Vec<u32>; COLOR_NB],
    /// The transformed features this evaluation produced, reused across evaluations.
    ///
    /// Held here rather than allocated in `Network::evaluate` because that is the per-node
    /// path: a `vec![0u8; L1]` there is a malloc, a 1 KiB zero-fill and a free per
    /// evaluation, and `transform` overwrites every one of those bytes before anything
    /// reads them. Aligned because it is the input side of the affine layer's vector loads,
    /// which is what upstream's `alignas(CacheLineSize)` on the same buffer is for.
    transformed: Aligned<u8>,
    /// One bit per threat-and-pawn-pair feature, used to diff two sets by MEMBERSHIP.
    ///
    /// Every pass leaves it all-zero, so it never has to be cleared in bulk: the walk that
    /// reads a bit is the walk that clears it. Grown on first use, like the refresh cache,
    /// so a worker that never evaluates does not pay for it.
    mark: Vec<u64>,
}

/// How many hops a roll-forward will walk back through.
///
/// **The cap stopped being the load-bearing constant when the roll started stamping every
/// ply it steps through.** While the chain was concatenated into one fold it had to be tiny:
/// records do not cancel before they are applied, so a piece that moved twice contributed
/// four rows rather than two, and a longer chain bought nothing for the next evaluation
/// because it wrote only its last ply. Search instructions at avx2 over the same 163,081
/// nodes, with that fold: 2,174M at one hop, 2,025M at two, 2,026M at three, 2,044M at four,
/// 2,131M at five, 2,232M at eight.
///
/// Hop by hop the curve runs the other way, on the same workload: 1,682M at two, 1,628M at
/// four, 1,603M at eight, **1,601M at twelve**, and 1,601M at sixteen, thirty-two and
/// sixty-four. It is a plateau rather than a peak — twelve is simply where the chains this
/// search actually walks run out, so a larger cap describes the same walk.
///
/// The cap is now a BOUND on the worst case rather than a tuning knob: the measured average
/// is 1.32 hops per roll-forward over 101,639 of them, because a chain walked once leaves
/// every ply it covered ready for the next evaluation to reach in one.
const HOP_CAP: usize = 12;

/// How many `u64` words cover the threat-and-pawn-pair feature space.
const MARK_WORDS: usize = THREAT_AND_PP_DIMENSIONS.div_ceil(64);

/// The key of a slot no evaluation has filled yet. No real position hashes to it.
const EMPTY_KEY: Key = Key::MAX;

impl Default for EvalScratch {
    fn default() -> EvalScratch {
        EvalScratch {
            key: EMPTY_KEY,
            plies: match vec![PlySlot::empty(); PLY_SLOTS].into_boxed_slice().try_into() {
                Ok(b) => b,
                // Built with exactly PLY_SLOTS elements, so this cannot fail. A match rather
                // than `expect`, because the error type is the boxed slice itself.
                Err(_) => unreachable!("the vec was built with exactly PLY_SLOTS items"),
            },
            cache: [Vec::new(), Vec::new()],
            next_threats: [Vec::new(), Vec::new()],
            adds: [Vec::new(), Vec::new()],
            subs: [Vec::new(), Vec::new()],
            tp_adds: [Vec::new(), Vec::new()],
            tp_subs: [Vec::new(), Vec::new()],
            mark: Vec::new(),
            transformed: Aligned::new(L1),
        }
    }
}

impl EvalScratch {
    /// The transformed features the last [`FeatureTransformer::transform`] wrote.
    #[must_use]
    pub fn transformed(&self) -> &[u8] {
        &self.transformed
    }

    /// Forget every cached accumulator.
    ///
    /// Not needed for correctness — a diff against any position is still exact — but a
    /// caller that has jumped somewhere unrelated saves the work of a large diff.
    pub fn reset(&mut self) {
        self.key = EMPTY_KEY;
        for i in 0..COLOR_NB {
            self.cache[i].clear();
        }
        self.invalidate_plies();
    }

    /// Mark every ply slot as holding nothing, keeping its buffers.
    ///
    /// Invalidated IN PLACE, never by `clear()`. A slot owns three heap buffers — an
    /// accumulator per perspective and a threat-record list — so clearing drops them all and
    /// the next descent re-allocates every one on its way down. It is also what sizes the
    /// vector, and `PLY_SLOTS` is the promise that every ply the search can reach is already
    /// there: emptying it breaks that promise as well as the allocations.
    ///
    /// Resetting the validity fields is exactly as strong, because it leaves each slot saying
    /// precisely what `PlySlot::empty` says: no key, no king, nothing recorded.
    fn invalidate_plies(&mut self) {
        for slot in &mut self.plies {
            slot.reached = EMPTY_KEY;
            slot.recorded = false;
            slot.dts.clear();
            slot.dpp = DirtyPawnPairs::default();
            for side in &mut slot.side {
                side.computed_for = EMPTY_KEY;
                side.king = None;
            }
        }
    }

    /// Drop the ply stack, keeping the refresh cache.
    ///
    /// Every ply from one is stamped by the `do_move` that reached it, so a slot the search
    /// has left cannot be mistaken for one it stands on. Ply ZERO has no such stamp — it is
    /// written only by the evaluation itself — so a root that changes without ply zero being
    /// re-evaluated would leave a slot that agrees with itself about the wrong position.
    /// Invalidating every slot per search is what closes that.
    ///
    /// Invalidated IN PLACE, not by `clear()`. A slot owns three heap buffers — an
    /// accumulator per perspective and a threat-record list — so clearing drops them all and
    /// the next descent re-allocates every one on its way down. Resetting the validity
    /// fields is exactly as strong, because it leaves each slot saying precisely what
    /// `PlySlot::empty` says: no key, no king, nothing recorded.
    ///
    /// The refresh cache survives on purpose: its entries are keyed by king square and are
    /// exact for any position that reaches them, so a new search inherits them.
    pub fn new_search(&mut self) {
        self.invalidate_plies();
        self.key = EMPTY_KEY;
    }

    /// Record the hop that reached `ply`, so the accumulator there can roll forward.
    ///
    /// Called from the search's own `do_move`. The records are consumed by
    /// [`FeatureTransformer::transform`] at that ply and at any ply below it that walks back
    /// through this one.
    pub fn record(&mut self, ply: usize, pos: &Position, key: Key, dpp: DirtyPawnPairs) {
        let slot = &mut self.plies[ply];
        slot.reached = key;
        slot.dpp = dpp;
        slot.recorded = true;
        slot.board = *pos.board();
        slot.king = [pos.king_square(Color::White), pos.king_square(Color::Black)];
    }

    /// The list [`EvalScratch::record`] appends this hop's threat records to.
    ///
    /// Handed out before the move is made, because `do_move_recording` fills it as it moves
    /// the pieces. Cleared here so the caller never has to remember to.
    pub fn record_threats(&mut self, ply: usize) -> &mut Vec<DirtyThreat> {
        self.plies[ply].dts.clear();
        &mut self.plies[ply].dts
    }

    /// Mark the hop that reached `ply` as one no records describe.
    ///
    /// A null move needs none — it moves no piece, so no threat and no pawn pair changes —
    /// but anything else that advances a ply without recording must say so, or the roll
    /// forward would skip a move.
    pub fn record_null(&mut self, ply: usize, pos: &Position, key: Key) {
        let slot = &mut self.plies[ply];
        slot.reached = key;
        slot.dts.clear();
        slot.dpp = DirtyPawnPairs::default();
        slot.recorded = true;
        slot.board = *pos.board();
        slot.king = [pos.king_square(Color::White), pos.king_square(Color::Black)];
    }
}

/// Accumulator entries the fold carries at once.
///
/// Measured rather than reasoned, and measured TWICE. Too small and the per-tile sweep
/// overhead — the copy in and the copy out — is paid too many times; too large and the
/// running values no longer stay in registers across the row loop.
///
/// 32, 64, 128 and 256 at **nehalem**, whole-run search instructions on `bench 16 1 8`:
/// 4,093M / 3,673M / 3,598M / 4,037M.
///
/// Re-swept at **avx2** after the fold was rewritten twice — once for the combined no-copy
/// sweep and once to index the weight rows instead of zipping range slices — because that
/// is the register and traffic context the earlier verdict was taken in, and zfish's own
/// tile knob only landed on its third look after exactly that kind of change. 64 / 128 /
/// 256 measured 1,947M / 1,751M / 1,768M. **128 is the peak on both tiers**, so the value
/// travels further than the original note assumed — but re-measure rather than assume it,
/// which is what turned this comment from one sweep into two.
const TILE: usize = 128;

/// [`FT_MAX_VAL`] at the accumulator's own width.
const FT_MAX: i16 = FT_MAX_VAL as i16;

impl FeatureTransformer {
    /// An unloaded transformer with every weight zero.
    #[must_use]
    pub fn new() -> FeatureTransformer {
        FeatureTransformer {
            biases: Aligned::new(L1),
            weights: Aligned::new(L1 * HALFKA_DIMENSIONS),
            threat_and_pp_weights: Aligned::new(L1 * THREAT_AND_PP_DIMENSIONS),
            psqt_weights: Aligned::new(PSQT_BUCKETS * HALFKA_DIMENSIONS),
            threat_and_pp_psqt_weights: Aligned::new(PSQT_BUCKETS * THREAT_AND_PP_DIMENSIONS),
        }
    }

    /// How many bytes of weights this transformer holds.
    #[must_use]
    pub fn weight_bytes(&self) -> usize {
        self.biases.len() * 2
            + self.weights.len() * 2
            + self.threat_and_pp_weights.len()
            + self.psqt_weights.len() * 4
            + self.threat_and_pp_psqt_weights.len() * 4
    }

    /// Read the transformer's parameters.
    ///
    /// **The order is the file's, and it is not negotiable.** Biases, then the threat
    /// weights and their PSQT block, then the pawn-pair weights and their PSQT block, then
    /// the king-piece weights and their PSQT block. The two encodings alternate: the `i8`
    /// blocks are stored raw and the rest LEB128-compressed, because a byte-wide weight
    /// gains nothing from a variable-length encoding.
    pub fn read(&mut self, r: &mut NetReader<impl std::io::Read>) -> Result<(), NetError> {
        use super::features::{PP_DIMENSIONS, THREAT_DIMENSIONS};
        let threat_dims = THREAT_DIMENSIONS as usize;
        let pp_dims = PP_DIMENSIONS as usize;

        r.leb128_i16(&mut self.biases)?;

        let (threat_w, pp_w) = self.threat_and_pp_weights.split_at_mut(threat_dims * L1);
        r.i8s(threat_w)?;
        let (threat_psqt, pp_psqt) =
            self.threat_and_pp_psqt_weights.split_at_mut(threat_dims * PSQT_BUCKETS);
        r.leb128(threat_psqt)?;
        r.i8s(&mut pp_w[..pp_dims * L1])?;
        r.leb128(&mut pp_psqt[..pp_dims * PSQT_BUCKETS])?;

        r.leb128_i16(&mut self.weights)?;
        r.leb128(&mut self.psqt_weights)?;

        // `permute_weights` is skipped on purpose: it exists only so a vector `packus` can
        // read adjacent lanes in order, and the permutation is the identity when no vector
        // path is compiled. rfish has none, so applying it and its inverse would cancel.
        Ok(())
    }

    /// Write the transformer back in the form [`FeatureTransformer::read`] expects.
    ///
    /// Same order, same encodings, same split points. `permute_weights` is skipped on the
    /// way out for the same reason it is skipped on the way in: with no vector path
    /// compiled it is the identity.
    pub fn write(&self, w: &mut NetWriter<impl std::io::Write>) -> Result<(), NetError> {
        use super::features::{PP_DIMENSIONS, THREAT_DIMENSIONS};
        let threat_dims = THREAT_DIMENSIONS as usize;
        let pp_dims = PP_DIMENSIONS as usize;

        w.leb128_i16(&self.biases)?;

        let (threat_w, pp_w) = self.threat_and_pp_weights.split_at(threat_dims * L1);
        w.i8s(threat_w)?;
        let (threat_psqt, pp_psqt) =
            self.threat_and_pp_psqt_weights.split_at(threat_dims * PSQT_BUCKETS);
        w.leb128(threat_psqt)?;
        w.i8s(&pp_w[..pp_dims * L1])?;
        w.leb128(&pp_psqt[..pp_dims * PSQT_BUCKETS])?;

        w.leb128_i16(&self.weights)?;
        w.leb128(&self.psqt_weights)?;
        Ok(())
    }

    /// Diff two sets by MEMBERSHIP, applying only what differs.
    ///
    /// Neither set is ordered. A feature index encodes exactly one (attacker, from, to,
    /// attacked) or pawn-pair tuple, so a set holds no duplicates, and for duplicate-free
    /// sets "is this index in the other one" is a complete and exact diff — which a bitmap
    /// answers in constant time per feature and a merge walk answers only after both sides
    /// have been SORTED.
    ///
    /// Sorting was what the old merge walk cost: the sets run to a few dozen features and
    /// the comparison sort over them was among the largest single symbols in the profile,
    /// against an upstream that sorts nothing at all. Three linear passes replace it —
    /// mark the old set, walk the new set clearing what both hold and collecting what only
    /// the new one has, then walk the old set again collecting what is still marked. The
    /// third pass is what returns the bitmap to all-zero, so there is no clearing sweep
    /// over the whole feature space and the next call inherits a clean one.
    ///
    /// The walk only COLLECTS what changed; [`FeatureTransformer::fold_rows_i16`] and its
    /// byte-wide twin then apply the whole collection in one sweep of the accumulator. Doing
    /// it a feature at a time meant reading and writing all 1024 entries once per changed
    /// feature, which is the same weight traffic and several times the accumulator traffic.
    /// Upstream folds its add and subtract lists in one pass for the same reason.
    ///
    /// Order does not matter to the result: the accumulator is wrapping `i16` and the PSQT
    /// head is `i32`, and both are associative and commutative under the additions applied
    /// here, so collecting first changes nothing about the value.
    fn collect_diff(
        old: &[u32],
        new: &[u32],
        adds: &mut Vec<u32>,
        subs: &mut Vec<u32>,
        mark: &mut [u64],
    ) {
        adds.clear();
        subs.clear();

        // Every index is bounded by the feature space it is drawn from, so the bitmap needs
        // no bounds arithmetic beyond the word split.
        debug_assert!(old.iter().chain(new).all(|&f| (f as usize) < THREAT_AND_PP_DIMENSIONS));

        for &f in old {
            let (w, b) = (f as usize / 64, 1u64 << (f % 64));
            mark[w] |= b;
        }
        // A hit means both sets hold it and the accumulator already has it; clearing the bit
        // as it is read leaves exactly the old-only features standing for the pass below.
        for &f in new {
            let (w, b) = (f as usize / 64, 1u64 << (f % 64));
            if mark[w] & b == 0 {
                adds.push(f);
            } else {
                mark[w] &= !b;
            }
        }
        for &f in old {
            let (w, b) = (f as usize / 64, 1u64 << (f % 64));
            if mark[w] & b != 0 {
                subs.push(f);
                mark[w] &= !b;
            }
        }
    }

    /// Fold both feature kinds from `src` into `dst`, in ONE sweep, without a copy.
    ///
    /// Three things upstream's `update_accumulator_incremental` does that applying the two
    /// kinds separately does not:
    ///
    /// - the source is READ rather than copied. Seeding `dst` from `src` and then folding in
    ///   place moves 1024 entries that the fold is about to rewrite anyway;
    /// - the accumulator is swept ONCE rather than twice. Each kind reads its own weight
    ///   table, so the lists cannot merge, but the tile they land in can be held across both;
    /// - the tile stays in registers for the whole of it, which is the reason [`TILE`] exists.
    fn fold_into(&self, src: &[i16], dst: &mut [i16], ka: (&[u32], &[u32]), tp: (&[u32], &[u32])) {
        // Both weight tables viewed as TILE-wide ROWS, once for the whole call. `base` was
        // `index * L1 + off` and the row was taken with a range slice, so every one of the
        // four inner loops paid a range bounds test and then walked a `zip` iterator: 94.5M
        // instructions of `slice::iter` and `ptr::non_null` machinery sat inside `transform`
        // on that alone. `L1` is a whole number of tiles, so a row is just
        // `index * ROWS_PER_FEATURE + t`, and indexing two `[i16; TILE]` arrays by
        // `0..TILE` needs no check at either end.
        const ROWS_PER_FEATURE: usize = L1 / TILE;
        let ka_rows = <[i16]>::as_chunks::<TILE>(&self.weights).0;
        let tp_rows = <[i8]>::as_chunks::<TILE>(&self.threat_and_pp_weights).0;
        let src_rows = src.as_chunks::<TILE>().0;
        for (t, chunk) in dst.as_chunks_mut::<TILE>().0.iter_mut().enumerate() {
            let mut tile = src_rows[t];
            for &index in ka.1 {
                let w = &ka_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    // Wrapping is upstream's behaviour: the accumulator is `i16` and the
                    // trainer keeps the sum in range, so a wrap here means a corrupt net
                    // rather than a value to saturate.
                    tile[j] = tile[j].wrapping_sub(w[j]);
                }
            }
            for &index in ka.0 {
                let w = &ka_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_add(w[j]);
                }
            }
            for &index in tp.1 {
                let w = &tp_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_sub(i16::from(w[j]));
                }
            }
            for &index in tp.0 {
                let w = &tp_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_add(i16::from(w[j]));
                }
            }
            *chunk = tile;
        }
    }

    /// Fold in place, and MIRROR each finished tile to a second destination.
    ///
    /// The refresh path has two consumers for the same 1024 values: the ply slot the search
    /// reads, and the cache slot the next refresh for this king square starts from. Folding
    /// into one and then copying to the other re-reads and re-writes the whole 2 KiB row that
    /// was just written; mirroring the tile while it is still in registers writes each entry
    /// to both places once and reads it back never.
    ///
    /// `acc` is the cache slot's own row, so it is both the source and the first destination
    /// — a refresh's base is by definition the entry it is about to replace.
    fn fold_mirror(
        &self,
        acc: &mut [i16],
        mirror: &mut [i16],
        ka: (&[u32], &[u32]),
        tp: (&[u32], &[u32]),
    ) {
        // Both weight tables as TILE-wide ROWS, exactly as [`FeatureTransformer::fold_into`]
        // takes them: `index * ROWS_PER_FEATURE + t` indexes a `[_; TILE]` array, where a
        // `base..base + TILE` range slice costs a bounds test per row and then walks a `zip`
        // iterator over it. That shape was worth 19.3M on the delta path; the refresh path is
        // the other half of the same fold and had been left behind, and it is worth 28.9M
        // here because a refresh applies MORE rows than a one-hop delta does.
        const ROWS_PER_FEATURE: usize = L1 / TILE;
        let ka_rows = <[i16]>::as_chunks::<TILE>(&self.weights).0;
        let tp_rows = <[i8]>::as_chunks::<TILE>(&self.threat_and_pp_weights).0;
        let mirror_tiles = mirror.as_chunks_mut::<TILE>().0;
        for (t, chunk) in acc.as_chunks_mut::<TILE>().0.iter_mut().enumerate() {
            let mut tile = *chunk;
            for &index in ka.1 {
                let w = &ka_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    // Wrapping is upstream's behaviour: the accumulator is `i16` and the
                    // trainer keeps the sum in range, so a wrap here means a corrupt net
                    // rather than a value to saturate.
                    tile[j] = tile[j].wrapping_sub(w[j]);
                }
            }
            for &index in ka.0 {
                let w = &ka_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_add(w[j]);
                }
            }
            for &index in tp.1 {
                let w = &tp_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_sub(i16::from(w[j]));
                }
            }
            for &index in tp.0 {
                let w = &tp_rows[index as usize * ROWS_PER_FEATURE + t];
                for j in 0..TILE {
                    tile[j] = tile[j].wrapping_add(i16::from(w[j]));
                }
            }
            *chunk = tile;
            if let Some(m) = mirror_tiles.get_mut(t) {
                *m = tile;
            }
        }
    }

    /// The PSQT head's half of [`FeatureTransformer::fold_into`].
    ///
    /// Separate because it is eight values against 1024 and shares nothing with the sweep
    /// above — folding it inside would put a second, differently shaped loop in the hot tile.
    fn fold_psqt(
        &self,
        psqt: &mut [i32; PSQT_BUCKETS],
        ka: (&[u32], &[u32]),
        tp: (&[u32], &[u32]),
    ) {
        // The same shape as `fold_into`: both tables viewed as PSQT_BUCKETS-wide rows once,
        // so a feature's row is `weights[index]` rather than a range slice, and the eight
        // accumulations are an indexed loop over two fixed-size arrays rather than a `zip`.
        // On a kernel that adds eight `i32`s, that machinery was 13.7M of the 29.1M.
        //
        // The head itself is held in a LOCAL for the whole call, so it is loaded and stored
        // once rather than once per feature -- `psqt` is behind a reference the compiler
        // must assume the weight reads could alias.
        // The head is EIGHT `i32`, which is one AVX2 register exactly -- and the indexed
        // loop below it did not vectorise. Disassembled, this function held 33 `mov`s and
        // not one vector instruction: LLVM will not turn an eight-lane integer loop into a
        // `vpaddd` on its own, and ../mcfish records hitting the same wall on the same
        // kernel (`nnue_acc_apply_psqt_delta`, "the scalar 8-step loop these replaced stayed
        // scalar"). Say it in vectors instead, which needs no `unsafe`.
        //
        // Bit-identical: the same eight integer additions in the same per-row order. `Simd`
        // arithmetic WRAPS where the scalar `-=` would trap under the gate profile's
        // overflow checks, so this drops a check the head cannot fail -- upstream carries
        // the same accumulation in `std::int32_t` and relies on the trainer to keep it in
        // range, exactly as the `i16` half above does.
        let ka_rows = <[i32]>::as_chunks::<PSQT_BUCKETS>(&self.psqt_weights).0;
        let tp_rows = <[i32]>::as_chunks::<PSQT_BUCKETS>(&self.threat_and_pp_psqt_weights).0;
        let mut acc = Simd::<i32, PSQT_BUCKETS>::from_array(*psqt);
        for (rows, (adds, subs)) in [(ka_rows, ka), (tp_rows, tp)] {
            for &index in subs {
                acc -= Simd::from_array(rows[index as usize]);
            }
            for &index in adds {
                acc += Simd::from_array(rows[index as usize]);
            }
        }
        *psqt = acc.to_array();
    }

    /// The active feature sets, one per perspective, in generation order.
    ///
    /// Deliberately NOT sorted: [`FeatureTransformer::diff_apply`] tests membership rather
    /// than merging, so an order would cost a comparison sort per evaluation and buy
    /// nothing. Upstream sorts nothing here either.
    fn active_sets(pos: &Position, wanted: [bool; COLOR_NB], threats: &mut [Vec<u32>; COLOR_NB]) {
        for (t, w) in threats.iter_mut().zip(wanted) {
            if w {
                t.clear();
            }
        }
        threat_active(pos, wanted, threats);
        pawn_pair_active(pos, wanted, threats);
    }

    /// The nearest ancestor ply whose accumulator this one can roll forward from.
    ///
    /// Three conditions, and all three are cheap. The base must have been COMPUTED for the
    /// position the search actually reached that ply with — the search leaves and returns,
    /// and a slot it left behind holds a cousin. It must have been computed against the SAME
    /// king square, because every index a perspective sees is keyed off it. And every hop
    /// between must be recorded, or the chain has a gap no records describe.
    ///
    /// The king squares BETWEEN do not matter. A record is a geometric tuple, not an index,
    /// so the whole chain is indexed against the current square when it is applied.
    fn rollforward_base(scratch: &EvalScratch, p: Color, ply: usize, ksq: Square) -> Option<usize> {
        let i = p.index();
        let mut at = ply;
        loop {
            let slot = &scratch.plies[at];
            // Every ply the chain covers is MATERIALISED against `ksq`, so the king square
            // has to be `ksq` at every one of them and not only at the base. Concatenating
            // the chain into one fold did not need this — nothing between was written, so
            // nothing between had to be a valid accumulator — but stamping the intermediates
            // is the whole point here, and one stamped against a king square the position
            // did not have would be read as valid by the next evaluation.
            //
            // It costs almost nothing: a king moves rarely, and the base has always had to
            // match anyway.
            if slot.king[i] != ksq {
                return None;
            }
            let side = &slot.side[i];
            if side.computed_for != EMPTY_KEY
                && side.computed_for == slot.reached
                && side.king == Some(ksq)
            {
                return Some(at);
            }
            // Step below only through a hop whose records exist.
            if at == 0 || !slot.recorded || ply - at >= HOP_CAP {
                return None;
            }
            at -= 1;
        }
    }

    /// Roll `base`'s accumulator forward to this position, ONE HOP AT A TIME.
    ///
    /// **No feature set is built here, and that is the point.** The king-piece half comes
    /// from a board diff; the threat and pawn-pair half comes from the hops' own records,
    /// where it used to come from rebuilding both sets and diffing them.
    ///
    /// **Every ply the chain covers is written, not just the last one.** That is upstream's
    /// `AccumulatorStack::evaluate` shape and it is what makes the walk-back pay for itself:
    /// a materialised intermediate is a base the NEXT evaluation can reach in a single hop,
    /// so the chain a subtree walks once it does not walk again. Concatenating every hop's
    /// records into one fold looks cheaper per traversal — one sweep of the accumulator
    /// rather than one per hop — and is dearer in total, because it leaves every ply it
    /// steps over exactly as stale as it found it AND because records do not cancel before
    /// they are applied, so a piece that moved twice contributes four rows rather than two.
    /// Measured: with the whole chain concatenated, an uncapped walk folds 28.1 rows per
    /// roll-forward against a refresh's 17.3, which is why the cap it needed was two.
    fn roll_forward(
        &self,
        pos: &Position,
        p: Color,
        ply: usize,
        base: usize,
        ksq: Square,
        scratch: &mut EvalScratch,
    ) {
        let i = p.index();
        let _ = pos;
        for hop in base + 1..=ply {
            scratch.adds[i].clear();
            scratch.subs[i].clear();
            scratch.tp_adds[i].clear();
            scratch.tp_subs[i].clear();

            // Split the borrow by FIELD -- the stack is read while the collection buffers
            // are written, and the two are distinct fields of the same scratch.
            let (plies, adds, subs) = (&scratch.plies, &mut scratch.adds[i], &mut scratch.subs[i]);
            let (from, to) = (&plies[hop - 1].board, &plies[hop].board);
            halfka_delta(from, to, p, ksq, adds, subs);
            let (plies, adds, subs) =
                (&scratch.plies, &mut scratch.tp_adds[i], &mut scratch.tp_subs[i]);
            threat_delta(&plies[hop].dts, p, ksq, adds, subs);
            pawn_pair_delta(&plies[hop].dpp, p, ksq, adds, subs);

            // Read one ply and write the next, both of them elements of the same stack:
            // split it so the fold reads the source and writes the destination in one pass.
            let (plies, adds, subs, tp_adds, tp_subs) = (
                &mut scratch.plies,
                &scratch.adds[i],
                &scratch.subs[i],
                &scratch.tp_adds[i],
                &scratch.tp_subs[i],
            );
            let (below, at) = plies.split_at_mut(hop);
            let src = &below[hop - 1].side[i];
            // Split the destination ply by FIELD too: its accumulator is written while its
            // own placement and arrival key are read.
            let PlySlot { side, board, reached, .. } = &mut at[0];
            let dst = &mut side[i];
            dst.acc.resize(L1, 0);
            dst.psqt = src.psqt;
            dst.king = Some(ksq);
            dst.board = *board;
            let ka = (&adds[..], &subs[..]);
            let tp = (&tp_adds[..], &tp_subs[..]);
            self.fold_psqt(&mut dst.psqt, ka, tp);
            self.fold_into(&src.acc, &mut dst.acc, ka, tp);
            // STAMP the hop. This is the whole difference from concatenating the chain into
            // one fold: an intermediate ply that has been materialised is a base the next
            // evaluation can roll a single hop from, where a chain that only ever wrote its
            // last ply left every ply it stepped over exactly as stale as it found them.
            dst.computed_for = *reached;
        }
    }

    /// Rebuild this perspective's accumulator against the refresh cache.
    ///
    /// The ONLY path that materialises a feature set, and the only one that needs to: a
    /// cache entry is reached by a DIFF rather than by records, so the other side of the
    /// comparison has to exist. Taken when the king has moved, when the search has jumped,
    /// or when the nearest computed ancestor is further than [`HOP_CAP`].
    fn refresh(
        &self,
        pos: &Position,
        p: Color,
        ply: usize,
        ksq: Square,
        scratch: &mut EvalScratch,
    ) {
        let i = p.index();
        if scratch.cache[i].is_empty() {
            scratch.cache[i].resize(SQUARE_NB, CacheSlot::empty());
        }
        scratch.adds[i].clear();
        scratch.subs[i].clear();
        scratch.tp_adds[i].clear();
        scratch.tp_subs[i].clear();

        // With no entry for this king square the base is the biases and an empty active set,
        // so the "diff" is every feature -- which is exactly the from-scratch computation.
        // One code path serves both, so there is no second implementation to disagree.
        let src = &scratch.cache[i][ksq.index()];
        let seeded = src.side.king.is_some();
        let base_board = if seeded { src.side.board } else { [Piece::NONE; SQUARE_NB] };
        let base_psqt = if seeded { src.side.psqt } else { [0; PSQT_BUCKETS] };

        let (plies, adds, subs) = (&scratch.plies, &mut scratch.adds[i], &mut scratch.subs[i]);
        let _ = plies;
        halfka_delta(&base_board, pos.board(), p, ksq, adds, subs);
        let old: &[u32] = if seeded { &scratch.cache[i][ksq.index()].threats } else { &[] };
        Self::collect_diff(
            old,
            &scratch.next_threats[i],
            &mut scratch.tp_adds[i],
            &mut scratch.tp_subs[i],
            &mut scratch.mark,
        );

        // The fold runs IN the cache slot and mirrors each tile out to the ply slot, so this
        // king square's entry becomes what was just computed without a second sweep. Seed the
        // slot first: an unseeded one starts from the biases, which is the from-scratch case.
        let cache_slot = &mut scratch.cache[i][ksq.index()];
        if !seeded {
            cache_slot.side.acc.clear();
            cache_slot.side.acc.extend_from_slice(&self.biases);
        }
        cache_slot.side.psqt = base_psqt;
        cache_slot.side.king = Some(ksq);
        cache_slot.side.board = *pos.board();

        // Split the borrow by FIELD: the collected lists are read while the cache and the ply
        // slot are written, and no two of them are the same field.
        let (plies, cache, adds, subs, tp_adds, tp_subs) = (
            &mut scratch.plies,
            &mut scratch.cache[i],
            &scratch.adds[i],
            &scratch.subs[i],
            &scratch.tp_adds[i],
            &scratch.tp_subs[i],
        );
        let ka = (&adds[..], &subs[..]);
        let tp = (&tp_adds[..], &tp_subs[..]);
        let slot = &mut cache[ksq.index()];
        self.fold_psqt(&mut slot.side.psqt, ka, tp);
        let dst = &mut plies[ply].side[i];
        dst.acc.resize(L1, 0);
        dst.psqt = slot.side.psqt;
        dst.king = Some(ksq);
        dst.board = *pos.board();
        self.fold_mirror(&mut slot.side.acc, &mut dst.acc, ka, tp);

        // The set lives only here, so it never touches the per-node path.
        slot.threats.clear();
        slot.threats.extend_from_slice(&scratch.next_threats[i]);
    }

    /// Fill the scratch's transformed features and return the PSQT score.
    ///
    /// The output is a pairwise product, not an activation: the 1024 accumulated values are
    /// split in half and multiplied element-wise, which is what gives the first hidden layer
    /// a quadratic term without a second matrix. Both halves are clamped to `[0, 255]`
    /// first, so the product fits `u8` after the shift.
    pub fn transform(
        &self,
        pos: &Position,
        bucket: usize,
        ply: usize,
        scratch: &mut EvalScratch,
    ) -> i32 {
        // The RAW key, not the table key: the accumulator depends on the pieces alone, and
        // mixing the halfmove clock in would miss the cache every time the clock ticked
        // past fourteen without a single feature having changed.
        let key = pos.raw_key();

        // The slot may already hold this position -- a quiescence stand-pat after a main
        // search evaluation at the same ply, say -- and then there is nothing to do. Asked
        // of the SLOT rather than of a "last key seen", so that it is also the answer to
        // "can the tail read this ply", which a last-key test is not.
        let ready = ply < scratch.plies.len()
            && Color::ALL.iter().all(|p| scratch.plies[ply].side[p.index()].computed_for == key);
        if !ready {
            if scratch.mark.is_empty() {
                scratch.mark = vec![0; MARK_WORDS];
            }
            let ksq = [pos.king_square(Color::White), pos.king_square(Color::Black)];
            // The root stamps its own arrival; every other ply's is written by `do_move`.
            // The PLACEMENT goes with it: a roll-forward diffs each hop's two boards, and
            // the root is the one ply no `do_move` reaches.
            if ply == 0 {
                scratch.plies[0].reached = key;
                scratch.plies[0].recorded = false;
                scratch.plies[0].board = *pos.board();
                scratch.plies[0].king = ksq;
            }

            // Decide BOTH perspectives before doing either. `active_sets` fills the sets
            // for both at once, so asking for it inside a per-perspective refresh built
            // every set twice when both refreshed, and built the other perspective's for
            // nothing when only one did.
            let bases = [0, 1].map(|i| Self::rollforward_base(scratch, Color::ALL[i], ply, ksq[i]));
            // Only the perspectives that will actually REFRESH need a set. A king move
            // invalidates one side and leaves the other rolling forward, and a king move is
            // what 99.3% of refreshes now are.
            let wanted = [bases[0].is_none(), bases[1].is_none()];
            if wanted[0] || wanted[1] {
                Self::active_sets(pos, wanted, &mut scratch.next_threats);
            }

            for p in Color::ALL {
                let i = p.index();
                match bases[i] {
                    // Already this position, for this perspective: the other one had to be
                    // recomputed, this one did not.
                    Some(base) if base == ply => {}
                    Some(base) => self.roll_forward(pos, p, ply, base, ksq[i], scratch),
                    None => self.refresh(pos, p, ply, ksq[i], scratch),
                }
                scratch.plies[ply].side[i].computed_for = key;
            }
            scratch.plies[ply].reached = key;
            scratch.key = key;
        }

        let us = pos.side_to_move();
        let perspectives = [us, !us];
        let psqt = (scratch.plies[ply].side[perspectives[0].index()].psqt[bucket]
            - scratch.plies[ply].side[perspectives[1].index()].psqt[bucket])
            / 2;

        let half = L1 / 2;
        // Both operands are clamped into [0, 255], so their product cannot exceed 65,025 and
        // the whole pairwise step fits in `u16` -- twice the lanes per register that the
        // `i32` form allowed, for identical values. The `/ 512` is a shift for the same
        // reason: the product is never negative, so there is no rounding direction to get
        // wrong.
        // Split the borrow by FIELD: the accumulators are read while the output is written,
        // and the two are distinct fields of the same scratch.
        let (live, output) = (&scratch.plies[ply].side, scratch.transformed.as_mut_slice());
        for (p, side) in perspectives.iter().enumerate() {
            let (lo, hi) = live[side.index()].acc.split_at(half);
            let out = &mut output[half * p..half * (p + 1)];
            for ((o, &a), &b) in out.iter_mut().zip(lo.iter()).zip(hi.iter()) {
                let sum0 = a.clamp(0, FT_MAX) as u16;
                let sum1 = b.clamp(0, FT_MAX) as u16;
                *o = ((sum0 * sum1) >> 9) as u8;
            }
        }
        psqt
    }
}

impl Default for FeatureTransformer {
    fn default() -> FeatureTransformer {
        FeatureTransformer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::position::START_FEN;

    #[test]
    fn an_unloaded_transformer_has_the_right_shape() {
        let ft = FeatureTransformer::new();
        assert_eq!(ft.biases.len(), L1);
        assert_eq!(ft.weights.len(), L1 * HALFKA_DIMENSIONS);
        assert_eq!(ft.psqt_weights.len(), PSQT_BUCKETS * HALFKA_DIMENSIONS);
        assert_eq!(ft.threat_and_pp_weights.len(), L1 * THREAT_AND_PP_DIMENSIONS);
        // Around 112 MiB: worth knowing, and worth failing on if a dimension moves.
        assert!(ft.weight_bytes() > 100 << 20);
        assert!(ft.weight_bytes() < 130 << 20);
    }

    /// With every weight zero the accumulator is the biases, and the pairwise product of
    /// two zeros is zero. This is the shape test that does not need a 100 MiB file.
    /// The diff must reach the same accumulator a from-scratch pass would. Feeding a
    /// sequence of unrelated positions through ONE scratch and comparing each against a
    /// fresh scratch is what proves it — a diff that drifts would show up on the second
    /// position, not the first.
    /// Rolling a line forward from records reaches the same accumulator as refreshing.
    ///
    /// Walks a real line, driving the scratch exactly as the search's `do_move` does, and
    /// compares each ply against a scratch that has seen nothing. A delta that drifts shows
    /// up at the ply it drifts on, with the move that caused it.
    #[test]
    fn rolling_a_line_forward_reaches_what_a_refresh_reaches() {
        use crate::board::movegen::generate_legal;

        let mut ft = FeatureTransformer::new();
        for (i, w) in ft.weights.iter_mut().enumerate() {
            *w = ((i * 7919) % 61) as i16 - 30;
        }
        for (i, w) in ft.threat_and_pp_weights.iter_mut().enumerate() {
            *w = ((i * 104_729) % 41) as i8 - 20;
        }
        for (i, w) in ft.psqt_weights.iter_mut().enumerate() {
            *w = ((i * 31) % 97) as i32 - 48;
        }
        for (i, w) in ft.threat_and_pp_psqt_weights.iter_mut().enumerate() {
            *w = ((i * 17) % 53) as i32 - 26;
        }

        // A deterministic walk: take the n-th legal move at each ply, for several n, so the
        // line covers captures, promotions and king moves rather than one narrow path.
        for pick in 0..7usize {
            let mut pos = Position::from_fen(
                "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
                false,
            )
            .expect("valid fen");
            let mut walk = EvalScratch::default();
            let _ = ft.transform(&pos, 0, 0, &mut walk);

            for ply in 1..12usize {
                let list = generate_legal(&pos);
                if list.is_empty() {
                    break;
                }
                let m = list.get(pick % list.len()).copied().expect("in range");
                let gives_check = pos.gives_check(m);
                let dts = walk.record_threats(ply);
                let dpp = pos.do_move_recording(m, gives_check, Some(dts));
                walk.record(ply, &pos, pos.raw_key(), dpp);

                let rolled = ft.transform(&pos, 0, ply, &mut walk);
                let rolled_out = walk.transformed().to_vec();

                let mut fresh = EvalScratch::default();
                let refreshed = ft.transform(&pos, 0, 0, &mut fresh);
                assert_eq!(rolled, refreshed, "pick {pick}, ply {ply}, after {m:?}: PSQT differs");
                assert_eq!(
                    rolled_out,
                    fresh.transformed().to_vec(),
                    "pick {pick}, ply {ply}, after {m:?}: accumulator differs"
                );
            }
        }
    }

    #[test]
    fn diffing_reaches_the_same_state_as_starting_over() {
        let mut ft = FeatureTransformer::new();
        // Give the weights some structure, or every difference is zero and proves nothing.
        for (i, w) in ft.weights.iter_mut().enumerate() {
            *w = ((i * 7919) % 61) as i16 - 30;
        }
        for (i, w) in ft.threat_and_pp_weights.iter_mut().enumerate() {
            *w = ((i * 104_729) % 41) as i8 - 20;
        }
        for (i, w) in ft.psqt_weights.iter_mut().enumerate() {
            *w = ((i * 7907) % 101) as i32 - 50;
        }

        let fens = [
            START_FEN,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 11",
            // Back to the first: the diff has to undo everything it applied.
            START_FEN,
            "4k3/8/8/8/8/8/8/4K3 w - - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        ];

        let mut shared = EvalScratch::default();
        for fen in fens {
            let pos = Position::from_fen(fen, false).expect("valid");
            let bucket = (pos.piece_total() as usize - 1) / 4;

            let pa = ft.transform(&pos, bucket, 0, &mut shared);
            let a = shared.transformed().to_vec();

            // A fresh scratch has nothing to diff against, so it computes from scratch.
            let mut fresh = EvalScratch::default();
            let pb = ft.transform(&pos, bucket, 0, &mut fresh);
            let b = fresh.transformed().to_vec();

            assert_eq!(a, b, "{fen}: diffed features differ from a fresh computation");
            assert_eq!(pa, pb, "{fen}: diffed PSQT differs from a fresh computation");
        }
    }

    /// `reset` must return the scratch to the state a fresh one is in.
    #[test]
    fn resetting_the_scratch_forces_a_fresh_computation() {
        let ft = FeatureTransformer::new();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        let first = ft.transform(&pos, 7, 0, &mut scratch);
        scratch.reset();
        let second = ft.transform(&pos, 7, 0, &mut scratch);
        assert_eq!(first, second);
    }

    #[test]
    fn a_zero_network_transforms_to_zero() {
        let ft = FeatureTransformer::new();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        let psqt = ft.transform(&pos, 7, 0, &mut scratch);
        assert_eq!(psqt, 0);
        assert!(scratch.transformed().iter().all(|&x| x == 0));
    }

    /// A positive bias in both halves must survive to the output as their scaled product,
    /// which is what pins the clamp and the shift.
    #[test]
    fn the_pairwise_product_uses_both_halves_and_saturates_at_the_clamp() {
        let mut ft = FeatureTransformer::new();
        let half = L1 / 2;
        ft.biases[0] = 300; // above the 255 clamp
        ft.biases[half] = 100;
        ft.biases[1] = -5; // below the 0 clamp
        ft.biases[half + 1] = 100;

        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        ft.transform(&pos, 7, 0, &mut scratch);

        // 300 clamps to 255: 255 * 100 / 512 = 49.
        assert_eq!(scratch.transformed()[0], 49);
        // -5 clamps to 0, so the product is zero however large the other half is.
        assert_eq!(scratch.transformed()[1], 0);
    }

    /// The PSQT head is a difference between the two perspectives, halved. A network
    /// symmetric in the two must therefore score zero.
    #[test]
    fn the_psqt_head_is_the_halved_perspective_difference() {
        let mut ft = FeatureTransformer::new();
        // Give every king-piece feature a constant PSQT weight in bucket 3. Both
        // perspectives then see the same total and the difference cancels.
        for i in 0..HALFKA_DIMENSIONS {
            ft.psqt_weights[i * PSQT_BUCKETS + 3] = 10;
        }
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        assert_eq!(ft.transform(&pos, 3, 0, &mut scratch), 0);
    }

    /// The scratch buffers must be reusable: a second call has to produce the same answer
    /// as the first, or the search would drift as it reuses them.
    #[test]
    fn transforming_twice_through_one_scratch_gives_the_same_answer() {
        let mut ft = FeatureTransformer::new();
        ft.biases[0] = 200;
        ft.biases[L1 / 2] = 200;
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        let pa = ft.transform(&pos, 7, 0, &mut scratch);
        let a = scratch.transformed().to_vec();
        let pb = ft.transform(&pos, 7, 0, &mut scratch);
        let b = scratch.transformed().to_vec();
        assert_eq!(a, b);
        assert_eq!(pa, pb);
    }
}
