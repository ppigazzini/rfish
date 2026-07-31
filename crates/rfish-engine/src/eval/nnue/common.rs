//! Constants and stream readers shared by every NNUE component.
//!
//! **Every constant here is a contract with the network FILE.** A net is a flat byte stream
//! with no self-describing structure beyond three hash words: change a dimension and the
//! loader reads the right number of bytes into the wrong place, producing an evaluation
//! that is wrong rather than a failure that is visible.
//!
//! Golden: `Stockfish/src/nnue/nnue_common.h`, `nnue_architecture.h`.

use std::io::{self, Read, Write};

/// The format version word every Stockfish net begins with.
pub const VERSION: u32 = 0x6A44_8AFA;

/// Transformed feature dimensions for one side.
pub const L1: usize = 1024;
/// Outputs of the first fully connected layer.
pub const L2: usize = 32;
/// Outputs of the second fully connected layer.
pub const L3: usize = 32;

/// Material buckets the PSQT head is split into.
pub const PSQT_BUCKETS: usize = 8;
/// Independent output heads, selected by the same bucket.
pub const LAYER_STACKS: usize = 8;

/// Scales the final output into centipawn-adjacent units.
pub const OUTPUT_SCALE: i64 = 16;
/// How far a layer's accumulator is shifted down before the activation.
pub const WEIGHT_SCALE_BITS: u32 = 6;
/// Clamp of a feature-transformer output before the pairwise multiply.
pub const FT_MAX_VAL: i32 = 255;
/// The quantised value of 1.0 inside the hidden layers.
pub const HIDDEN_ONE_VAL: i64 = 128;

/// The magic string that precedes every LEB128-compressed block.
pub const LEB128_MAGIC: &[u8] = b"COMPRESSED_LEB128";

/// What went wrong while reading a net.
#[derive(Debug)]
pub enum NetError {
    /// The file could not be opened or read.
    Io(io::Error),
    /// The leading version word is not [`VERSION`].
    NotANet,
    /// The file is a net, but not for the architecture this engine implements.
    ///
    /// The hash is computed from the layer shapes, so this fires before a single weight is
    /// read — which is the only place it can fire usefully, since a mismatched net would
    /// otherwise load without complaint and evaluate nonsense.
    WrongArchitecture { expected: u32, found: u32 },
    /// A component's own hash word did not match.
    WrongComponent { what: &'static str, expected: u32, found: u32 },
    /// The file ended before the structure did.
    Truncated,
    /// A compressed block did not begin with the magic string.
    NotCompressed,
    /// The file has trailing bytes after the last layer stack.
    TrailingData,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "{e}"),
            NetError::NotANet => f.write_str("not an NNUE network file"),
            NetError::WrongArchitecture { expected, found } => {
                write!(f, "network architecture {found:#010x} does not match {expected:#010x}")
            }
            NetError::WrongComponent { what, expected, found } => {
                write!(f, "{what}: hash {found:#010x} does not match {expected:#010x}")
            }
            NetError::Truncated => f.write_str("network file ends before its structure does"),
            NetError::NotCompressed => f.write_str("a weight block is missing its LEB128 marker"),
            NetError::TrailingData => f.write_str("network file has trailing data"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> NetError {
        NetError::Io(e)
    }
}

/// A reader that also decodes the two encodings a net uses.
pub struct NetReader<R: Read> {
    inner: R,
}

/// Written by hand rather than derived: requiring `R: Debug` would exclude every reader a
/// caller actually has, and the wrapped reader has nothing worth printing anyway.
impl<R: Read> std::fmt::Debug for NetReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetReader").finish_non_exhaustive()
    }
}

impl<R: Read> NetReader<R> {
    pub fn new(inner: R) -> NetReader<R> {
        NetReader { inner }
    }

