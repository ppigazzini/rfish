//! The NNUE evaluation: the network file, its loader, and the forward pass.
//!
//! # Shape
//!
//! ```text
//!   position ──> 3 feature sets ──> FeatureTransformer ──> 1024 u8
//!                                        │                     │
//!                                        └──> PSQT head        └──> fc_0 (1024→32)
//!                                             (8 buckets)            ├─ sqr_relu ─┐
//!                                                                    └─ relu ─────┤
//!                                                                        fc_1 (64→32)
//!                                                                    ┌─ sqr_relu ─┤
//!                                                                    └─ relu ─────┤
//!                                                                        fc_2 (128→1)
//! ```
//!
//! Eight independent output heads exist; the material count selects one. Both the PSQT
//! score and the positional score come back, because the search blends them by their
//! disagreement — a position the two heads argue about is one to be less confident in.
//!
//! # No embedded net
//!
//! Upstream can embed a network into the binary. rfish does not, and will not: the bench
//! anchor is a property of a file fetched separately, and embedding it would make the
//! anchor look like a property of this repository instead.
//!
//! Golden: `Stockfish/src/nnue/network.cpp`, `nnue_architecture.h`, `evaluate.cpp`.

pub mod common;
pub mod features;
pub mod layers;
pub mod transformer;

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use crate::board::position::Position;
use crate::board::types::{Ply, Value};

pub use common::NetError;
use common::{
    HIDDEN_ONE_VAL, L1, L2, L3, LAYER_STACKS, NetReader, NetWriter, OUTPUT_SCALE, VERSION,
    WEIGHT_SCALE_BITS,
};
use features::{HALFKA_DIMENSIONS, THREAT_AND_PP_DIMENSIONS};
use layers::{AffineLayer, clipped_relu, sqr_clipped_relu};
use transformer::{EvalScratch, FeatureTransformer};

/// The default net file name, matching the one upstream pins at the same commit.
///
/// Read from here, never recited into prose: it changes on every net-swapping upstream
/// sync, and a doc that names the old one sends a reader looking for a file that no longer
/// exists.
pub const DEFAULT_NET: &str = "nn-ab28990d4ea3.nnue";

/// The concatenated activation buffer the two hidden layers share.
const CONCAT: usize = L2 * 2 + L3 * 2;

/// One output head.
#[derive(Debug)]
#[allow(clippy::struct_field_names)]
struct LayerStack {
    fc_0: AffineLayer,
    fc_1: AffineLayer,
    fc_2: AffineLayer,
}

impl LayerStack {
    /// How many bytes of weights and biases this stack holds.
    fn weight_bytes(&self) -> usize {
        self.fc_0.weight_bytes() + self.fc_1.weight_bytes() + self.fc_2.weight_bytes()
    }

    fn new() -> LayerStack {
        LayerStack {
            fc_0: AffineLayer::new(L1, L2),
            fc_1: AffineLayer::new(L2 * 2, L3),
            fc_2: AffineLayer::new(CONCAT, 1),
        }
    }

    fn read(&mut self, r: &mut NetReader<impl std::io::Read>) -> Result<(), NetError> {
        // The two activation layers have no parameters, so they are absent from the file
        // even though the architecture lists them between the affine layers.
        self.fc_0.read(r)?;
        self.fc_1.read(r)?;
        self.fc_2.read(r)?;
        Ok(())
    }

    fn write(&self, w: &mut NetWriter<impl std::io::Write>) -> Result<(), NetError> {
        self.fc_0.write(w)?;
        self.fc_1.write(w)?;
        self.fc_2.write(w)?;
        Ok(())
    }

