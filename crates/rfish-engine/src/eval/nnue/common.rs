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

/// The cache line every NNUE weight table starts on.
///
/// Upstream's own `nnue_common.h` constant, and the alignment it puts on every array in the
/// feature transformer and the affine layers.
pub const CACHE_LINE: usize = 64;

/// A heap buffer of `T` whose first element sits on a cache-line boundary.
///
/// **`vec![0; n]` does not give this, and the difference is invisible to an instruction
/// counter.** A `Vec<i16>` is aligned for an `i16` — two bytes — and glibc hands these tables
/// back sixteen bytes past a cache line. A 32-byte vector load splits a line whenever its
/// address is not 32-byte aligned, so at a sixteen-byte offset every second load in the
/// weight sweep costs two line fills instead of one. Callgrind counts the load either way,
/// which is why this survived a ledger full of instruction ratios.
///
/// Upstream declares every one of these arrays `alignas(CacheLineSize)`. `../zfish` and
/// `../mcfish` each built an allocator whose stated contract is the same 64 bytes —
/// `../mcfish`'s says "alignment is load-bearing" in as many words. This is that guarantee
/// with no dependency and no `unsafe`: over-allocate by one line and start the slice at the
/// first aligned element. Reading an address as an integer is safe; nothing here
/// dereferences a raw pointer or reinterprets bytes.
///
/// The buffer is sized once and never grown. A reallocation would move the base and strand
/// `off`, so there is deliberately no API that can resize it.
pub struct Aligned<T> {
    buf: Vec<T>,
    off: usize,
    len: usize,
}

impl<T: Copy + Default> Aligned<T> {
    /// `len` default-valued elements, the first on a cache-line boundary.
    #[must_use]
    pub fn new(len: usize) -> Aligned<T> {
        let stride = size_of::<T>();
        // One spare line of elements, so an aligned start always exists inside the buffer.
        let buf = vec![T::default(); len + CACHE_LINE / stride];
        let misalign = (buf.as_ptr() as usize) % CACHE_LINE;
        let off = if misalign == 0 { 0 } else { (CACHE_LINE - misalign) / stride };
        let aligned = Aligned { buf, off, len };
        debug_assert_eq!(
            aligned.as_slice().as_ptr() as usize % CACHE_LINE,
            0,
            "the aligned start was not reachable: the allocation is not a multiple of \
             size_of::<T>() away from a cache line"
        );
        aligned
    }

    /// A copy of `src`, aligned.
    #[must_use]
    pub fn from_slice(src: &[T]) -> Aligned<T> {
        let mut out = Aligned::new(src.len());
        out.as_mut_slice().copy_from_slice(src);
        out
    }

    /// The aligned elements, and only those: the padding is never visible.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.buf[self.off..self.off + self.len]
    }

    /// [`Aligned::as_slice`], mutably.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.buf[self.off..self.off + self.len]
    }
}

impl<T: Copy + Default> Clone for Aligned<T> {
    /// Re-align rather than copy the offset: a cloned `Vec` has its own base address, and
    /// carrying the original's `off` across would point the slice at a different place in
    /// the line.
    fn clone(&self) -> Aligned<T> {
        Aligned::from_slice(self.as_slice())
    }
}

impl<T: Copy + Default> std::ops::Deref for Aligned<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy + Default> std::ops::DerefMut for Aligned<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Copy + Default + std::fmt::Debug> std::fmt::Debug for Aligned<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aligned").field("len", &self.len).finish_non_exhaustive()
    }
}

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

/// The most memory a LEB128 block's DECLARED length may reserve before a byte is read.
///
/// A capacity hint rather than a limit: the real bound is the file, and a legitimate block
/// larger than this still reads, growing as it goes. Sixty-four mebibytes clears the largest
/// block a shipped net carries — the feature transformer's 23,068,672 weights at about 1.28
/// bytes each — so the common case is one allocation and no copy.
const MAX_BLOCK_HINT: usize = 1 << 26;

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
    /// A compressed block declared more bytes than its values consume.
    BlockNotConsumed,
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
            NetError::BlockNotConsumed => {
                f.write_str("a weight block has bytes left after its last value")
            }
        }
    }
}