    /// Read exactly `buf.len()` bytes, or fail.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), NetError> {
        self.inner.read_exact(buf).map_err(|_| NetError::Truncated)
    }

    /// One little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, NetError> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// `out.len()` little-endian `i16`s, uncompressed.
    pub fn i16s(&mut self, out: &mut [i16]) -> Result<(), NetError> {
        let mut buf = vec![0u8; out.len() * 2];
        self.read_exact(&mut buf)?;
        for (o, c) in out.iter_mut().zip(buf.as_chunks::<2>().0) {
            *o = i16::from_le_bytes(*c);
        }
        Ok(())
    }

    /// `out.len()` `i8`s, uncompressed.
    ///
    /// The threat and pawn-pair weight blocks are stored this way — NOT LEB128 — because
    /// they are already one byte per weight and the compression would cost a byte on any
    /// value outside `-64..64`.
    pub fn i8s(&mut self, out: &mut [i8]) -> Result<(), NetError> {
        let mut buf = vec![0u8; out.len()];
        self.read_exact(&mut buf)?;
        for (o, b) in out.iter_mut().zip(buf.iter()) {
            *o = *b as i8;
        }
        Ok(())
    }

    /// `out.len()` `i32`s, uncompressed.
    pub fn i32s(&mut self, out: &mut [i32]) -> Result<(), NetError> {
        let mut buf = vec![0u8; out.len() * 4];
        self.read_exact(&mut buf)?;
        for (o, c) in out.iter_mut().zip(buf.as_chunks::<4>().0) {
            *o = i32::from_le_bytes(*c);
        }
        Ok(())
    }

    /// One LEB128-compressed block holding exactly `out.len()` signed values.
    ///
    /// Signed LEB128: seven payload bits per byte, low group first, continuation in bit 7.
    /// A value whose last byte has bit 6 set is negative and gets sign-extended. The sign
    /// extension is skipped once 32 bits have been consumed, because the value is already
    /// complete — reproducing that special case matters, since it is what lets a full-range
    /// `i32` weight round-trip.
    pub fn leb128(&mut self, out: &mut [i32]) -> Result<(), NetError> {
        let mut magic = [0u8; 17];
        self.read_exact(&mut magic)?;
        if magic != LEB128_MAGIC {
            return Err(NetError::NotCompressed);
        }
        let byte_count = self.u32()? as usize;
        let mut bytes = vec![0u8; byte_count];
        self.read_exact(&mut bytes)?;

        let mut result: i32 = 0;
        let mut shift: u32 = 0;
        let mut i = 0usize;
        for &byte in &bytes {
            if i == out.len() {
                break;
            }
            result |= i32::from(byte & 0x7F) << (shift % 32);
            shift += 7;
            if byte & 0x80 == 0 {
                out[i] = if shift >= 32 || byte & 0x40 == 0 {
                    result
                } else {
                    // Sign-extend: every bit above the payload becomes one.
                    result | !((1i32 << shift).wrapping_sub(1))
                };
                i += 1;
                result = 0;
                shift = 0;
            }
        }
        if i == out.len() { Ok(()) } else { Err(NetError::Truncated) }
    }

    /// One LEB128 block decoded into `i16`s.
    pub fn leb128_i16(&mut self, out: &mut [i16]) -> Result<(), NetError> {
        let mut wide = vec![0i32; out.len()];
        self.leb128(&mut wide)?;
        for (o, w) in out.iter_mut().zip(wide.iter()) {
            *o = *w as i16;
        }
        Ok(())
    }

    /// True when the stream holds nothing more.
    ///
    /// Upstream requires it: a net with trailing bytes is a net whose structure this build
    /// does not agree with, even when every hash matched.
    pub fn at_end(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(self.inner.read(&mut b), Ok(0))
    }
}

/// Round `n` up to a multiple of `base`.
#[must_use]
pub const fn ceil_to_multiple(n: usize, base: usize) -> usize {
    n.div_ceil(base) * base
}

/// A `Box<[T; N]>` built on the heap, never through a stack temporary.
///
/// The weight arrays here are tens of megabytes; `Box::new([0; N])` materialises them in a
/// stack frame first and overflows the thread stack long before the allocation happens.
pub fn boxed<T: Copy, const N: usize>(fill: T) -> Box<[T; N]> {
    match vec![fill; N].into_boxed_slice().try_into() {
        Ok(b) => b,
        Err(_) => unreachable!("the vec was built with exactly N items"),
    }
}