    /// The positional score for one set of transformed features.
    fn propagate(&self, transformed: &[u8]) -> i32 {
        let mut fc_0_out = [0i32; L2];
        let mut concat = [0u8; CONCAT];
        let mut fc_1_out = [0i32; L3];

        self.fc_0.propagate_sparse(transformed, &mut fc_0_out);
        // The squared and the plain activation of the SAME accumulator are concatenated:
        // the layer that follows sees both, which is where the network's non-linearity in
        // this stage comes from.
        sqr_clipped_relu(&fc_0_out, WEIGHT_SCALE_BITS + 1, &mut concat[..L2]);
        clipped_relu(&fc_0_out, WEIGHT_SCALE_BITS + 1, &mut concat[L2..L2 * 2]);

        self.fc_1.propagate(&concat[..L2 * 2], &mut fc_1_out);
        sqr_clipped_relu(&fc_1_out, WEIGHT_SCALE_BITS, &mut concat[L2 * 2..L2 * 2 + L3]);
        clipped_relu(&fc_1_out, WEIGHT_SCALE_BITS, &mut concat[L2 * 2 + L3..]);

        // The one-output layer has its own dot: through the generic path it instantiated at
        // a one-lane vector and LLVM put a horizontal reduction inside the loop. See
        // [`AffineLayer::propagate_one`].
        let fc_2_out = self.fc_2.propagate_one(&concat);

        // A skip connection: the last two outputs of the FIRST layer bypass everything and
        // are added as a difference. Dropping it costs the network its linear term.
        let fwd = fc_2_out + (fc_0_out[L2 - 2] - fc_0_out[L2 - 1]);

        // `fwd` is quantised so that 1.0 is `HIDDEN_ONE_VAL * 2^WEIGHT_SCALE_BITS * 2`, and
        // the caller wants 1.0 to be `600 * OUTPUT_SCALE`. The i64 is what makes the
        // product safe; the division truncates toward zero, as upstream's does.
        let multiplier = 600 * OUTPUT_SCALE;
        let denominator = HIDDEN_ONE_VAL * (1 << WEIGHT_SCALE_BITS) * 2;
        ((i64::from(fwd) * multiplier) / denominator) as i32
    }
}

/// A loaded network.
#[derive(Debug)]
pub struct Network {
    transformer: FeatureTransformer,
    stacks: Vec<LayerStack>,
    name: String,
    description: String,
}

/// What the network says about a position, before the search blends the two.
#[derive(Clone, Copy, Debug)]
pub struct NetworkOutput {
    /// The material-and-placement head.
    pub psqt: Value,
    /// The learned positional head.
    pub positional: Value,
}

impl Network {
    /// Read a net from `path`.
    ///
    /// Every hash in the file is checked before any weight is used. A net for a different
    /// architecture would otherwise load without complaint and evaluate nonsense, which is
    /// the failure mode that is hardest to notice: the engine plays, just badly.
    pub fn load(path: &Path) -> Result<Network, NetError> {
        let mut r = NetReader::new(BufReader::with_capacity(1 << 20, File::open(path)?));

        let version = r.u32()?;
        if version != VERSION {
            return Err(NetError::NotANet);
        }
        let file_hash = r.u32()?;
        let desc_len = r.u32()? as usize;
        if desc_len > 1 << 16 {
            return Err(NetError::Truncated);
        }
        let mut desc = vec![0u8; desc_len];
        r.read_exact(&mut desc)?;
        let description = String::from_utf8_lossy(&desc).into_owned();

        let expected = hash::NETWORK;
        if file_hash != expected {
            return Err(NetError::WrongArchitecture { expected, found: file_hash });
        }

        let mut transformer = FeatureTransformer::new();
        let ft_hash = r.u32()?;
        if ft_hash != hash::FEATURE_TRANSFORMER {
            return Err(NetError::WrongComponent {
                what: "feature transformer",
                expected: hash::FEATURE_TRANSFORMER,
                found: ft_hash,
            });
        }
        transformer.read(&mut r)?;

        let mut stacks = Vec::with_capacity(LAYER_STACKS);
        for _ in 0..LAYER_STACKS {
            let stack_hash = r.u32()?;
            if stack_hash != hash::ARCHITECTURE {
                return Err(NetError::WrongComponent {
                    what: "layer stack",
                    expected: hash::ARCHITECTURE,
                    found: stack_hash,
                });
            }
            let mut stack = LayerStack::new();
            stack.read(&mut r)?;
            stacks.push(stack);
        }

        // Upstream requires the stream to be exhausted. A net with trailing bytes has a
        // structure this build does not agree with, even when every hash matched.
        if !r.at_end() {
            return Err(NetError::TrailingData);
        }

        let name = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned());
        Ok(Network { transformer, stacks, name, description })
    }

    /// Write this network back out in the format [`Network::load`] reads.
    ///
    /// The point is not to produce a new net — it is that a net can be read and written
    /// without changing, which makes the format code check itself. Every hash is recomputed
    /// from THIS build's constants rather than copied from the file that was loaded, so a
    /// saved net asserts the architecture the saving binary actually implements.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the file cannot be created or written.
    pub fn save(&self, path: &Path) -> Result<(), NetError> {
        let mut w = NetWriter::new(BufWriter::with_capacity(1 << 20, File::create(path)?));

        w.u32(VERSION)?;
        w.u32(hash::NETWORK)?;
        let desc = self.description.as_bytes();
        w.u32(u32::try_from(desc.len()).map_err(|_| NetError::Truncated)?)?;
        w.write_all(desc)?;

        w.u32(hash::FEATURE_TRANSFORMER)?;
        self.transformer.write(&mut w)?;

        for stack in &self.stacks {
            w.u32(hash::ARCHITECTURE)?;
            stack.write(&mut w)?;
        }

        w.flush()
    }

    /// The file name, for the UCI `EvalFile` report.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Upstream's architecture summary: the resident size in MiB, then the layer widths.
    ///
    /// Upstream builds this from `sizeof` its own structures. rfish computes it from the
    /// ARCHITECTURE instead -- the weight blocks the file describes -- because a Rust
    /// struct's padding is not C++'s and the number has to be the same one upstream prints,
    /// not the same expression.
    #[must_use]
    pub fn arch_summary(&self) -> String {
        let bytes = self.transformer.weight_bytes()
            + self.stacks.iter().map(LayerStack::weight_bytes).sum::<usize>();
        format!(
            "{}MiB, ({}, {}, {}, {}, 1)",
            bytes / (1024 * 1024),
            HALFKA_DIMENSIONS + THREAT_AND_PP_DIMENSIONS,
            L1,
            L2,
            L3
        )
    }

    /// The free-text architecture description the trainer wrote.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Evaluate `pos`.
    ///
    /// The bucket is chosen by material: `(pieces - 1) / 4`, so an endgame and a full board
    /// get different heads. Both scores come back separately, because the search blends
    /// them by their disagreement.
    #[must_use]
    pub fn evaluate(&self, pos: &Position, ply: Ply, scratch: &mut EvalScratch) -> NetworkOutput {
        let bucket = (pos.piece_total() as usize - 1) / 4;
        let psqt = self.transformer.transform(pos, bucket, ply.index(), scratch);
        let positional = self.stacks[bucket].propagate(scratch.transformed());
        NetworkOutput {
            psqt: (i64::from(psqt) / OUTPUT_SCALE) as Value,
            positional: (i64::from(positional) / OUTPUT_SCALE) as Value,
        }
    }

    /// Every bucket's output, and which one this position actually uses.
    ///
    /// What `eval` prints. Upstream runs the whole network once per bucket rather than
    /// reading the chosen one, so the table shows what each head WOULD have said — that is
    /// the diagnostic, and evaluating only the live bucket would leave seven rows blank.
    #[must_use]
    pub fn trace_evaluate(&self, pos: &Position, scratch: &mut EvalScratch) -> NetworkTrace {
        let mut trace = NetworkTrace {
            psqt: [0; LAYER_STACKS],
            positional: [0; LAYER_STACKS],
            correct_bucket: (pos.piece_total() as usize - 1) / 4,
        };
        for bucket in 0..LAYER_STACKS {
            let psqt = self.transformer.transform(pos, bucket, 0, scratch);
            let positional = self.stacks[bucket].propagate(scratch.transformed());
            trace.psqt[bucket] = (i64::from(psqt) / OUTPUT_SCALE) as Value;
            trace.positional[bucket] = (i64::from(positional) / OUTPUT_SCALE) as Value;
        }
        trace
    }
}

