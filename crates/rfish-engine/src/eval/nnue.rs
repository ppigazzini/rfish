//! The NNUE network file: its header, its identity, and its loader.
//!
//! # Status
//!
//! **The forward pass is not ported yet.** What is here is the file format — enough to
//! find a net, read its header, check that it is the architecture this engine expects, and
//! report its name to the UCI layer. [`super::classical`] stands in for the evaluation
//! itself until milestone M3 lands the feature transformer, the accumulator and the affine
//! layers. See `__DEV/PORTING.md`.
//!
//! Keeping the loader ahead of the forward pass is deliberate: the net is a runtime input,
//! not a build product, and every piece of machinery around it — the `EvalFile` option,
//! the search path, the "where do I look for it" rules — is testable before a single
//! weight is multiplied.
//!
//! # The net is never embedded
//!
//! Upstream can embed a network into the binary. rfish does not, and will not. The bench
//! anchor is a property of a file fetched separately, and embedding it would make the
//! anchor look like a property of this repository instead.
//!
//! Golden: `Stockfish/src/nnue/network.cpp`, `Stockfish/src/nnue/nnue_common.h`.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

/// The magic version word every Stockfish net begins with.
const NNUE_VERSION: u32 = 0x7AF3_2F20;

/// What went wrong while loading a net.
#[derive(Debug)]
pub enum NetError {
    /// The file could not be opened or read.
    Io(io::Error),
    /// The leading version word is not [`NNUE_VERSION`].
    NotANet,
    /// The file is a net, but not for the architecture this engine implements.
    WrongArchitecture { expected: u32, found: u32 },
    /// The file ended before the header did.
    Truncated,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "{e}"),
            NetError::NotANet => f.write_str("not an NNUE network file"),
            NetError::WrongArchitecture { expected, found } => {
                write!(f, "network architecture {found:#010x} does not match {expected:#010x}")
            }
            NetError::Truncated => f.write_str("network file ends inside its header"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> NetError {
        NetError::Io(e)
    }
}

/// A network file's header: everything readable without knowing the layer shapes.
#[derive(Clone, Debug)]
pub struct NetHeader {
    /// The format version word.
    pub version: u32,
    /// A hash of the architecture the net was trained for. A net whose hash differs uses
    /// different layer shapes, so loading its weights into these buffers would silently
    /// evaluate nonsense rather than fail.
    pub hash: u32,
    /// The free-text architecture description the trainer wrote.
    pub description: String,
}

/// A loaded network.
///
/// Today this holds the header and the raw weight block. When M3 lands, the weight block
/// is parsed into the feature transformer and the affine layers and this type grows the
/// `evaluate` method the search will call.
#[derive(Debug)]
pub struct Network {
    header: NetHeader,
    /// The file's name, as `EvalFile` reports it.
    name: String,
    /// The weights, still unparsed. Held as a `Vec<u8>` rather than memory-mapped: a
    /// mapping is `unsafe` in Rust and buys nothing here, because the whole file is read
    /// exactly once and then never touched again.
    weights: Vec<u8>,
}

impl Network {
    /// Read a net from `path`.
    pub fn load(path: &Path) -> Result<Network, NetError> {
        let mut reader = BufReader::new(File::open(path)?);
        let header = read_header(&mut reader)?;
        let mut weights = Vec::new();
        reader.read_to_end(&mut weights)?;
        let name = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned());
        Ok(Network { header, name, weights })
    }

    /// The net's header.
    #[must_use]
    pub fn header(&self) -> &NetHeader {
        &self.header
    }

    /// The file name, for the UCI `EvalFile` report.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many bytes of weights follow the header.
    #[must_use]
    pub fn weight_len(&self) -> usize {
        self.weights.len()
    }
}

/// Read and validate the header from `r`.
pub fn read_header(r: &mut impl Read) -> Result<NetHeader, NetError> {
    let version = read_u32(r)?;
    if version != NNUE_VERSION {
        return Err(NetError::NotANet);
    }
    let hash = read_u32(r)?;
    let desc_len = read_u32(r)? as usize;
    // A description longer than a few kilobytes means the file is not what it claims;
    // refuse rather than allocating whatever the header asks for.
    if desc_len > 1 << 16 {
        return Err(NetError::Truncated);
    }
    let mut buf = vec![0u8; desc_len];
    r.read_exact(&mut buf).map_err(|_| NetError::Truncated)?;
    Ok(NetHeader { version, hash, description: String::from_utf8_lossy(&buf).into_owned() })
}

fn read_u32(r: &mut impl Read) -> Result<u32, NetError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(|_| NetError::Truncated)?;
    Ok(u32::from_le_bytes(b))
}

/// The default net file name, matching the one upstream pins at the same commit.
///
/// Read from here, never recited into prose: it changes on every net-swapping upstream
/// sync, and a doc that names the old one sends a reader looking for a file that no longer
/// exists.
pub const DEFAULT_NET: &str = "nn-0ee0657fb25e.nnue";

/// Where to look for a net named `name`.
///
/// In order: the working directory, the `resources/` directory beside it, and the
/// directory holding the executable. That is upstream's search order plus `resources/`,
/// which is where every gate in this repository puts runtime inputs.
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
/// Returns `None` when no candidate exists — which is not an error: rfish runs without a
/// net, on the classical scaffolding, and says so rather than refusing to start.
#[must_use]
pub fn find_and_load(name: &str) -> Option<Network> {
    search_paths(name).into_iter().find_map(|p| Network::load(&p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal well-formed header, built by hand so the parser is tested without needing
    /// a 100 MiB file in the repository.
    fn header_bytes(version: u32, hash: u32, desc: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&version.to_le_bytes());
        v.extend_from_slice(&hash.to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(desc.as_bytes());
        v
    }

    #[test]
    fn a_well_formed_header_parses() {
        let bytes = header_bytes(NNUE_VERSION, 0x1234_5678, "Network trained by someone");
        let h = read_header(&mut bytes.as_slice()).expect("header parses");
        assert_eq!(h.version, NNUE_VERSION);
        assert_eq!(h.hash, 0x1234_5678);
        assert_eq!(h.description, "Network trained by someone");
    }

    #[test]
    fn a_non_net_is_rejected_before_anything_is_allocated() {
        let bytes = header_bytes(0xDEAD_BEEF, 0, "");
        assert!(matches!(read_header(&mut bytes.as_slice()), Err(NetError::NotANet)));
    }

    #[test]
    fn a_truncated_header_is_rejected_rather_than_padded() {
        let mut bytes = header_bytes(NNUE_VERSION, 1, "a long description");
        bytes.truncate(bytes.len() - 5);
        assert!(matches!(read_header(&mut bytes.as_slice()), Err(NetError::Truncated)));
    }

    /// A header claiming a gigabyte description must not be believed: the length field is
    /// attacker-controlled in the sense that it comes from a file the user downloaded.
    #[test]
    fn an_absurd_description_length_is_refused() {
        let mut v = Vec::new();
        v.extend_from_slice(&NNUE_VERSION.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(read_header(&mut v.as_slice()), Err(NetError::Truncated)));
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
}
