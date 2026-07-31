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

use crate::board::position::Position;
use crate::board::types::{COLOR_NB, Color, Key, Piece, SQUARE_NB, Square};

use super::common::{FT_MAX_VAL, L1, NetError, NetReader, NetWriter, PSQT_BUCKETS};
use super::features::{
    HALFKA_DIMENSIONS, THREAT_AND_PP_DIMENSIONS, halfka_delta, pawn_pair_active, threat_active,
};

/// The transformer's weights.
///
/// Around 112 MiB, all heap-allocated: an array this size cannot live in a stack frame and
/// cannot be a `static` without making the binary that size.
pub struct FeatureTransformer {
    /// One bias per output, shared by both perspectives.
    biases: Vec<i16>,
    /// `weights[index * L1 + j]` for the king-piece features.
    weights: Vec<i16>,
    /// `threat_and_pp_weights[index * L1 + j]`, `i8` because these features are many and
    /// their individual contributions small.
    threat_and_pp_weights: Vec<i8>,
    /// `psqt_weights[index * PSQT_BUCKETS + k]` for the king-piece features.
    psqt_weights: Vec<i32>,
    /// The same, for the threat and pawn-pair features.
    threat_and_pp_psqt_weights: Vec<i32>,
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
    acc: Vec<i16>,
    psqt: [i32; PSQT_BUCKETS],
    /// The placement that produced the above. The king-piece features are diffed straight
    /// off this rather than recomputed and merged, so the set itself is never materialised.
    board: [Piece; SQUARE_NB],
    /// The active threat and pawn-pair set, SORTED so the next diff is a merge walk. These
    /// have no equivalent board diff: one square changing moves many threats at once.
    threats: Vec<u32>,
}

impl Side {
    fn empty() -> Side {
        Side {
            king: None,
            acc: Vec::new(),
            psqt: [0; PSQT_BUCKETS],
            board: [Piece::NONE; SQUARE_NB],
            threats: Vec::new(),
        }
    }
}

/// Scratch buffers an evaluation reuses, so a search does not allocate per node.
#[derive(Debug)]
pub struct EvalScratch {
    /// The position the live slots hold, or [`EMPTY_KEY`] when they hold nothing.
    key: Key,
    /// The accumulator the search is walking, one per perspective.
    live: [Side; COLOR_NB],
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
    cache: [Vec<Side>; COLOR_NB],
    /// Freshly computed sets, reused between calls to avoid reallocating.
    next_threats: [Vec<u32>; COLOR_NB],
    /// The features one diff adds and removes, collected before either is applied.
    adds: Vec<u32>,
    subs: Vec<u32>,
}

/// The key of a slot no evaluation has filled yet. No real position hashes to it.
const EMPTY_KEY: Key = Key::MAX;

impl Default for EvalScratch {
    fn default() -> EvalScratch {
        EvalScratch {
            key: EMPTY_KEY,
            live: [Side::empty(), Side::empty()],
            cache: [Vec::new(), Vec::new()],
            next_threats: [Vec::new(), Vec::new()],
            adds: Vec::new(),
            subs: Vec::new(),
        }
    }
}

impl EvalScratch {
    /// Forget every cached accumulator.
    ///
    /// Not needed for correctness — a diff against any position is still exact — but a
    /// caller that has jumped somewhere unrelated saves the work of a large diff.
    pub fn reset(&mut self) {
        self.key = EMPTY_KEY;
        for i in 0..COLOR_NB {
            self.live[i] = Side::empty();
            self.cache[i].clear();
        }
    }
}

/// Accumulator entries the fold carries at once.
///
/// Measured rather than reasoned: 32, 64, 128 and 256 were all built and run, and the curve
/// is not monotonic in either direction. On `bench 16 1 8` at nehalem, whole-run search
/// instructions were 4,093M / 3,673M / 3,598M / 4,037M. Too small and the per-tile sweep
/// overhead — the copy in and the copy out — is paid too many times; too large and the
/// running values no longer stay in registers across the row loop. 128 is the peak on this
/// tier; a tier with more or wider registers may well peak elsewhere, so re-measure rather
/// than assuming this number travels.
const TILE: usize = 128;