/// Every output head's answer for one position.
#[derive(Clone, Copy, Debug)]
pub struct NetworkTrace {
    /// The material head, per bucket.
    pub psqt: [Value; LAYER_STACKS],
    /// The positional head, per bucket.
    pub positional: [Value; LAYER_STACKS],
    /// The bucket the piece count selects.
    pub correct_bucket: usize,
}

/// The structural hashes embedded in the file.
///
/// Each is computed the way upstream computes it, from the layer shapes alone. They are the
/// only thing standing between a mismatched net and a silently wrong evaluation, so they
/// are derived here rather than pasted as literals — a literal would not follow a shape
/// change, and a shape change is exactly when the check has to fire.
mod hash {
    use super::common::{L1, L2, L3};

    /// The hash each feature set carries, from upstream.
    const THREAT_HASH: u32 = 0x2e6b_9d04;
    const PAIR_HASH: u32 = 0x86f2_b1dd;
    const PSQ_HASH: u32 = 0x7f23_4cb8;

    /// Fold a list of component hashes, rotating one bit between each.
    const fn combine(hashes: [u32; 3]) -> u32 {
        let mut h: u32 = 0;
        let mut i = 0;
        while i < hashes.len() {
            h = h.rotate_left(1);
            h ^= hashes[i];
            i += 1;
        }
        h
    }

    /// An affine layer's contribution to the chain.
    const fn affine(prev: u32, output_dims: u32) -> u32 {
        let mut h: u32 = 0xCC03_DAE4;
        h = h.wrapping_add(output_dims);
        h ^= prev >> 1;
        h ^= prev << 31;
        h
    }

    /// A clipped-ReLU layer's contribution. Both activation kinds use the same constant.
    const fn relu(prev: u32) -> u32 {
        0x538D_24C7u32.wrapping_add(prev)
    }