impl std::error::Error for NetError {}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> NetError {
        NetError::Io(e)
    }
}

/// The narrowing a LEB128 block applies on its way into the destination width.
///
/// Sealed by being private: the decode is `i32` internally because that is the width the
/// encoding is defined over, and the two blocks a net carries land in `i32` and `i16`.
trait FromI32Truncating: Copy {
    fn from_i32_truncating(v: i32) -> Self;
}

impl FromI32Truncating for i32 {
    fn from_i32_truncating(v: i32) -> i32 {
        v
    }
}

impl FromI32Truncating for i16 {
    /// Truncating, exactly as the narrowing pass this replaced was: the trainer keeps every
    /// value in range and a wrap here means a corrupt net rather than a value to saturate.
    fn from_i32_truncating(v: i32) -> i16 {
        v as i16
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
        self.leb128_block(out)
    }

    /// One LEB128 block decoded into `i16`s.
    ///
    /// Decoded STRAIGHT into the `i16`s. It used to decode into a `vec![0i32; out.len()]`
    /// and narrow afterwards, and on the main weight block that temporary is 23,068,672
    /// entries — **92 MiB, allocated and page-faulted in to be read once and thrown away**,
    /// which is most of what made a `quit`-only run peak at 253 MiB.
    pub fn leb128_i16(&mut self, out: &mut [i16]) -> Result<(), NetError> {
        self.leb128_block(out)
    }

