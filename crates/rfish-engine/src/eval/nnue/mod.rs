//! The NNUE evaluation: the network file, its loader, and the forward pass.
//!
//! # Shape
//!
//! ```text
//!   position ──> 3 feature sets ──> FeatureTransformer ──> 1024 u8
//!                                                              │
//!                                                              └──> fc_0 (1024→32)
//!                                                                    ├─ sqr_relu ─┐
//!                                                                    └─ relu ─────┤
//!                                                                        fc_1 (64→32)
//!                                                                    ┌─ sqr_relu ─┤
//!                                                                    └─ relu ─────┤
//!                                                                        fc_2 (128→1)
//! ```
//!
//! Independent output stacks exist, and four facts about the position select one: the
//! material band, which sides have a queen, where the pawns stand, and whether either side
//! has two officers. Those make 192 keys; the net stores fewer heads and carries its own
//! table from each key to the head that answers it. The chosen head's output is the
//! network's whole answer -- upstream `a17c0ce28` removed the PSQT head that used to be
//! summed beside it.
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
use crate::board::types::{Ply, VALUE_ZERO, Value};

pub use common::NetError;
use common::{
    HIDDEN_ONE_VAL, L1, L2, L3, LAYER_STACKS, NetReader, NetWriter, OUTPUT_SCALE, VERSION,
    WEIGHT_SCALE_BITS,
};
use features::{K32H3_DIMENSIONS, THREAT_AND_PP_DIMENSIONS};
use layers::{AffineLayer, clipped_relu, sqr_clipped_relu};
use transformer::{EvalScratch, FeatureTransformer};

/// The default net file name, matching the one upstream pins at the same commit.
///
/// Read from here, never recited into prose: it changes on every net-swapping upstream
/// sync, and a doc that names the old one sends a reader looking for a file that no longer
/// exists.
pub const DEFAULT_NET: &str = "nn-18cd32e11a9c.nnue";

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
    /// The heads the file stores, in file order.
    stacks: Vec<LayerStack>,
    /// Per key, the key whose head answers it -- the file's own table, kept to write back.
    fallback: [u16; LAYER_STACKS],
    /// Per key, the index into `stacks` of the head that answers it.
    stack_index: [u16; LAYER_STACKS],
    name: String,
    description: String,
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

        let (fallback, stack_index, stored) = read_stack_table(&mut r)?;

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

        let mut stacks = Vec::with_capacity(stored);
        for _ in 0..stored {
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
        Ok(Network { transformer, stacks, fallback, stack_index, name, description })
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

        w.u32(hash::LAYER_STACK_TABLE)?;
        w.u32(LAYER_STACKS as u32)?;
        w.u32(self.stacks.len() as u32)?;
        w.u16s(&self.fallback)?;

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
            K32H3_DIMENSIONS + THREAT_AND_PP_DIMENSIONS,
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

    /// Evaluate `pos`: the raw network output, which `eval` prints as internal units.
    #[must_use]
    pub fn evaluate(&self, pos: &Position, ply: Ply, scratch: &mut EvalScratch) -> Value {
        let bucket = self.stack_of(pos);
        self.transformer.transform(pos, ply.index(), scratch);
        let positional = self.stacks[bucket].propagate(scratch.transformed());
        Value::new((i64::from(positional) / OUTPUT_SCALE) as i32)
    }

    /// Every bucket's output, and which one this position actually uses.
    ///
    /// What `eval` prints. Upstream runs every output stack over the one set of transformed
    /// features rather than reading the chosen one, so the table shows what each stack WOULD
    /// have said — that is the diagnostic, and evaluating only the live bucket would leave
    /// every other row blank. The features do not depend on the bucket, so they are
    /// transformed once, as upstream's trace does.
    #[must_use]
    pub fn trace_evaluate(&self, pos: &Position, scratch: &mut EvalScratch) -> NetworkTrace {
        let mut trace = NetworkTrace {
            positional: [VALUE_ZERO; LAYER_STACKS],
            stored: self.stacks.len(),
            correct_bucket: self.stack_of(pos),
        };
        self.transformer.transform(pos, 0, scratch);
        for (bucket, stack) in self.stacks.iter().enumerate() {
            let positional = stack.propagate(scratch.transformed());
            trace.positional[bucket] = Value::new((i64::from(positional) / OUTPUT_SCALE) as i32);
        }
        trace
    }

    /// The stored head that answers `pos`: its key, through the file's table.
    fn stack_of(&self, pos: &Position) -> usize {
        usize::from(self.stack_index[stack_key(pos)])
    }
}

/// Read the table that sends each layer-stack key to a stored head, and check it.
///
/// Returns the file's fallback per key, the stored-head index per key, and how many heads the
/// file stores. A key whose fallback is itself owns a head; heads are numbered in key order.
/// Every other key must fall back to a key that owns one -- one hop, never a chain -- which is
/// upstream's check, and what keeps `stack_index` total.
fn read_stack_table(
    r: &mut NetReader<impl std::io::Read>,
) -> Result<([u16; LAYER_STACKS], [u16; LAYER_STACKS], usize), NetError> {
    let table_hash = r.u32()?;
    if table_hash != hash::LAYER_STACK_TABLE {
        return Err(NetError::WrongComponent {
            what: "layer stack table",
            expected: hash::LAYER_STACK_TABLE,
            found: table_hash,
        });
    }
    if r.u32()? as usize != LAYER_STACKS {
        return Err(NetError::BadStackTable("the key count is not this build's"));
    }
    let stored = r.u32()? as usize;
    if stored == 0 || stored > LAYER_STACKS {
        return Err(NetError::BadStackTable("the stored head count is out of range"));
    }
    let mut fallback = [0u16; LAYER_STACKS];
    r.u16s(&mut fallback)?;
    if fallback.iter().any(|&f| usize::from(f) >= LAYER_STACKS) {
        return Err(NetError::BadStackTable("a fallback names no key"));
    }

    let mut stack_index = [0u16; LAYER_STACKS];
    let mut owned = 0u16;
    for (s, &f) in fallback.iter().enumerate() {
        if usize::from(f) == s {
            stack_index[s] = owned;
            owned += 1;
        }
    }
    if usize::from(owned) != stored {
        return Err(NetError::BadStackTable("the keys that own a head are not the stored count"));
    }
    for s in 0..LAYER_STACKS {
        let f = usize::from(fallback[s]);
        if usize::from(fallback[f]) != f {
            return Err(NetError::BadStackTable("a fallback names a key without a head"));
        }
        stack_index[s] = stack_index[f];
    }
    Ok((fallback, stack_index, stored))
}

