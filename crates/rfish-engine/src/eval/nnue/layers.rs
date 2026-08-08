//! The fully connected layers and their activations.
//!
//! Everything here is integer arithmetic on a fixed quantisation. The shifts are not
//! rounding conveniences — they are where the quantisation scale changes, and moving one by
//! a bit changes every score the network produces.
//!
//! # No intrinsics
//!
//! Upstream writes these as hand-vectorised kernels behind one `#if` per instruction set.
//! Every `std::arch` intrinsic is an `unsafe fn`, so that route is closed here. `std::simd`
//! is not: it needs no `unsafe` block, which is why the dated nightly pin buys it without
//! touching `forbid(unsafe_code)`. The kernels below are written in it where a measurement
//! says it pays, and left as ordinary loops for LLVM to vectorise under `-C target-cpu`
//! where it does not.
//!
//! The arithmetic is upstream's SCALAR fallback either way, which upstream keeps precisely
//! so that the vector paths have something to be bit-identical to.
//!
//! Golden: `Stockfish/src/nnue/layers/affine_transform.h`, `clipped_relu.h`,
//! `sqr_clipped_relu.h`.

use core::simd::Simd;
use core::simd::cmp::{SimdOrd, SimdPartialEq};
use core::simd::num::{SimdInt, SimdUint};

