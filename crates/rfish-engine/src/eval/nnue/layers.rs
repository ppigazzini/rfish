//! The fully connected layers and their activations.
//!
//! Everything here is integer arithmetic on a fixed quantisation. The shifts are not
//! rounding conveniences — they are where the quantisation scale changes, and moving one by
//! a bit changes every score the network produces.
//!
//! # No intrinsics
//!
//! Upstream writes these as hand-vectorised kernels behind one `#if` per instruction set.
//! `std::arch` intrinsics are `unsafe` and `std::simd` is nightly, so rfish writes the
//! loops and lets LLVM vectorise them under `-C target-cpu`. The arithmetic below is
//! upstream's SCALAR fallback, which upstream keeps precisely so that the vector paths have
//! something to be bit-identical to.
//!
//! Golden: `Stockfish/src/nnue/layers/affine_transform.h`, `clipped_relu.h`,
//! `sqr_clipped_relu.h`.

use core::simd::Simd;
use core::simd::cmp::SimdPartialEq;
use core::simd::num::SimdInt;

use super::common::{NetError, NetReader, NetWriter, ceil_to_multiple};

/// Inputs whose non-zero test is answered by one vector compare.
///
/// Upstream's `find_nnz` does exactly this and then walks the resulting bitmask. The point
/// is not the compare -- it is that finding the non-zero inputs stops being a branch per
/// input. At this layer's density that branch is unpredictable roughly 40% of the time, 1024
/// times per evaluation, and no amount of reshaping the scalar loop removes it.
const SCAN: usize = 64;

/// The padded input width a layer's weight rows are stored at.
///
/// Upstream rounds every layer's input up to 32 so a vector load never crosses the end of a
/// row. The FILE contains the padding, so the reader must consume it even though the scalar
/// path never reads it.
#[must_use]
pub const fn padded(input: usize) -> usize {
    ceil_to_multiple(input, 32)
}

/// A fully connected layer, sized at run time rather than by const generics.
///
/// Const-generic array sizes would be prettier, but the layer shapes come from the file
/// format and are checked against it — a `Vec` with an asserted length carries the same
/// guarantee and does not need a `where` clause at every use.
#[derive(Debug)]
pub struct AffineLayer {
    input_dims: usize,
    padded_dims: usize,
    output_dims: usize,
    biases: Vec<i32>,
    weights: Vec<i8>,
    /// The same weights in the order [`AffineLayer::propagate_sparse`] walks them, or empty
    /// when that path is not enabled for this layer. See [`AffineLayer::enable_sparse`].
    sparse: Vec<i8>,
}

impl AffineLayer {
    /// An unloaded layer of the given shape.
    #[must_use]
    pub fn new(input_dims: usize, output_dims: usize) -> AffineLayer {
        let padded_dims = padded(input_dims);
        AffineLayer {
            input_dims,
            padded_dims,
            output_dims,
            biases: vec![0; output_dims],
            weights: vec![0; output_dims * padded_dims],
            sparse: vec![0; input_dims * output_dims],
        }
    }

    /// Rebuild the transposed weight copy [`AffineLayer::propagate_sparse`] reads.
    ///
    /// Maintained as an invariant rather than offered as an opt-in: every path that changes
    /// the weights ends here, so no caller can leave the two copies disagreeing. The dense
    /// weights stay as they were read, so [`AffineLayer::write`] still round-trips the file.
    fn rebuild_sparse(&mut self) {
        for o in 0..self.output_dims {
            for i in 0..self.input_dims {
                // Transposed: every output's weight for one input, contiguously. That is
                // the run `propagate_sparse` sweeps once it has decided an input is worth
                // visiting at all.
                self.sparse[i * self.output_dims + o] = self.weights[o * self.padded_dims + i];
            }
        }
    }

    /// Read this layer's parameters from the stream.
    ///
    /// Biases first, then the weight rows INCLUDING their padding, all uncompressed
    /// little-endian. Skipping the padding would leave the stream one row short and every
    /// later layer would read someone else's weights.
    pub fn read(&mut self, r: &mut NetReader<impl std::io::Read>) -> Result<(), NetError> {
        r.i32s(&mut self.biases)?;
        r.i8s(&mut self.weights)?;
        self.rebuild_sparse();
        Ok(())
    }

    /// Write this layer back in the form [`AffineLayer::read`] expects.
    pub fn write(&self, w: &mut NetWriter<impl std::io::Write>) -> Result<(), NetError> {
        w.i32s(&self.biases)?;
        w.i8s(&self.weights)?;
        Ok(())
    }