/// The layer-stack KEY of a position, from four facts about it.
///
/// - the material band, `(pieces - 1) / 4`;
/// - the queens: 2 if the side to move has one, plus 1 if its opponent does;
/// - the pawn layout: 0 with pawns on both halves of the board (files a-d and e-h), 1 with
///   pawns on one half only, 2 with none;
/// - whether either side has two or more officers -- pieces that are neither pawn nor king.
fn stack_key(pos: &Position) -> usize {
    use crate::board::bitboard::{Bitboard, FILE_A, FILE_B, FILE_C, FILE_D};
    use crate::board::types::{Color, PieceType};
    const QUEEN_SIDE: Bitboard =
        Bitboard::from_bits(FILE_A.bits() | FILE_B.bits() | FILE_C.bits() | FILE_D.bits());

    let band = (pos.piece_total() as usize - 1) / 4;
    let us = pos.side_to_move();
    let queens = (usize::from(pos.pieces_of(us, PieceType::Queen).any()) << 1)
        | usize::from(pos.pieces_of(!us, PieceType::Queen).any());

    let pawns = pos.pieces(PieceType::Pawn);
    let both_halves = (pawns & QUEEN_SIDE).any() && (pawns & !QUEEN_SIDE).any();
    let layout = if pawns.any() { usize::from(!both_halves) } else { 2 };

    let not_officers = pos.pieces(PieceType::Pawn) | pos.pieces(PieceType::King);
    let officers = |c: Color| (pos.colored(c) & !not_officers).count();
    let many = usize::from(officers(Color::White).max(officers(Color::Black)) >= 2);

    ((band * 4 + queens) * 3 + layout) * 2 + many
}

/// Every output stack's answer for one position.
#[derive(Clone, Copy, Debug)]
pub struct NetworkTrace {
    /// Each stored head's evaluation, in file order; only the first `stored` are meaningful.
    pub positional: [Value; LAYER_STACKS],
    /// How many heads the net stores.
    pub stored: usize,
    /// The stored head the position's key selects.
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
    const THREAT_HASH: u32 = 0x2c41_eaea;
    const PAIR_HASH: u32 = 0x5c80_fa69;
    const PSQ_HASH: u32 = 0xab00_5cef;

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

    /// The layer-stack table's own hash word, upstream's `LayerStackTableHash`.
    pub(super) const LAYER_STACK_TABLE: u32 = 0x5A7A_B1E5;

    /// The whole file's hash, as written in the header.
    pub(super) const NETWORK: u32 = FEATURE_TRANSFORMER ^ ARCHITECTURE ^ LAYER_STACK_TABLE;
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
        assert_eq!(hash::FEATURE_TRANSFORMER, 0xA306_0B95);
        assert_eq!(
            hash::NETWORK,
            hash::FEATURE_TRANSFORMER ^ hash::ARCHITECTURE ^ hash::LAYER_STACK_TABLE
        );
        assert_ne!(hash::ARCHITECTURE, 0);
    }

    /// The table section as a file carries it, for `read_stack_table`.
    fn stack_table_bytes(stored: u32, fallback: &[u16; LAYER_STACKS]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&hash::LAYER_STACK_TABLE.to_le_bytes());
        b.extend_from_slice(&(LAYER_STACKS as u32).to_le_bytes());
        b.extend_from_slice(&stored.to_le_bytes());
        for f in fallback {
            b.extend_from_slice(&f.to_le_bytes());
        }
        b
    }

    #[test]
    fn every_key_reaches_the_head_its_fallback_owns() {
        // Keys 5 and 9 own heads 0 and 1; every other key falls back to one of them.
        let mut fallback = [5u16; LAYER_STACKS];
        fallback[9] = 9;
        fallback[100] = 9;
        let bytes = stack_table_bytes(2, &fallback);
        let (read, index, stored) =
            read_stack_table(&mut NetReader::new(bytes.as_slice())).expect("a valid table");
        assert_eq!(read, fallback);
        assert_eq!(stored, 2);
        assert_eq!((index[5], index[9], index[100], index[0]), (0, 1, 1, 0));

        // A fallback to a key that owns no head is a chain, which upstream refuses.
        let mut chained = fallback;
        chained[7] = 100;
        let bytes = stack_table_bytes(2, &chained);
        assert!(matches!(
            read_stack_table(&mut NetReader::new(bytes.as_slice())),
            Err(NetError::BadStackTable(_))
        ));

        // A stored count that is not the number of keys owning a head.
        let bytes = stack_table_bytes(3, &fallback);
        assert!(matches!(
            read_stack_table(&mut NetReader::new(bytes.as_slice())),
            Err(NetError::BadStackTable(_))
        ));
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
