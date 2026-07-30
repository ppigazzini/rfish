//! The feature transformer: the first and by far the largest layer.
//!
//! It turns the position's active feature indices into 1024 accumulated `i16` values per
//! perspective, plus an 8-bucket PSQT head. Ninety-nine percent of the network's weights
//! live here, and the whole design of NNUE is that this layer can be updated incrementally
//! rather than recomputed.
//!
//! # This accumulator is computed from scratch
//!
//! Upstream maintains it incrementally: a move changes a handful of features, so the
//! accumulator is patched rather than rebuilt, and a king-bucket cache absorbs the case
//! where the king moved. rfish recomputes it per evaluation.
//!
//! That is CORRECT and slow, in that order deliberately. The from-scratch path is what
//! upstream's incremental path has to agree with, so it is the thing to have first and the
//! thing an incremental update is later checked against. Making it incremental needs the
//! board zone to maintain a per-move threat delta, which it does not yet — see
//! `docs/01-engine-board.md`.
//!
//! Golden: `Stockfish/src/nnue/nnue_feature_transformer.h`,
//! `Stockfish/src/nnue/nnue_accumulator.cpp`.

use crate::board::position::Position;
use crate::board::types::{COLOR_NB, Color};

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

/// Scratch buffers an evaluation reuses, so a search does not allocate per node.
#[derive(Debug, Default)]
pub struct EvalScratch {
    accumulator: Accumulator,
    halfka: Vec<u32>,
    threats: Vec<u32>,
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

    /// Accumulate every active feature for `perspective` into `out`.
    fn accumulate(
        &self,
        pos: &Position,
        perspective: Color,
        halfka: &mut Vec<u32>,
        threats: &mut Vec<u32>,
        acc: &mut [i16],
        psqt: &mut [i32; PSQT_BUCKETS],
    ) {
        acc.copy_from_slice(&self.biases);
        psqt.fill(0);

        halfka.clear();
        halfka_active(pos, perspective, halfka);
        for &index in halfka.iter() {
            let row = &self.weights[index as usize * L1..index as usize * L1 + L1];
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                // Wrapping is upstream's behaviour: the accumulator is `i16` and the
                // trainer keeps the sum in range, so a wrap here means a corrupt net rather
                // than a value to saturate.
                *a = a.wrapping_add(*w);
            }
            let prow = &self.psqt_weights
                [index as usize * PSQT_BUCKETS..index as usize * PSQT_BUCKETS + PSQT_BUCKETS];
            for (p, w) in psqt.iter_mut().zip(prow.iter()) {
                *p += w;
            }
        }

        threats.clear();
        threat_active(pos, perspective, threats);
        pawn_pair_active(pos, perspective, threats);
        for &index in threats.iter() {
            let row = &self.threat_and_pp_weights[index as usize * L1..index as usize * L1 + L1];
            for (a, w) in acc.iter_mut().zip(row.iter()) {
                *a = a.wrapping_add(i16::from(*w));
            }
            let prow = &self.threat_and_pp_psqt_weights
                [index as usize * PSQT_BUCKETS..index as usize * PSQT_BUCKETS + PSQT_BUCKETS];
            for (p, w) in psqt.iter_mut().zip(prow.iter()) {
                *p += w;
            }
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
        let us = pos.side_to_move();
        let perspectives = [us, !us];

        for p in perspectives {
            let EvalScratch { accumulator, halfka, threats } = scratch;
            let (acc, psqt) =
                (&mut accumulator.accumulation[p.index()], &mut accumulator.psqt[p.index()]);
            self.accumulate(pos, p, halfka, threats, acc, psqt);
        }

        let psqt = (scratch.accumulator.psqt[perspectives[0].index()][bucket]
            - scratch.accumulator.psqt[perspectives[1].index()][bucket])
            / 2;

        let half = L1 / 2;
        for (p, side) in perspectives.iter().enumerate() {
            let acc = &scratch.accumulator.accumulation[side.index()];
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