    /// The transformer's own hash.
    pub(super) const FEATURE_TRANSFORMER: u32 =
        combine([THREAT_HASH, PAIR_HASH, PSQ_HASH]) ^ (L1 as u32 * 2);

    /// One layer stack's hash: the chain through `fc_0`, `ac_0`, `fc_1`, `ac_1`, `fc_2`.
    ///
    /// The squared activations are deliberately absent from the chain — upstream omits them
    /// because the trainer does not write them, and reproducing that omission is what makes
    /// the value match.
    pub(super) const ARCHITECTURE: u32 = {
        let h = 0xEC42_E90Du32 ^ (L1 as u32 * 2);
        let h = affine(h, L2 as u32);
        let h = relu(h);
        let h = affine(h, L3 as u32);
        let h = relu(h);
        affine(h, 1)
    };

    /// The whole file's hash, as written in the header.
    pub(super) const NETWORK: u32 = FEATURE_TRANSFORMER ^ ARCHITECTURE;
}

/// Where to look for a net named `name`.
///
/// In order: the working directory, the `resources/` directory beside it, and the directory
/// holding the executable. That is upstream's search order plus `resources/`, which is
/// where every gate in this repository puts runtime inputs.
#[must_use]
pub fn search_paths(name: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(name), Path::new("resources").join(name)];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join(name));
        out.push(dir.join("resources").join(name));
    }
    out
}

/// Find and load a net, trying every path [`search_paths`] suggests.
///
/// Returns the first error encountered at a path that EXISTS, so "the net is corrupt" is
/// reported rather than being reduced to "no net found".
#[must_use]
pub fn find_and_load(name: &str) -> Option<Result<Network, NetError>> {
    for p in search_paths(name) {
        if p.is_file() {
            return Some(Network::load(&p));
        }
    }
    None
}

/// A reusable evaluation scratchpad. One per search thread.
pub type Scratch = EvalScratch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structural_hashes_match_the_ones_upstream_computes() {
        // Derived from the layer shapes; if a shape moves these move with it, which is the
        // whole point. The values are recorded so a refactor that silently changes the
        // derivation is caught here rather than by a net that will not load.
        assert_eq!(hash::FEATURE_TRANSFORMER, 0xCB68_5313);
        assert_eq!(hash::NETWORK, hash::FEATURE_TRANSFORMER ^ hash::ARCHITECTURE);
        assert_ne!(hash::ARCHITECTURE, 0);
    }

    #[test]
    fn a_non_net_is_rejected_before_anything_is_allocated() {
        let dir = std::env::temp_dir().join(format!("rfish-net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("bogus.nnue");
        std::fs::write(&path, b"not a network at all").expect("write");
        assert!(matches!(Network::load(&path), Err(NetError::NotANet)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_net_with_the_wrong_architecture_hash_is_rejected() {
        let dir = std::env::temp_dir().join(format!("rfish-net2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("wrong.nnue");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &bytes).expect("write");
        assert!(matches!(Network::load(&path), Err(NetError::WrongArchitecture { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_search_order_starts_with_the_working_directory() {
        let paths = search_paths("nn-test.nnue");
        assert_eq!(paths[0], PathBuf::from("nn-test.nnue"));
        assert_eq!(paths[1], Path::new("resources").join("nn-test.nnue"));
    }

    #[test]
    fn a_missing_net_is_not_an_error() {
        assert!(find_and_load("nn-definitely-not-present-0000.nnue").is_none());
    }

    #[test]
    fn a_zero_network_propagates_to_zero() {
        let s = LayerStack::new();
        assert_eq!(s.propagate(&[0u8; L1]), 0);
        // Every input at the maximum still gives zero when every weight is zero, which is
        // what shows the biases are the only source of a non-zero output.
        assert_eq!(s.propagate(&[127u8; L1]), 0);
    }

    #[test]
    fn the_skip_connection_reaches_the_output() {
        let mut s = LayerStack::new();
        // Bias the two outputs the skip connection reads, and nothing else.
        let mut fc0 = AffineLayer::new(L1, L2);
        let mut biases = vec![0i32; L2];
        biases[L2 - 2] = 1 << 20;
        let mut bytes = Vec::new();
        for b in &biases {
            bytes.extend_from_slice(&b.to_le_bytes());
        }
        bytes.extend_from_slice(&vec![0u8; L2 * L1]);
        fc0.read(&mut NetReader::new(bytes.as_slice())).expect("reads");
        s.fc_0 = fc0;

        // The squared activation saturates, so the skip term is what distinguishes this
        // from a network with no bias at all.
        assert_ne!(s.propagate(&[0u8; L1]), 0);
    }
}
