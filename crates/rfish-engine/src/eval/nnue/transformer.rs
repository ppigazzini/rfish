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
use crate::board::types::{COLOR_NB, Color, Key};

use super::common::{FT_MAX_VAL, L1, NetError, NetReader, PSQT_BUCKETS};
use super::features::{
    HALFKA_DIMENSIONS, THREAT_AND_PP_DIMENSIONS, halfka_active, pawn_pair_active, threat_active,
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
/// features. A deeper stack would cost memory for cases that do not arise.
#[derive(Clone, Debug)]
struct Cached {
    key: Key,
    accumulation: [Vec<i16>; COLOR_NB],
    psqt: [[i32; PSQT_BUCKETS]; COLOR_NB],
    /// The active sets that produced the above, SORTED so the next diff is a merge walk.
    halfka: [Vec<u32>; COLOR_NB],
    threats: [Vec<u32>; COLOR_NB],
}

/// Scratch buffers an evaluation reuses, so a search does not allocate per node.
#[derive(Debug, Default)]
pub struct EvalScratch {
    cached: Option<Cached>,
    /// Freshly computed sets, reused between calls to avoid reallocating.
    next_halfka: [Vec<u32>; COLOR_NB],
    next_threats: [Vec<u32>; COLOR_NB],
}

impl EvalScratch {
    /// Forget the cached accumulator.
    ///
    /// Not needed for correctness — a diff against any position is still exact — but a
    /// caller that has jumped somewhere unrelated saves the work of a large diff.
    pub fn reset(&mut self) {
        self.cached = None;
    }
}

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

    /// Add or subtract one king-piece feature's weight row.
    #[inline]
    fn apply_halfka(&self, index: u32, add: bool, acc: &mut [i16], psqt: &mut [i32; PSQT_BUCKETS]) {
        let base = index as usize * L1;
        let row = &self.weights[base..base + L1];
        if add {
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                // Wrapping is upstream's behaviour: the accumulator is `i16` and the trainer
                // keeps the sum in range, so a wrap here means a corrupt net rather than a
                // value to saturate.
                *a = a.wrapping_add(*w);
            }
        } else {
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                *a = a.wrapping_sub(*w);
            }
        }
        let pbase = index as usize * PSQT_BUCKETS;
        let prow = &self.psqt_weights[pbase..pbase + PSQT_BUCKETS];
        for (p, w) in psqt.iter_mut().zip(prow.iter()) {
            if add {
                *p += w;
            } else {
                *p -= w;
            }
        }
    }

    /// The same for a threat or pawn-pair feature, whose weights are one byte wide.
    #[inline]
    fn apply_threat(&self, index: u32, add: bool, acc: &mut [i16], psqt: &mut [i32; PSQT_BUCKETS]) {
        let base = index as usize * L1;
        let row = &self.threat_and_pp_weights[base..base + L1];
        if add {
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                *a = a.wrapping_add(i16::from(*w));
            }
        } else {
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                *a = a.wrapping_sub(i16::from(*w));
            }
        }
        let pbase = index as usize * PSQT_BUCKETS;
        let prow = &self.threat_and_pp_psqt_weights[pbase..pbase + PSQT_BUCKETS];
        for (p, w) in psqt.iter_mut().zip(prow.iter()) {
            if add {
                *p += w;
            } else {
                *p -= w;
            }
        }
    }

    /// Walk two sorted sets, applying only what differs.
    ///
    /// Both sets are strictly increasing and hold no duplicates — a feature index encodes
    /// exactly one (piece, square) or (attacker, from, to, attacked) tuple — so a merge walk
    /// is a complete and exact diff.
    fn diff_apply(
        &self,
        old: &[u32],
        new: &[u32],
        halfka: bool,
        acc: &mut [i16],
        psqt: &mut [i32; PSQT_BUCKETS],
    ) {
        let (mut i, mut j) = (0usize, 0usize);
        while i < old.len() && j < new.len() {
            match old[i].cmp(&new[j]) {
                core::cmp::Ordering::Equal => {
                    // Present in both: the accumulator already holds it.
                    i += 1;
                    j += 1;
                }
                core::cmp::Ordering::Less => {
                    self.apply(old[i], false, halfka, acc, psqt);
                    i += 1;
                }
                core::cmp::Ordering::Greater => {
                    self.apply(new[j], true, halfka, acc, psqt);
                    j += 1;
                }
            }
        }
        // Whatever is left in one list has no counterpart in the other.
        for &a in &old[i..] {
            self.apply(a, false, halfka, acc, psqt);
        }
        for &b in &new[j..] {
            self.apply(b, true, halfka, acc, psqt);
        }
    }

    #[inline]
    fn apply(
        &self,
        index: u32,
        add: bool,
        halfka: bool,
        acc: &mut [i16],
        psqt: &mut [i32; PSQT_BUCKETS],
    ) {
        if halfka {
            self.apply_halfka(index, add, acc, psqt);
        } else {
            self.apply_threat(index, add, acc, psqt);
        }
    }

    /// The active feature sets for one perspective, sorted.
    fn active_sets(
        pos: &Position,
        perspective: Color,
        halfka: &mut Vec<u32>,
        threats: &mut Vec<u32>,
    ) {
        halfka.clear();
        halfka_active(pos, perspective, halfka);
        halfka.sort_unstable();

        threats.clear();
        threat_active(pos, perspective, threats);
        pawn_pair_active(pos, perspective, threats);
        threats.sort_unstable();
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
        let key = pos.key();

        // The same position twice in a row -- a quiescence stand-pat after a main-search
        // evaluation, say -- needs no work at all.
        let unchanged = scratch.cached.as_ref().is_some_and(|c| c.key == key);
        if !unchanged {
            for p in Color::ALL {
                Self::active_sets(
                    pos,
                    p,
                    &mut scratch.next_halfka[p.index()],
                    &mut scratch.next_threats[p.index()],
                );
            }

            // With no cached state, start from the biases and an empty active set: the
            // "diff" is then every feature, which is exactly the from-scratch computation.
            // One code path serves both, so there is no second implementation to disagree.
            let c = scratch.cached.get_or_insert_with(|| Cached {
                key,
                accumulation: [self.biases.clone(), self.biases.clone()],
                psqt: [[0; PSQT_BUCKETS]; COLOR_NB],
                halfka: [Vec::new(), Vec::new()],
                threats: [Vec::new(), Vec::new()],
            });
            for p in Color::ALL {
                let i = p.index();
                let (acc, psqt) = (&mut c.accumulation[i], &mut c.psqt[i]);
                self.diff_apply(&c.halfka[i], &scratch.next_halfka[i], true, acc, psqt);
                self.diff_apply(&c.threats[i], &scratch.next_threats[i], false, acc, psqt);
                c.halfka[i].clone_from(&scratch.next_halfka[i]);
                c.threats[i].clone_from(&scratch.next_threats[i]);
            }
            c.key = key;
        }

        let cached = scratch.cached.as_ref().expect("just populated");
        let us = pos.side_to_move();
        let perspectives = [us, !us];
        let psqt = (cached.psqt[perspectives[0].index()][bucket]
            - cached.psqt[perspectives[1].index()][bucket])
            / 2;

        let half = L1 / 2;
        for (p, side) in perspectives.iter().enumerate() {
            let acc = &cached.accumulation[side.index()];
            let offset = half * p;
            for j in 0..half {
                let sum0 = i32::from(acc[j]).clamp(0, FT_MAX_VAL);
                let sum1 = i32::from(acc[j + half]).clamp(0, FT_MAX_VAL);
                output[offset + j] = ((sum0 * sum1) / 512) as u8;
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