/// A writer that produces exactly what [`NetReader`] consumes.
///
/// Written as the mirror of the reader rather than from the file-format spec, because the
/// property that matters is that a net survives a round trip through THIS code. Any
/// disagreement between the two halves is then a test failure rather than a corrupt file
/// somebody discovers later.
pub struct NetWriter<W: Write> {
    inner: W,
}

/// Hand-written for the same reason [`NetReader`]'s is: requiring `W: Debug` would exclude
/// every writer a caller actually has.
impl<W: Write> std::fmt::Debug for NetWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetWriter").finish_non_exhaustive()
    }
}

impl<W: Write> NetWriter<W> {
    pub fn new(inner: W) -> NetWriter<W> {
        NetWriter { inner }
    }

    /// Write every byte of `buf`.
    pub fn write_all(&mut self, buf: &[u8]) -> Result<(), NetError> {
        self.inner.write_all(buf).map_err(NetError::Io)
    }

    /// One little-endian `u32`.
    pub fn u32(&mut self, v: u32) -> Result<(), NetError> {
        self.write_all(&v.to_le_bytes())
    }

    /// Little-endian `i16`s, uncompressed.
    pub fn i16s(&mut self, v: &[i16]) -> Result<(), NetError> {
        let mut buf = Vec::with_capacity(v.len() * 2);
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        self.write_all(&buf)
    }

    /// `i8`s, uncompressed — the encoding the threat and pawn-pair blocks use.
    pub fn i8s(&mut self, v: &[i8]) -> Result<(), NetError> {
        let buf: Vec<u8> = v.iter().map(|x| *x as u8).collect();
        self.write_all(&buf)
    }