use super::common::{Aligned, NetError, NetReader, NetWriter, ceil_to_multiple};

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
    biases: Aligned<i32>,
    weights: Aligned<i8>,
    /// The same weights in the order [`AffineLayer::propagate_sparse`] walks them, or empty
    /// when that path is not enabled for this layer. See [`AffineLayer::enable_sparse`].
    sparse: Aligned<i8>,
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
            biases: Aligned::new(output_dims),
            weights: Aligned::new(output_dims * padded_dims),
            sparse: Aligned::new(input_dims * output_dims),
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
    /// Walked by COLUMN, over the transposed copy, rather than a row at a time. A row walk
    /// ends every output in a horizontal reduction — log2(N) shuffle-and-add pairs whose
    /// only product is one lane — and repeats that N times. A column walk holds the whole
    /// output row in one accumulator and never reduces at all; the products land in the lane
    /// they belong to. The sum is the same integers in a different order, which for integers
    /// is the same sum.
    pub fn propagate<const N: usize>(&self, input: &[u8], output: &mut [i32; N]) {
        debug_assert!(input.len() >= self.input_dims);
        debug_assert_eq!(N, self.output_dims);
        let mut acc = Simd::<i32, N>::from_slice(&self.biases);
        let blocks = self.sparse.as_chunks::<N>().0;
        for (block, &x) in blocks.iter().zip(input[..self.input_dims].iter()) {
            let w: Simd<i8, N> = Simd::from_array(*block);
            acc += w.cast::<i32>() * Simd::splat(i32::from(x));
        }
        *output = acc.to_array();
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
    /// are skipping zeros against different cost models: upstream's four-way byte dot costs
    /// it the same for a group of four as for one, so the coarser test comes free, while
    /// rfish pays per multiply and only cares how much work a test can SKIP.
    ///
    /// **The density this argument rests on is 14.8%, not the ~40% this comment used to
    /// claim.** Counted rather than estimated: 9,551,893 non-zero inputs over 62,975
    /// evaluations of `bench 16 1 8`, which is 151.7 of 1024. The old figure was never
    /// measured and it was nearly three times too high.
    ///
    /// The conclusion survives the correction, and is worth restating with the right number.
    /// A group of four is skippable only when all four are zero, so at 14.8% it survives
    /// `1 - 0.852^4` = **47%** of the time: 121 group visits replace 151.7 input visits, a
    /// fifth fewer, and each would have to do four inputs' arithmetic. That is a win only
    /// with an instruction that dots four bytes at once, and it is a loss without one —
    /// measured, `bench 16 1 8` at `nehalem`: groups of four 5,783,523,617 instructions,
    /// groups of one 4,769,344,411.
    ///
    /// **Disassembled, the per-input loop is now twenty instructions and there is nothing
    /// left in it that is not arithmetic:** `tzcnt`/`blsr` to walk the mask, a `movzbl` and
    /// a broadcast for the input, four `vpmovsxbd`, four `vpmaddwd`, four `vpaddd`, one
    /// `shl` for the row address and the loop branch. No bounds test, no composite index,
    /// no spill. What separates it from upstream is that those four `vpmaddwd` cover one
    /// input where `vpmaddubsw` covers four, and that is the instruction — not the shape.
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
        // The weight rows chunked the SAME way the inputs are, so the bitmask walk indexes a
        // fixed-size array rather than a runtime-length slice.
        //
        // `blocks[c * SCAN + lane]` cost 66.9M on this one line and 19.1M on the index that
        // fed it, against 9.6M for the multiply-accumulate it existed to reach: `blocks`
        // takes its length from `self.sparse` at run time, so every visit paid a bounds test
        // and a scaled address. Through `&[[i8; N]; SCAN]` the bound is a CONSTANT and
        // `lane` came out of `trailing_zeros()` on a `u64`, so it cannot reach it -- LLVM
        // drops the test, and the row address is one displacement off a base the loop
        // already holds. Same shape, and the same reason, as the weight rows in
        // `FeatureTransformer::fold_into`.
        let (row_scans, row_tail) = blocks.as_chunks::<SCAN>();

        for (chunk, rows) in scans.iter().zip(row_scans.iter()) {
            // One compare answers SCAN inputs, and the bitmask walk visits only the inputs
            // that are actually non-zero -- no branch is taken for the ones that are not.
            let mut nnz = Simd::<u8, SCAN>::from_array(*chunk).simd_ne(Simd::splat(0)).to_bitmask();
            while nnz != 0 {
                let lane = nnz.trailing_zeros() as usize;
                nnz &= nnz - 1;
                let w: Simd<i8, N> = Simd::from_array(rows[lane]);
                acc += w.cast::<i32>() * Simd::splat(i32::from(chunk[lane]));
            }
        }
        for (&x, row) in tail.iter().zip(row_tail.iter()) {
            if x != 0 {
                let w: Simd<i8, N> = Simd::from_array(*row);
                acc += w.cast::<i32>() * Simd::splat(i32::from(x));
            }
        }

        *output = acc.to_array();
    }

    /// `bias + sum_i weight[i] * input[i]`, for the layer whose output width is ONE.
    ///
    /// [`AffineLayer::propagate`] is generic over the output width and the last layer is
    /// 128 -> 1, so it instantiated at `Simd<i32, 1>` -- a one-lane vector, which is a
    /// scalar. What LLVM then made of it is worth reading, because it is not the obvious
    /// failure: it widened the loop to `xmm` and put a HORIZONTAL REDUCTION inside it.
    ///
    /// ```text
    ///   vpmovzxbw / vpmovsxbw     8 inputs, 8 weights, widened to i16
    ///   vpmaddwd                  4 partial sums
    ///   vpshufd / vpaddd          \  reduce four lanes to one, every iteration,
    ///   vpshufd / vpaddd          /  because the accumulator in the source is a scalar
    ///   vmovd / add
    /// ```
    ///
    /// Twelve instructions per eight inputs, and the shuffle pair serialises the loop on a
    /// dependency chain no wider accumulator would have. Sixteen `i32` lanes carried across
    /// the whole walk and reduced ONCE at the end is the same arithmetic without either.
    ///
    /// The reassociation is exact rather than approximate: every product is a `u8` in
    /// `[0, 127]` times an `i8`, so it is bounded by 16,256 in magnitude, and 128 of them
    /// plus the bias cannot leave `i32`. Integer addition is associative, so the order the
    /// lanes are summed in cannot change the total. `cargo xtask nnue-check` pins it against
    /// upstream and the bench signature pins it against the whole tree.
    ///
    /// ../mcfish gives its own one-output layer a dedicated contiguous dot for exactly this
    /// reason, and says so in `nnue_affine.c`: the generic path "loads four bytes at a time,
    /// sign/zero-extends each quad to int32, repacks to int16 and reduces" for what one wide
    /// instruction does at a stride.
    #[must_use]
    pub fn propagate_one(&self, input: &[u8]) -> i32 {
        // At an output width of one the transposed copy IS the weight row, contiguously:
        // `sparse[i * 1 + 0] == weights[0 * padded_dims + i]`. So this is a plain dot over
        // two byte arrays, and needs no gather and no stride.
        const LANES: usize = 16;
        debug_assert_eq!(self.output_dims, 1);
        debug_assert!(input.len() >= self.input_dims);

        let (w_blocks, w_tail) = self.sparse[..self.input_dims].as_chunks::<LANES>();
        let (i_blocks, i_tail) = input[..self.input_dims].as_chunks::<LANES>();
        let mut acc = Simd::<i32, LANES>::splat(0);
        for (wb, ib) in w_blocks.iter().zip(i_blocks.iter()) {
            let w: Simd<i8, LANES> = Simd::from_array(*wb);
            let x: Simd<u8, LANES> = Simd::from_array(*ib);
            acc += w.cast::<i32>() * x.cast::<i32>();
        }
        let mut sum = self.biases[0] + acc.reduce_sum();
        for (&w, &x) in w_tail.iter().zip(i_tail.iter()) {
            sum += i32::from(w) * i32::from(x);
        }
        sum
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

/// The lane count both activations narrow in one step.
///
/// Sixteen `i32` is two AVX2 registers in and one byte store out, which is the widest
/// narrowing the tier can retire without a second pack. Every caller passes a multiple of
/// it, so the scalar tail below exists only to keep the function total.
const RELU_LANES: usize = 16;

/// `output[i] = clamp(input[i] >> shift, 0, 127)`.
///
/// The shift is where the accumulator's quantisation scale drops back to the activation's.
pub fn clipped_relu(input: &[i32], shift: u32, output: &mut [u8]) {
    debug_assert_eq!(input.len(), output.len());
    let (blocks, tail) = input.as_chunks::<RELU_LANES>();
    let (outs, out_tail) = output.as_chunks_mut::<RELU_LANES>();
    for (o, block) in outs.iter_mut().zip(blocks.iter()) {
        let x = Simd::<i32, RELU_LANES>::from_array(*block) >> Simd::splat(shift as i32);
        *o = x.simd_clamp(Simd::splat(0), Simd::splat(127)).cast::<u8>().to_array();
    }
    for (o, &x) in out_tail.iter_mut().zip(tail.iter()) {
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
///
/// The vector body squares in `i32` after saturating the input into `i16`, where the scalar
/// tail squares in `i64`. The two agree for every input this network produces, and the
/// reason is the cap rather than the arithmetic: an operand outside `i16` has a square of at
/// least `2^30`, and `2^30` shifted right by the largest shift a caller passes is still far
/// above 127, so both forms saturate. Inside `i16` nothing is lost — the largest square is
/// `2^30`, which is an `i32`. The assert below is what pins the "largest shift a caller
/// passes"; widen the clamp, not the shift, if a future layer breaks it.
pub fn sqr_clipped_relu(input: &[i32], shift: u32, output: &mut [u8]) {
    debug_assert_eq!(input.len(), output.len());
    let total = 2 * shift + 7;
    debug_assert!(total <= 23, "an i16-saturated square must still exceed the 127 cap");
    let (blocks, tail) = input.as_chunks::<RELU_LANES>();
    let (outs, out_tail) = output.as_chunks_mut::<RELU_LANES>();
    for (o, block) in outs.iter_mut().zip(blocks.iter()) {
        let x = Simd::<i32, RELU_LANES>::from_array(*block)
            .simd_clamp(Simd::splat(-32768), Simd::splat(32767));
        let q = (x * x) >> Simd::splat(total as i32);
        *o = q.simd_min(Simd::splat(127)).cast::<u8>().to_array();
    }
    for (o, &x) in out_tail.iter_mut().zip(tail.iter()) {
        let squared = i64::from(x) * i64::from(x);
        *o = (squared >> total).min(127) as u8;
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
        l.biases = Aligned::from_slice(&[10, -10]);
        // Row 0 is [1, 2, 3, 4] then padding; row 1 is [-1, -1, -1, -1].
        l.weights = Aligned::new(2 * 32);
        l.weights[0..4].copy_from_slice(&[1, 2, 3, 4]);
        l.weights[32..36].copy_from_slice(&[-1, -1, -1, -1]);
        l.rebuild_sparse();

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
        assert_eq!(l.biases.as_slice(), &[7, -7]);
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