    /// `output[o] = bias[o] + sum_i weight[o][i] * input[i]`.
    ///
    /// `N` is the output width, static for the same reason as in
    /// [`AffineLayer::propagate_sparse`].
    pub fn propagate<const N: usize>(&self, input: &[u8], output: &mut [i32; N]) {
        debug_assert!(input.len() >= self.input_dims);
        debug_assert_eq!(N, self.output_dims);
        let rows = self.weights.chunks_exact(self.padded_dims);
        for ((out, bias), row) in output.iter_mut().zip(self.biases.iter()).zip(rows) {
            let mut sum = *bias;
            for (w, &x) in row[..self.input_dims].iter().zip(input[..self.input_dims].iter()) {
                sum += i32::from(*w) * i32::from(x);
            }
            *out = sum;
        }
    }

    /// The same result as [`AffineLayer::propagate`], skipping the inputs that are zero.
    ///
    /// This is upstream's `AffineTransformSparseInput`, and it is the first layer's whole
    /// cost model. The activation feeding it clamps at zero from below, so most of its 1024
    /// outputs ARE zero — and a zero input contributes zero to every output, so the chunks
    /// containing only zeros can be skipped outright. The result is bit-identical by
    /// construction rather than by approximation: nothing is dropped that could have
    /// contributed.
    ///
    /// Inputs are tested ONE at a time, where upstream's kernel tests four. The two engines
    /// are skipping zeros against different cost models. Upstream's `vpdpbusd` dots four
    /// bytes in a single instruction, so a group of four costs it what a group of one costs
    /// and the coarser test comes free. rfish has no such instruction and pays per multiply,
    /// so the only thing that matters is how much work a test can skip — and a group is
    /// skippable only when EVERY input in it is zero. At this layer's density four is the
    /// wrong number: with roughly 40% of the inputs non-zero, a group of four survives the
    /// test about 87% of the time and almost nothing is skipped, where a group of one skips
    /// 60% outright. Measured, `bench 16 1 8` at `nehalem`, against the same net and the
    /// same 166 964 nodes: groups of four 5,783,523,617 instructions, groups of one
    /// 4,769,344,411.
    /// `N` is the output width, static so the accumulators can stay in registers. Through a
    /// `&mut [i32]` the compiler must assume the stores alias the weights it is reading and
    /// spills all of them on every input; through a fixed-size array taken by value it does
    /// not have to.
    pub fn propagate_sparse<const N: usize>(&self, input: &[u8], output: &mut [i32; N]) {
        debug_assert!(input.len() >= self.input_dims);
        debug_assert_eq!(N, self.output_dims);

        // The whole output row lives in registers for the duration, as one vector.
        let mut acc = Simd::<i32, N>::from_slice(&self.biases);
        let blocks = self.sparse.as_chunks::<N>().0;
        let (scans, tail) = input[..self.input_dims].as_chunks::<SCAN>();

        for (c, chunk) in scans.iter().enumerate() {
            // One compare answers SCAN inputs, and the bitmask walk visits only the inputs
            // that are actually non-zero -- no branch is taken for the ones that are not.
            let mut nnz = Simd::<u8, SCAN>::from_array(*chunk).simd_ne(Simd::splat(0)).to_bitmask();
            while nnz != 0 {
                let lane = nnz.trailing_zeros() as usize;
                nnz &= nnz - 1;
                let i = c * SCAN + lane;
                let w: Simd<i8, N> = Simd::from_array(blocks[i]);
                acc += w.cast::<i32>() * Simd::splat(i32::from(chunk[lane]));
            }
        }
        for (k, &x) in tail.iter().enumerate() {
            if x != 0 {
                let i = scans.len() * SCAN + k;
                let w: Simd<i8, N> = Simd::from_array(blocks[i]);
                acc += w.cast::<i32>() * Simd::splat(i32::from(x));
            }
        }

        *output = acc.to_array();
    }

    /// How many bytes of weights and biases this layer holds.
    #[must_use]
    pub fn weight_bytes(&self) -> usize {
        self.biases.len() * 4 + self.weights.len()
    }

    /// How many outputs this layer has.
    #[must_use]
    pub fn output_dims(&self) -> usize {
        self.output_dims
    }
}