    /// Little-endian `i32`s, uncompressed.
    pub fn i32s(&mut self, v: &[i32]) -> Result<(), NetError> {
        let mut buf = Vec::with_capacity(v.len() * 4);
        for x in v {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        self.write_all(&buf)
    }

    /// One LEB128-compressed block.
    ///
    /// The encoder has to agree with the decoder's stopping rule exactly: emit groups of
    /// seven bits, low group first, and stop once the remaining bits are all sign bits AND
    /// the sign bit of the payload agrees with them. Stopping one group early would make
    /// the decoder sign-extend a positive value into a negative one.
    pub fn leb128(&mut self, v: &[i32]) -> Result<(), NetError> {
        self.write_all(LEB128_MAGIC)?;

        let mut bytes: Vec<u8> = Vec::with_capacity(v.len());
        for &value in v {
            let mut x = value;
            loop {
                let byte = (x & 0x7F) as u8;
                // Arithmetic shift: the sign bits keep arriving, which is what lets the
                // test below detect that nothing but sign is left.
                x >>= 7;
                let sign_bit_set = byte & 0x40 != 0;
                let done = (x == 0 && !sign_bit_set) || (x == -1 && sign_bit_set);
                bytes.push(if done { byte } else { byte | 0x80 });
                if done {
                    break;
                }
            }
        }

        self.u32(u32::try_from(bytes.len()).map_err(|_| NetError::Truncated)?)?;
        self.write_all(&bytes)
    }

    /// One LEB128 block from `i16`s.
    pub fn leb128_i16(&mut self, v: &[i16]) -> Result<(), NetError> {
        let wide: Vec<i32> = v.iter().map(|x| i32::from(*x)).collect();
        self.leb128(&wide)
    }

    /// Flush whatever the wrapped writer is buffering.
    pub fn flush(&mut self) -> Result<(), NetError> {
        self.inner.flush().map_err(NetError::Io)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn leb128_survives_a_round_trip_over_the_whole_signed_range() {
        // The encoder's stopping rule and the decoder's sign extension have to agree
        // EXACTLY, and they disagree only at the boundaries -- so test the boundaries.
        let mut values: Vec<i32> = vec![
            0,
            1,
            -1,
            63,
            64,
            -64,
            -65,
            8191,
            8192,
            -8192,
            -8193,
            i32::MAX,
            i32::MIN,
            i32::MAX - 1,
            i32::MIN + 1,
        ];
        for shift in 0..31 {
            values.push(1i32 << shift);
            values.push(-(1i32 << shift));
        }

        let mut buf = Vec::new();
        super::NetWriter::new(&mut buf).leb128(&values).expect("writes");

        let mut back = vec![0i32; values.len()];
        super::NetReader::new(buf.as_slice()).leb128(&mut back).expect("reads");
        assert_eq!(back, values);
    }

    #[test]
    fn the_uncompressed_encodings_round_trip_too() {
        let i16s: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 300, -300];
        let mut buf = Vec::new();
        super::NetWriter::new(&mut buf).i16s(&i16s).expect("writes");
        let mut back = vec![0i16; i16s.len()];
        super::NetReader::new(buf.as_slice()).i16s(&mut back).expect("reads");
        assert_eq!(back, i16s);

        let i8s: Vec<i8> = vec![0, 1, -1, i8::MAX, i8::MIN, 64, -64];
        let mut buf = Vec::new();
        super::NetWriter::new(&mut buf).i8s(&i8s).expect("writes");
        let mut back = vec![0i8; i8s.len()];
        super::NetReader::new(buf.as_slice()).i8s(&mut back).expect("reads");
        assert_eq!(back, i8s);

        let i32s: Vec<i32> = vec![0, 1, -1, i32::MAX, i32::MIN];
        let mut buf = Vec::new();
        super::NetWriter::new(&mut buf).i32s(&i32s).expect("writes");
        let mut back = vec![0i32; i32s.len()];
        super::NetReader::new(buf.as_slice()).i32s(&mut back).expect("reads");
        assert_eq!(back, i32s);
    }

    use super::*;

    fn leb_block(values: &[i32]) -> Vec<u8> {
        // Encode with the standard signed LEB128 the trainer writes.
        let mut payload = Vec::new();
        for &v in values {
            let mut value = v;
            loop {
                let byte = (value & 0x7F) as u8;
                value >>= 7;
                let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
                payload.push(if done { byte } else { byte | 0x80 });
                if done {
                    break;
                }
            }
        }
        let mut out = LEB128_MAGIC.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn leb128_round_trips_the_whole_signed_range() {
        let values = [0, 1, -1, 63, 64, -64, -65, 127, -128, 1000, -1000, i32::MAX, i32::MIN];
        let bytes = leb_block(&values);
        let mut out = vec![0i32; values.len()];
        NetReader::new(bytes.as_slice()).leb128(&mut out).expect("decodes");
        assert_eq!(out, values);
    }

    #[test]
    fn a_block_without_the_magic_string_is_rejected() {
        let mut bytes = leb_block(&[1, 2, 3]);
        bytes[0] = b'X';
        let mut out = vec![0i32; 3];
        assert!(matches!(
            NetReader::new(bytes.as_slice()).leb128(&mut out),
            Err(NetError::NotCompressed)
        ));
    }

    #[test]
    fn a_short_block_is_truncated_rather_than_zero_filled() {
        let bytes = leb_block(&[1, 2]);
        let mut out = vec![0i32; 5];
        assert!(matches!(
            NetReader::new(bytes.as_slice()).leb128(&mut out),
            Err(NetError::Truncated)
        ));
    }

    #[test]
    fn fixed_width_readers_are_little_endian() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0xFF, 0xFF];
        let mut r = NetReader::new(bytes.as_slice());
        assert_eq!(r.u32().expect("u32"), 0x0403_0201);
        let mut v = [0i16; 1];
        r.i16s(&mut v).expect("i16");
        assert_eq!(v[0], -1);

        let mut r = NetReader::new([0xFFu8, 0x7F].as_slice());
        let mut v = [0i8; 2];
        r.i8s(&mut v).expect("i8");
        assert_eq!(v, [-1, 127]);
    }

    #[test]
    fn the_architecture_constants_are_the_ones_the_file_encodes() {
        // These are not tunables. A change here without a matching net silently misreads
        // every weight after the first mismatched block.
        assert_eq!(L1, 1024);
        assert_eq!(L2, 32);
        assert_eq!(L3, 32);
        assert_eq!(PSQT_BUCKETS, 8);
        assert_eq!(LAYER_STACKS, 8);
        assert_eq!(ceil_to_multiple(1024, 32), 1024);
        assert_eq!(ceil_to_multiple(60, 32), 64);
    }
}