    /// One LEB128 block decoded into a sequence of fixed-size `i16` GROUPS.
    ///
    /// The feature transformer stores its king-piece weights as vector lanes, so that a row's
    /// alignment is carried by its type. That storage has no `&mut [i16]` view a caller can
    /// hand over without reinterpreting bytes, and `Simd::as_mut_array` gives one group at a
    /// time instead.
    ///
    /// Each group is a slice walk of a length the type states, which is the shape the block
    /// already decoded into -- so the 23,068,672-entry weight block costs what it did before:
    /// measured against the same tree, a `quit`-only profile reads 1,281,493,841 instructions
    /// either way, and peak RSS 216,448 KB against 216,580 KB.
    pub fn leb128_i16_groups<'a, const N: usize>(
        &mut self,
        out: impl Iterator<Item = &'a mut [i16; N]>,
    ) -> Result<(), NetError> {
        let bytes = self.leb128_bytes()?;
        let mut src = bytes.iter();
        for group in out {
            // Threaded BY VALUE so the byte walk stays in registers across groups, which is
            // the residency the walk-the-output rewrite bought for the flat block.
            src = Self::leb128_values(src, group)?;
        }
        Self::whole(src.as_slice())
    }

    /// The decode both widths share, walking the OUTPUT and consuming bytes as it needs them.
    ///
    /// The loop used to walk the BYTES and index the output, which put two tests on the
    /// per-byte path that belong on neither: `if i == out.len()` ran once per byte where a
    /// value takes one or two, and `out[i] = ...` was a bounds test against a runtime length.
    /// Walking `out.iter_mut()` and pulling bytes from an iterator moves the length test to
    /// once per VALUE and removes the store's test outright — the same shape as the fold's
    /// weight rows in the transformer, one zone over.
    ///
    /// The arithmetic is unchanged, including the `% 32` and the `shift >= 32` guard that
    /// skips sign extension once the value is already complete. That guard is what lets a
    /// full-range `i32` weight round-trip, and `leb128_survives_a_round_trip_over_the_whole_signed_range`
    /// pins it.
    fn leb128_block<T: FromI32Truncating>(&mut self, out: &mut [T]) -> Result<(), NetError> {
        let bytes = self.leb128_bytes()?;
        Self::whole(Self::leb128_values(bytes.iter(), out)?.as_slice())
    }

    /// Refuse a block with bytes left after its last value.
    ///
    /// The declared length and the value count are two statements of the same fact, written
    /// at opposite ends of the format. A block that satisfies one but not the other has a
    /// structure this build does not share -- and it reaches here only when every hash
    /// already matched, which is exactly when accepting it silently costs the most.
    fn whole(rest: &[u8]) -> Result<(), NetError> {
        if rest.is_empty() { Ok(()) } else { Err(NetError::BlockNotConsumed) }
    }

    /// A LEB128 block's header, checked, and its payload.
    ///
    /// **The declared length is a capacity HINT, never the allocation.** It is an unvalidated
    /// `u32` read out of the file, so a twenty-two byte net can claim `0xFFFFFFFF` and a
    /// reader that believes it commits four gibibytes before discovering the file is empty —
    /// a denial of service from `setoption name EvalFile`, and the same defect upstream has
    /// at its own `read_header`, which this port already bounds one zone over.
    ///
    /// The bound that actually holds is the FILE: `take` stops at the declared count and
    /// `read_to_end` stops at end-of-input, so a short file yields a short read and the
    /// length test below turns it into `Truncated`. Nothing is trusted about the header
    /// except how much to hope for.
    fn leb128_bytes(&mut self) -> Result<Vec<u8>, NetError> {
        let mut magic = [0u8; 17];
        self.read_exact(&mut magic)?;
        if magic != LEB128_MAGIC {
            return Err(NetError::NotCompressed);
        }
        let byte_count = self.u32()? as usize;
        // Sized so the largest block a real net carries — the feature transformer's
        // 23,068,672 weights at about 1.28 bytes each — still lands in ONE allocation, which
        // is what keeps this off the startup profile. A larger legitimate block still reads,
        // it just grows; a hostile one never gets to ask for more than this up front.
        let mut bytes = Vec::with_capacity(byte_count.min(MAX_BLOCK_HINT));
        let read = std::io::Read::by_ref(&mut self.inner)
            .take(byte_count as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| NetError::Truncated)?;
        if read != byte_count {
            return Err(NetError::Truncated);
        }
        Ok(bytes)
    }

    /// As many values as `out` holds, pulled from `src`.
    fn leb128_values<'b, T: FromI32Truncating>(
        mut src: std::slice::Iter<'b, u8>,
        out: &mut [T],
    ) -> Result<std::slice::Iter<'b, u8>, NetError> {
        for o in out.iter_mut() {
            let mut result: i32 = 0;
            let mut shift: u32 = 0;
            loop {
                let Some(&byte) = src.next() else {
                    return Err(NetError::Truncated);
                };
                result |= i32::from(byte & 0x7F) << (shift % 32);
                shift += 7;
                if byte & 0x80 == 0 {
                    if shift < 32 && byte & 0x40 != 0 {
                        // Sign-extend: every bit above the payload becomes one.
                        result |= !((1i32 << shift).wrapping_sub(1));
                    }
                    break;
                }
            }
            *o = T::from_i32_truncating(result);
        }
        Ok(src)
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
        self.leb128_i16_from(v.iter().copied())
    }

    /// The same block, from any walk of `i16`s -- the mirror of
    /// [`NetReader::leb128_i16_into`], and there for the same storage.
    pub fn leb128_i16_from(&mut self, v: impl Iterator<Item = i16>) -> Result<(), NetError> {
        let wide: Vec<i32> = v.map(i32::from).collect();
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

    /// A block that declares four gibibytes and delivers eight bytes.
    ///
    /// The declared length is a `u32` straight out of the file, so this is what a hostile
    /// `setoption name EvalFile` looks like. The reader must reserve its bounded hint and
    /// then discover the truth from the FILE, not commit to the header and find out during
    /// the read.
    ///
    /// **The negative control for this one is deliberately not executed.** Restoring the
    /// `vec![0u8; byte_count]` it replaced makes this test allocate and ZERO four gibibytes,
    /// which is the denial of service being fixed and which has taken this machine down
    /// twice in its `speedtest` form. What the test can assert without running that is the
    /// fixed behaviour, and it is asserted on a declared count far above the hint so the
    /// clamped-reservation path is the one taken.
    #[test]
    fn a_block_claiming_more_than_the_file_holds_is_truncated_not_reserved() {
        let mut bytes = LEB128_MAGIC.to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);

        let mut out = vec![0i32; 4];
        assert!(matches!(
            NetReader::new(bytes.as_slice()).leb128(&mut out),
            Err(NetError::Truncated)
        ));

        // The same claim with no payload at all, which is the twenty-two byte file.
        let empty = [LEB128_MAGIC, &u32::MAX.to_le_bytes()[..]].concat();
        let mut out = vec![0i32; 1];
        assert!(matches!(
            NetReader::new(empty.as_slice()).leb128(&mut out),
            Err(NetError::Truncated)
        ));
    }

    /// The hint must stay a hint: a legitimate block LARGER than it still reads.
    ///
    /// Otherwise the bound quietly becomes a maximum net size, and the next architecture
    /// that outgrows it fails to load with a truncation error naming the wrong cause.
    #[test]
    fn a_block_larger_than_the_hint_still_reads() {
        // Not built at 64 MiB -- the property is that the count is a hint rather than a
        // limit, and `min` is what expresses it. Pin the expression instead of the volume.
        let declared = MAX_BLOCK_HINT * 4;
        assert_eq!(declared.min(MAX_BLOCK_HINT), MAX_BLOCK_HINT);
        // And a block SMALLER than the hint reserves only what it declared.
        assert_eq!((MAX_BLOCK_HINT / 4).min(MAX_BLOCK_HINT), MAX_BLOCK_HINT / 4);
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

    /// A block that declares more bytes than its values consume is refused.
    ///
    /// The complement of the truncation row above: too FEW bytes ends inside a value and is
    /// caught there, while too many decodes every value successfully and leaves the rest
    /// unread.
    #[test]
    fn a_block_with_bytes_left_over_is_refused() {
        let mut bytes = leb_block(&[1, 2, 3]);
        // One more payload byte than the three values need, and a declared length that
        // counts it: a well-formed encoding of nothing the reader was asked for. The count
        // sits immediately after the magic string, which is what fixes its offset.
        let at = LEB128_MAGIC.len();
        let declared = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
        bytes[at..at + 4].copy_from_slice(&(declared + 1).to_le_bytes());
        bytes.push(0x00);
        let mut out = vec![0i32; 3];
        assert!(matches!(
            NetReader::new(bytes.as_slice()).leb128(&mut out),
            Err(NetError::BlockNotConsumed)
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

    /// The property the NNUE weight tables depend on and `vec![0; n]` does not provide.
    #[test]
    fn an_aligned_buffer_starts_on_a_cache_line() {
        for len in [1usize, 7, 63, L1, L1 * 33] {
            let a: Aligned<i16> = Aligned::new(len);
            assert_eq!(a.as_slice().as_ptr() as usize % CACHE_LINE, 0, "i16 len {len}");
            assert_eq!(a.len(), len);
        }
        for len in [1usize, 1023, L1 * 17] {
            let a: Aligned<i8> = Aligned::new(len);
            assert_eq!(a.as_slice().as_ptr() as usize % CACHE_LINE, 0, "i8 len {len}");
        }
        for len in [1usize, 999, PSQT_BUCKETS * 3072] {
            let a: Aligned<i32> = Aligned::new(len);
            assert_eq!(a.as_slice().as_ptr() as usize % CACHE_LINE, 0, "i32 len {len}");
        }
    }

    /// A clone re-aligns: carrying the source's offset to a new base would point the slice
    /// somewhere else in the line.
    #[test]
    fn a_clone_is_realigned_and_equal() {
        let mut a: Aligned<i16> = Aligned::new(300);
        for (i, v) in a.as_mut_slice().iter_mut().enumerate() {
            *v = i as i16;
        }
        let b = a.clone();
        assert_eq!(a.as_slice(), b.as_slice());
        assert_eq!(b.as_slice().as_ptr() as usize % CACHE_LINE, 0);
    }
}