/// `output[i] = clamp(input[i] >> shift, 0, 127)`.
///
/// The shift is where the accumulator's quantisation scale drops back to the activation's.
pub fn clipped_relu(input: &[i32], shift: u32, output: &mut [u8]) {
    debug_assert_eq!(input.len(), output.len());
    for (o, &x) in output.iter_mut().zip(input.iter()) {
        *o = (x >> shift).clamp(0, 127) as u8;
    }
}

/// `output[i] = min(127, (input[i]^2) >> (2 * shift + 7))`.
///
/// The extra seven bits are upstream's: the value should be divided by 127 after squaring,
/// and a shift by seven is used instead because it is one instruction. The trainer knows,
/// and compensates — which is why this cannot be "corrected".
///
/// No lower clamp is needed: a square is never negative.
pub fn sqr_clipped_relu(input: &[i32], shift: u32, output: &mut [u8]) {
    debug_assert_eq!(input.len(), output.len());
    for (o, &x) in output.iter_mut().zip(input.iter()) {
        let squared = i64::from(x) * i64::from(x);
        *o = (squared >> (2 * shift + 7)).min(127) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_rounds_every_layer_width_up_to_thirty_two() {
        assert_eq!(padded(1024), 1024);
        assert_eq!(padded(64), 64);
        assert_eq!(padded(128), 128);
        assert_eq!(padded(1), 32);
        assert_eq!(padded(33), 64);
    }

    #[test]
    fn an_affine_layer_computes_bias_plus_the_dot_product() {
        let mut l = AffineLayer::new(4, 2);
        l.biases = vec![10, -10];
        // Row 0 is [1, 2, 3, 4] then padding; row 1 is [-1, -1, -1, -1].
        l.weights = vec![0; 2 * 32];
        l.weights[0..4].copy_from_slice(&[1, 2, 3, 4]);
        l.weights[32..36].copy_from_slice(&[-1, -1, -1, -1]);

        let mut out = [0i32; 2];
        l.propagate(&[1, 2, 3, 4], &mut out);
        assert_eq!(out[0], 10 + 1 + 4 + 9 + 16);
        assert_eq!(out[1], -10 - 10);
    }

    /// The padding must be skipped, not read as data: a row read at the unpadded stride
    /// walks into the next row and every output after the first is wrong.
    #[test]
    fn the_weight_rows_are_read_at_the_padded_stride() {
        let mut l = AffineLayer::new(4, 2);
        // Biases: two i32. Weights: 2 rows x 32 padded bytes.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7i32.to_le_bytes());
        bytes.extend_from_slice(&(-7i32).to_le_bytes());
        let mut w = vec![0u8; 64];
        w[0] = 1;
        w[32] = 2;
        bytes.extend_from_slice(&w);

        l.read(&mut NetReader::new(bytes.as_slice())).expect("reads");
        assert_eq!(l.biases, vec![7, -7]);
        assert_eq!(l.weights[0], 1);
        assert_eq!(l.weights[32], 2);
    }

    #[test]
    fn clipped_relu_clamps_at_both_ends_after_the_shift() {
        let input = [-1000, 0, 64, 127 << 7, 1 << 30];
        let mut out = [0u8; 5];
        clipped_relu(&input, 7, &mut out);
        assert_eq!(out, [0, 0, 0, 127, 127]);

        // The shift is arithmetic, so a small negative stays negative and clamps to zero.
        clipped_relu(&[-1], 6, &mut out[..1]);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn sqr_clipped_relu_is_symmetric_and_saturates() {
        let mut out = [0u8; 2];
        sqr_clipped_relu(&[1000, -1000], 6, &mut out);
        assert_eq!(out[0], out[1], "squaring makes the sign irrelevant");
        // (1000^2) >> 19 = 1000000 >> 19 = 1
        assert_eq!(out[0], 1);

        sqr_clipped_relu(&[1 << 20], 6, &mut out[..1]);
        assert_eq!(out[0], 127, "saturates rather than wrapping");
        // The i64 intermediate is what makes that true: i32 would overflow at 2^16.
        sqr_clipped_relu(&[i32::MAX], 6, &mut out[..1]);
        assert_eq!(out[0], 127);
    }

    #[test]
    fn zero_is_the_fixed_point_of_both_activations() {
        let mut out = [1u8; 1];
        clipped_relu(&[0], 6, &mut out);
        assert_eq!(out[0], 0);
        sqr_clipped_relu(&[0], 6, &mut out);
        assert_eq!(out[0], 0);
    }
}