/// [`FT_MAX_VAL`] at the accumulator's own width.
const FT_MAX: i16 = FT_MAX_VAL as i16;

impl FeatureTransformer {
    /// An unloaded transformer with every weight zero.
    #[must_use]
    pub fn new() -> FeatureTransformer {
        FeatureTransformer {
            biases: vec![0; L1],
            weights: vec![0; L1 * HALFKA_DIMENSIONS],
            threat_and_pp_weights: vec![0; L1 * THREAT_AND_PP_DIMENSIONS],
            psqt_weights: vec![0; PSQT_BUCKETS * HALFKA_DIMENSIONS],
            threat_and_pp_psqt_weights: vec![0; PSQT_BUCKETS * THREAT_AND_PP_DIMENSIONS],
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

    /// Walk two sorted sets, applying only what differs.
    ///
    /// Both sets are strictly increasing and hold no duplicates — a feature index encodes
    /// exactly one (piece, square) or (attacker, from, to, attacked) tuple — so a merge walk
    /// is a complete and exact diff.
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
    fn diff_apply(
        &self,
        old: &[u32],
        new: &[u32],
        halfka: bool,
        acc: &mut [i16],
        psqt: &mut [i32; PSQT_BUCKETS],
        adds: &mut Vec<u32>,
        subs: &mut Vec<u32>,
    ) {
        adds.clear();
        subs.clear();
        let (mut i, mut j) = (0usize, 0usize);
        while i < old.len() && j < new.len() {
            match old[i].cmp(&new[j]) {
                core::cmp::Ordering::Equal => {
                    // Present in both: the accumulator already holds it.
                    i += 1;
                    j += 1;
                }
                core::cmp::Ordering::Less => {
                    subs.push(old[i]);
                    i += 1;
                }
                core::cmp::Ordering::Greater => {
                    adds.push(new[j]);
                    j += 1;
                }
            }
        }
        // Whatever is left in one list has no counterpart in the other.
        subs.extend_from_slice(&old[i..]);
        adds.extend_from_slice(&new[j..]);

        self.fold_changed(halfka, adds, subs, acc, psqt);
    }

    /// Apply a set of added and removed features to the accumulator and the PSQT head.
    ///
    /// Takes the changes rather than deriving them, because the two feature kinds arrive at
    /// them differently: the king-piece features come from a board diff and the threat
    /// features from a merge walk over two sorted sets.
    fn fold_changed(
        &self,
        halfka: bool,
        adds: &[u32],
        subs: &[u32],
        acc: &mut [i16],
        psqt: &mut [i32; PSQT_BUCKETS],
    ) {
        if adds.is_empty() && subs.is_empty() {
            return;
        }

        let psqt_weights =
            if halfka { &self.psqt_weights } else { &self.threat_and_pp_psqt_weights };
        for &index in subs {
            let base = index as usize * PSQT_BUCKETS;
            for (p, w) in psqt.iter_mut().zip(psqt_weights[base..base + PSQT_BUCKETS].iter()) {
                *p -= w;
            }
        }
        for &index in adds {
            let base = index as usize * PSQT_BUCKETS;
            for (p, w) in psqt.iter_mut().zip(psqt_weights[base..base + PSQT_BUCKETS].iter()) {
                *p += w;
            }
        }

        if halfka {
            Self::fold_rows_i16(&self.weights, adds, subs, acc);
        } else {
            Self::fold_rows_i8(&self.threat_and_pp_weights, adds, subs, acc);
        }
    }

    /// Add every row in `adds` and subtract every row in `subs`, in one sweep of `acc`.
    ///
    /// Swept a TILE at a time so the running values stay in registers: the accumulator is
    /// read once and written once however many rows are folded into it, where applying one
    /// row at a time read and wrote all of it every time.
    fn fold_rows_i16(weights: &[i16], adds: &[u32], subs: &[u32], acc: &mut [i16]) {
        for (t, chunk) in acc.as_chunks_mut::<TILE>().0.iter_mut().enumerate() {
            let off = t * TILE;
            let mut tile = [0i16; TILE];
            tile.copy_from_slice(chunk);
            for &index in subs {
                let base = index as usize * L1 + off;
                for (a, w) in tile.iter_mut().zip(weights[base..base + TILE].iter()) {
                    // Wrapping is upstream's behaviour: the accumulator is `i16` and the
                    // trainer keeps the sum in range, so a wrap here means a corrupt net
                    // rather than a value to saturate.
                    *a = a.wrapping_sub(*w);
                }
            }
            for &index in adds {
                let base = index as usize * L1 + off;
                for (a, w) in tile.iter_mut().zip(weights[base..base + TILE].iter()) {
                    *a = a.wrapping_add(*w);
                }
            }
            chunk.copy_from_slice(&tile);
        }
    }

    /// [`FeatureTransformer::fold_rows_i16`] for the threat and pawn-pair rows, whose
    /// weights are one byte wide.
    fn fold_rows_i8(weights: &[i8], adds: &[u32], subs: &[u32], acc: &mut [i16]) {
        for (t, chunk) in acc.as_chunks_mut::<TILE>().0.iter_mut().enumerate() {
            let off = t * TILE;
            let mut tile = [0i16; TILE];
            tile.copy_from_slice(chunk);
            for &index in subs {
                let base = index as usize * L1 + off;
                for (a, w) in tile.iter_mut().zip(weights[base..base + TILE].iter()) {
                    *a = a.wrapping_sub(i16::from(*w));
                }
            }
            for &index in adds {
                let base = index as usize * L1 + off;
                for (a, w) in tile.iter_mut().zip(weights[base..base + TILE].iter()) {
                    *a = a.wrapping_add(i16::from(*w));
                }
            }
            chunk.copy_from_slice(&tile);
        }
    }

    /// The active feature sets for one perspective, sorted.
    fn active_sets(pos: &Position, threats: &mut [Vec<u32>; COLOR_NB]) {
        for t in threats.iter_mut() {
            t.clear();
        }
        threat_active(pos, threats);
        pawn_pair_active(pos, threats);
        for t in threats.iter_mut() {
            t.sort_unstable();
        }
    }

    /// Fill `output` with the transformed features and return the PSQT score.
    ///
    /// The output is a pairwise product, not an activation: the 1024 accumulated values are
    /// split in half and multiplied element-wise, which is what gives the first hidden layer
    /// a quadratic term without a second matrix. Both halves are clamped to `[0, 255]`
    /// first, so the product fits `u8` after the shift.
    pub fn transform(
        &self,
        pos: &Position,
        bucket: usize,
        scratch: &mut EvalScratch,
        output: &mut [u8],
    ) -> i32 {
        debug_assert_eq!(output.len(), L1);
        // The RAW key, not the table key: the accumulator depends on the pieces alone, and
        // mixing the halfmove clock in would miss the cache every time the clock ticked
        // past fourteen without a single feature having changed.
        let key = pos.raw_key();

        // The same position twice in a row -- a quiescence stand-pat after a main-search
        // evaluation, say -- needs no work at all.
        if scratch.key != key {
            Self::active_sets(pos, &mut scratch.next_threats);

            for p in Color::ALL {
                let i = p.index();
                let ksq = pos.king_square(p);

                let refreshed = scratch.live[i].king != Some(ksq);
                if refreshed {
                    // This perspective's king has moved, so every feature it sees has been
                    // re-indexed and the live slot is no longer a useful base. Take the last
                    // accumulator computed for THIS king square instead. With none -- the
                    // first evaluation from this square -- the base is the biases and an
                    // empty active set, so the "diff" is every feature, which is exactly the
                    // from-scratch computation. One code path serves both, so there is no
                    // second implementation to disagree.
                    if scratch.cache[i].is_empty() {
                        scratch.cache[i].resize(SQUARE_NB, Side::empty());
                    }
                    let src = &scratch.cache[i][ksq.index()];
                    let live = &mut scratch.live[i];
                    if src.king.is_some() {
                        live.acc.clone_from(&src.acc);
                        live.psqt = src.psqt;
                        live.board = src.board;
                        live.threats.clone_from(&src.threats);
                    } else {
                        live.acc.clear();
                        live.acc.extend_from_slice(&self.biases);
                        live.psqt = [0; PSQT_BUCKETS];
                        live.board = [Piece::NONE; SQUARE_NB];
                        live.threats.clear();
                    }
                    live.king = Some(ksq);
                }

                let live = &mut scratch.live[i];
                // The king-piece features come straight off a board diff: no set to build,
                // no sort, no merge walk. The fold does not care what order they arrive in.
                scratch.adds.clear();
                scratch.subs.clear();
                halfka_delta(
                    &live.board,
                    pos.board(),
                    p,
                    ksq,
                    &mut scratch.adds,
                    &mut scratch.subs,
                );
                self.fold_changed(
                    true,
                    &scratch.adds,
                    &scratch.subs,
                    &mut live.acc,
                    &mut live.psqt,
                );
                live.board = *pos.board();
                self.diff_apply(
                    &live.threats,
                    &scratch.next_threats[i],
                    false,
                    &mut live.acc,
                    &mut live.psqt,
                    &mut scratch.adds,
                    &mut scratch.subs,
                );
                live.threats.clone_from(&scratch.next_threats[i]);

                // Refresh this king square's slot only when the refresh path was taken.
                // Writing it on every evaluation costs a copy per evaluation to make a rare
                // case cheaper, which is the trade the per-ply stack already lost twice.
                if refreshed {
                    scratch.cache[i][ksq.index()].clone_from(live);
                }
            }
            scratch.key = key;
        }

        let us = pos.side_to_move();
        let perspectives = [us, !us];
        let psqt = (scratch.live[perspectives[0].index()].psqt[bucket]
            - scratch.live[perspectives[1].index()].psqt[bucket])
            / 2;

        let half = L1 / 2;
        // Both operands are clamped into [0, 255], so their product cannot exceed 65,025 and
        // the whole pairwise step fits in `u16` -- twice the lanes per register that the
        // `i32` form allowed, for identical values. The `/ 512` is a shift for the same
        // reason: the product is never negative, so there is no rounding direction to get
        // wrong.
        for (p, side) in perspectives.iter().enumerate() {
            let (lo, hi) = scratch.live[side.index()].acc.split_at(half);
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

            let mut a = vec![0u8; L1];
            let pa = ft.transform(&pos, bucket, &mut shared, &mut a);

            // A fresh scratch has nothing to diff against, so it computes from scratch.
            let mut fresh = EvalScratch::default();
            let mut b = vec![0u8; L1];
            let pb = ft.transform(&pos, bucket, &mut fresh, &mut b);

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
        let mut out = vec![0u8; L1];
        let first = ft.transform(&pos, 7, &mut scratch, &mut out);
        scratch.reset();
        let second = ft.transform(&pos, 7, &mut scratch, &mut out);
        assert_eq!(first, second);
    }

    #[test]
    fn a_zero_network_transforms_to_zero() {
        let ft = FeatureTransformer::new();
        let pos = Position::from_fen(START_FEN, false).expect("valid");
        let mut scratch = EvalScratch::default();
        let mut out = vec![0u8; L1];
        let psqt = ft.transform(&pos, 7, &mut scratch, &mut out);
        assert_eq!(psqt, 0);
        assert!(out.iter().all(|&x| x == 0));
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
        let mut out = vec![0u8; L1];
        ft.transform(&pos, 7, &mut scratch, &mut out);

        // 300 clamps to 255: 255 * 100 / 512 = 49.
        assert_eq!(out[0], 49);
        // -5 clamps to 0, so the product is zero however large the other half is.
        assert_eq!(out[1], 0);
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
        let mut out = vec![0u8; L1];
        assert_eq!(ft.transform(&pos, 3, &mut scratch, &mut out), 0);
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
        let mut a = vec![0u8; L1];
        let mut b = vec![0u8; L1];
        let pa = ft.transform(&pos, 7, &mut scratch, &mut a);
        let pb = ft.transform(&pos, 7, &mut scratch, &mut b);
        assert_eq!(a, b);
        assert_eq!(pa, pb);
    }
}
