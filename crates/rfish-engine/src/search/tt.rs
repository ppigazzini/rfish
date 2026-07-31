//! The shared transposition table.
//!
//! # Why this is atomics and not a byte array
//!
//! Upstream's table is a raw allocation that every search thread writes without
//! synchronisation: a 10-byte entry can be read while another thread is halfway through
//! writing it, and the design absorbs that by checking the stored key fragment before
//! trusting anything else. That is a data race in the language's terms, and reproducing it
//! literally would need `unsafe`.
//!
//! rfish keeps the RACE and drops the undefined behaviour. Every field lives in an
//! [`AtomicU64`] read and written `Relaxed`, so two threads may still interleave and a
//! reader may still see a half-updated cluster — exactly upstream's behaviour, and exactly
//! what the key check exists to catch — but the program has defined semantics and
//! `ThreadSanitizer` stays quiet.
//!
//! # The layout is upstream's, to the byte
//!
//! A cluster is 32 bytes holding three entries, because the number of clusters a `Hash`
//! setting buys is `mb * 1024 * 1024 / 32`, and that number decides which positions
//! collide. A larger cluster would change the collision pattern and therefore the node
//! count, so the packing below fits three 10-byte entries into four `u64` words with no
//! field straddling a word:
//!
//! ```text
//! word[0..3]  entry i: key16 | move16 | value16 | eval16
//! word[3]     byte 2i = depth8, byte 2i+1 = gen_bound8, bytes 6..8 unused
//! ```
//!
//! Golden: `Stockfish/src/tt.h`, `Stockfish/src/tt.cpp`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::board::types::{
    Bound, Key, Move, VALUE_MATE, VALUE_NONE, VALUE_TB, VALUE_TB_LOSS_IN_MAX_PLY,
    VALUE_TB_WIN_IN_MAX_PLY, Value, is_loss, is_mate, is_mated, is_valid, is_win,
};

/// Entries per cluster.
pub const CLUSTER_SIZE: usize = 3;
/// Bytes per cluster, upstream's. The cluster COUNT derives from this, and the count is
/// what decides which positions share a slot.
pub const CLUSTER_BYTES: usize = 32;

/// Upstream's `DEPTH_NONE`: the depth stored is biased by this, so the all-zero encoding
/// reads back as a depth below any real one and `depth8 != 0` is the occupancy test.
const DEPTH_NONE: i32 = -3;

/// Bits of `gen_bound8` holding the generation. The bound occupies the next two and the
/// PV flag the top one, so a generation comparison must mask.
const GENERATION_BITS: u8 = 5;
/// The mask isolating the generation from the bound and PV bits.
const GENERATION_MASK: u8 = (1 << GENERATION_BITS) - 1;
/// Where the bound sits in `gen_bound8`.
const BOUND_SHIFT: u8 = GENERATION_BITS;
/// Where the PV flag sits in `gen_bound8`.
const PV_SHIFT: u8 = BOUND_SHIFT + 2;

/// One cluster: three entries, packed into four atomic words.
#[derive(Debug, Default)]
#[repr(align(32))]
struct Cluster {
    /// `key16 | move16 | value16 | eval16` for entry `i`.
    main: [AtomicU64; CLUSTER_SIZE],
    /// `depth8` and `gen_bound8` for all three entries, two bytes each.
    meta: AtomicU64,
}

/// A decoded entry, as the search reads it.
#[derive(Clone, Copy, Debug)]
pub struct TTData {
    /// The best move found, or [`Move::NONE`].
    pub mv: Move,
    /// The stored score, still in the table's mate-distance convention.
    pub value: Value,
    /// The static evaluation of the position, or [`VALUE_NONE`].
    pub eval: Value,
    /// The depth the score was searched to.
    pub depth: i32,
    /// What the score bounds.
    pub bound: Bound,
    /// True when the position was a PV node when it was stored.
    pub is_pv: bool,
}

/// The result of a probe: whether it hit, what it found, and where to write back.
#[derive(Clone, Copy, Debug)]
pub struct TTProbe {
    /// True when a stored entry matched the position key.
    pub hit: bool,
    /// The decoded entry. Meaningless unless `hit`.
    pub data: TTData,
    cluster: usize,
    slot: usize,
    key16: u16,
}

/// The transposition table.
///
/// Shared across every search thread by `&` reference: all mutation goes through atomics,
/// so no lock and no `&mut` is needed and the borrow checker can prove the sharing safe.
#[derive(Debug)]
pub struct TranspositionTable {
    clusters: Vec<Cluster>,
    /// The current search generation, in the top five bits.
    generation: AtomicU64,
}

impl Default for TranspositionTable {
    fn default() -> TranspositionTable {
        TranspositionTable::new(16)
    }
}

impl TranspositionTable {
    /// A table of `mb` mebibytes.
    #[must_use]
    pub fn new(mb: usize) -> TranspositionTable {
        let mut tt = TranspositionTable { clusters: Vec::new(), generation: AtomicU64::new(0) };
        tt.resize(mb);
        tt
    }

    /// Reallocate to `mb` mebibytes and clear.
    ///
    /// The cluster count is `mb * 1024 * 1024 / 32` — upstream's arithmetic, using
    /// upstream's cluster size rather than this type's own, because the count is what
    /// decides the collision pattern and therefore the node count.
    pub fn resize(&mut self, mb: usize) {
        let count = (mb.max(1) * 1024 * 1024) / CLUSTER_BYTES;
        self.clusters = Vec::new();
        self.clusters.resize_with(count, Cluster::default);
        self.generation.store(0, Ordering::Relaxed);
    }

    /// Forget everything. Called on `ucinewgame`.
    pub fn clear(&self) {
        for c in &self.clusters {
            for w in &c.main {
                w.store(0, Ordering::Relaxed);
            }
            c.meta.store(0, Ordering::Relaxed);
        }
        self.generation.store(0, Ordering::Relaxed);
    }

    /// How many clusters the table holds.
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// Advance the generation counter — one `go`, one generation.
    pub fn new_search(&self) {
        // Masked so it never overflows into the bound and PV bits it shares a byte with.
        let next =
            (self.generation.load(Ordering::Relaxed) as u8).wrapping_add(1) & GENERATION_MASK;
        self.generation.store(u64::from(next), Ordering::Relaxed);
    }

    /// The current generation byte, already masked.
    #[must_use]
    fn generation8(&self) -> u8 {
        self.generation.load(Ordering::Relaxed) as u8
    }

    /// The cluster `key` maps to.
    ///
    /// Upstream's multiply-shift: the high 64 bits of `key * clusterCount` in 128-bit
    /// arithmetic, which spreads the key over the whole table without a modulo.
    #[inline(always)]
    fn cluster_of(&self, key: Key) -> usize {
        ((u128::from(key) * self.clusters.len() as u128) >> 64) as usize
    }

    /// Look `key` up.
    ///
    /// Returns a probe result that is also the write handle: [`TranspositionTable::store`]
    /// takes it and writes to the slot the replacement policy already chose, so the policy
    /// runs once per position rather than once per probe and once per store.
    #[must_use]
    pub fn probe(&self, key: Key) -> TTProbe {
        let cluster = self.cluster_of(key);
        let key16 = key as u16;
        let c = &self.clusters[cluster];
        let meta = c.meta.load(Ordering::Relaxed);

        for slot in 0..CLUSTER_SIZE {
            let main = c.main[slot].load(Ordering::Relaxed);
            if (main as u16) == key16 {
                let (depth8, gen_bound) = unpack_meta(meta, slot);
                // A key match is not yet a hit: the entry must also be OCCUPIED. An entry
                // whose depth was penalised down to zero keeps its key but has been
                // retired, and reading it as a hit would resurrect a score the search
                // deliberately discarded.
                //
                // The generation is NOT refreshed here. Upstream's probe only reads, and
                // refreshing on read would keep a stale entry alive across generations and
                // change which entries the replacement sweep evicts.
                return TTProbe {
                    hit: depth8 != 0,
                    data: decode(main, depth8, gen_bound),
                    cluster,
                    slot,
                    key16,
                };
            }
        }

        // Miss: pick the replacement victim now, by upstream's rule -- prefer the entry
        // whose depth, discounted eight-fold by how many generations old it is, is lowest.
        let cur_gen = self.generation8();
        let mut victim = 0usize;
        let (d0, g0) = unpack_meta(meta, 0);
        let mut worst = i32::from(d0) - 8 * i32::from(relative_age(g0, cur_gen));
        for slot in 1..CLUSTER_SIZE {
            let (depth8, gen_bound) = unpack_meta(meta, slot);
            let score = i32::from(depth8) - 8 * i32::from(relative_age(gen_bound, cur_gen));
            // Strictly greater, so a tie keeps the earlier slot -- upstream compares the
            // incumbent against the candidate in that direction.
            if worst > score {
                worst = score;
                victim = slot;
            }
        }

        TTProbe { hit: false, data: empty_data(), cluster, slot: victim, key16 }
    }

    /// Write an entry back into the slot `probe` selected.
    ///
    /// The four conditions under which the existing entry is overwritten are upstream's: an
    /// exact bound always wins, a different position always wins, a deep enough search
    /// wins, and any entry from an older generation wins. When none holds, the entry
    /// survives — but a decisive score at a useful depth is aged down a ply, which is what
    /// keeps a stale forced mate from blocking the slot forever.
    pub fn store(
        &self,
        probe: TTProbe,
        mv: Move,
        value: Value,
        eval: Value,
        depth: i32,
        bound: Bound,
        is_pv: bool,
    ) {
        let c = &self.clusters[probe.cluster];
        let main = c.main[probe.slot].load(Ordering::Relaxed);
        let meta = c.meta.load(Ordering::Relaxed);
        let (old_depth8, old_gen) = unpack_meta(meta, probe.slot);
        let same_key = (main as u16) == probe.key16;
        let cur_gen = self.generation8();

        // Keep the previous move when this search found none for the SAME position: a move
        // from a shallower search still orders better than nothing. For a different
        // position the stored move is meaningless and is cleared. This happens BEFORE the
        // replacement test and independently of it -- an entry that survives the test
        // still takes the new move.
        let packed_move =
            if !mv.is_none() || !same_key { u64::from(mv.raw()) } else { (main >> 16) & 0xFFFF };
        let main = (main & !(0xFFFFu64 << 16)) | (packed_move << 16);
        c.main[probe.slot].store(main, Ordering::Relaxed);

        if bound == Bound::Exact
            || !same_key
            || depth - DEPTH_NONE + 2 * i32::from(is_pv) > i32::from(old_depth8) - 4
            || relative_age(old_gen, cur_gen) != 0
        {
            let depth8 = (depth - DEPTH_NONE).clamp(0, 255) as u8;
            let gen_bound =
                cur_gen | ((bound as u8) << BOUND_SHIFT) | (u8::from(is_pv) << PV_SHIFT);
            let packed = u64::from(probe.key16)
                | (packed_move << 16)
                | (u64::from(value as i16 as u16) << 32)
                | (u64::from(eval as i16 as u16) << 48);
            c.main[probe.slot].store(packed, Ordering::Relaxed);
            c.meta.store(pack_meta(meta, probe.slot, depth8, gen_bound), Ordering::Relaxed);
            return;
        }

        // Secondary aging. A stored mate or tablebase score that is not exact loses a ply
        // of depth each time it blocks a write, so it eventually stops winning the
        // replacement test. Without it, elementary mates can be missed: the entry that
        // proves the mate at the wrong distance never yields its slot.
        if i32::from(old_depth8) + DEPTH_NONE >= 5
            && Bound::from_raw((old_gen >> BOUND_SHIFT) & 3) != Bound::Exact
        {
            let v16 = i32::from((main >> 32) as u16 as i16);
            if v16.abs() < crate::board::types::VALUE_INFINITE
                && crate::board::types::is_decisive(v16)
            {
                // Saturating at zero: a racy read could otherwise underflow into the
                // "occupied" encoding of a far deeper entry.
                let aged = (i32::from(old_depth8) - 1).max(0) as u8;
                c.meta.store(pack_meta(meta, probe.slot, aged, old_gen), Ordering::Relaxed);
            }
        }
    }

    /// Reduce the stored depth of the slot `probe` selected.
    ///
    /// Used when a lookup failed only because the stored bound sits on the wrong side of
    /// the current window: the entry is not wrong, but it is not answering questions
    /// either, and letting it keep a deep slot crowds out entries that would.
    pub fn penalize(&self, probe: TTProbe, penalty: i32) {
        let c = &self.clusters[probe.cluster];
        let meta = c.meta.load(Ordering::Relaxed);
        let (depth8, gen_bound) = unpack_meta(meta, probe.slot);
        let reduced = (i32::from(depth8) - penalty).max(0) as u8;
        c.meta.store(pack_meta(meta, probe.slot, reduced, gen_bound), Ordering::Relaxed);
    }

    /// Per-mille of the table holding entries no older than `max_age` generations, as
    /// UCI's `hashfull`.
    ///
    /// Sampled over the first thousand clusters, exactly as upstream does: a full sweep of
    /// a gigabyte table would cost more than the information is worth. The divisor is the
    /// cluster size, not the entry count, because a thousand clusters hold three thousand
    /// entries and the protocol wants per mille.
    #[must_use]
    pub fn hashfull(&self, max_age: u8) -> u32 {
        let n = 1000.min(self.clusters.len());
        if n == 0 {
            return 0;
        }
        let cur_gen = self.generation8();
        let mut used = 0u32;
        for c in &self.clusters[..n] {
            let meta = c.meta.load(Ordering::Relaxed);
            for slot in 0..CLUSTER_SIZE {
                let (depth8, gen_bound) = unpack_meta(meta, slot);
                if depth8 != 0 && relative_age(gen_bound, cur_gen) <= max_age {
                    used += 1;
                }
            }
        }
        used / CLUSTER_SIZE as u32
    }
}

/// How many generations old an entry is, counted like a clock: `0 - 1 == 31`.
///
/// The subtraction is done on the WHOLE stored byte, bound and PV bits included, and only
/// then masked. Masking first would give a different answer whenever those upper bits are
/// set, and they usually are.
#[inline(always)]
fn relative_age(gen_bound: u8, current: u8) -> u8 {
    current.wrapping_sub(gen_bound) & GENERATION_MASK
}

/// The two metadata bytes for `slot`.
#[inline(always)]
fn unpack_meta(meta: u64, slot: usize) -> (u8, u8) {
    let shift = slot * 16;
    ((meta >> shift) as u8, (meta >> (shift + 8)) as u8)
}

/// `meta` with `slot`'s two metadata bytes replaced.
#[inline(always)]
fn pack_meta(meta: u64, slot: usize, depth8: u8, gen_bound: u8) -> u64 {
    let shift = slot * 16;
    let cleared = meta & !(0xFFFFu64 << shift);
    cleared | (u64::from(depth8) << shift) | (u64::from(gen_bound) << (shift + 8))
}

fn decode(main: u64, depth8: u8, gen_bound: u8) -> TTData {
    TTData {
        mv: Move::from_raw((main >> 16) as u16),
        value: i32::from((main >> 32) as u16 as i16),
        eval: i32::from((main >> 48) as u16 as i16),
        depth: DEPTH_NONE + i32::from(depth8),
        bound: Bound::from_raw((gen_bound >> BOUND_SHIFT) & 3),
        is_pv: gen_bound & (1 << PV_SHIFT) != 0,
    }
}

fn empty_data() -> TTData {
    TTData {
        mv: Move::NONE,
        value: VALUE_NONE,
        eval: VALUE_NONE,
        depth: DEPTH_NONE,
        bound: Bound::None,
        is_pv: false,
    }
}

/// Convert a score for storage: a mate or tablebase score is stored as distance from the
/// CURRENT node, not from the root, so the same entry is correct wherever it is found
/// again.
#[inline]
#[must_use]
pub fn value_to_tt(v: Value, ply: i32) -> Value {
    if is_win(v) {
        v + ply
    } else if is_loss(v) {
        v - ply
    } else {
        v
    }
}

/// The inverse of [`value_to_tt`], with upstream's fifty-move guard.
///
/// A proven score found under a nearly expired halfmove clock is demoted to the highest
/// non-tablebase score rather than trusted: the rule may draw the game before the win
/// arrives, and the stored entry has no way to know how much clock its finder had. The
/// guard applies to mate scores and tablebase scores separately, because their distances
/// are measured from different origins.
#[inline]
#[must_use]
pub fn value_from_tt(v: Value, ply: i32, rule50: i32) -> Value {
    if !is_valid(v) {
        return VALUE_NONE;
    }
    if is_win(v) {
        if is_mate(v) && VALUE_MATE - v > 100 - rule50 {
            return VALUE_TB_WIN_IN_MAX_PLY - 1;
        }
        if VALUE_TB - v > 100 - rule50 {
            return VALUE_TB_WIN_IN_MAX_PLY - 1;
        }
        return v - ply;
    }
    if is_loss(v) {
        if is_mated(v) && VALUE_MATE + v > 100 - rule50 {
            return VALUE_TB_LOSS_IN_MAX_PLY + 1;
        }
        if VALUE_TB + v > 100 - rule50 {
            return VALUE_TB_LOSS_IN_MAX_PLY + 1;
        }
        return v + ply;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::types::{Square, VALUE_MATE};

    #[test]
    fn cluster_count_follows_upstreams_arithmetic() {
        // The count decides which positions collide, so it is pinned to upstream's
        // 32-byte cluster rather than to this type's own size.
        let tt = TranspositionTable::new(16);
        assert_eq!(tt.cluster_count(), 16 * 1024 * 1024 / 32);
        assert_eq!(size_of::<Cluster>(), CLUSTER_BYTES);
    }

    #[test]
    fn store_then_probe_round_trips_every_field() {
        let tt = TranspositionTable::new(1);
        let key = 0x0123_4567_89AB_CDEF;
        let mv = Move::new(Square::A1, Square::H8);
        let p = tt.probe(key);
        assert!(!p.hit);
        tt.store(p, mv, 123, -45, 7, Bound::Exact, true);

        let q = tt.probe(key);
        assert!(q.hit);
        assert_eq!(q.data.mv, mv);
        assert_eq!(q.data.value, 123);
        assert_eq!(q.data.eval, -45);
        assert_eq!(q.data.depth, 7);
        assert_eq!(q.data.bound, Bound::Exact);
        assert!(q.data.is_pv);
    }

    #[test]
    fn three_entries_share_a_cluster_without_disturbing_each_other() {
        let tt = TranspositionTable::new(1);
        // Keys differing only in the low 16 bits land in the same cluster but different
        // slots, which is exactly what the packed metadata word has to keep separate.
        let base = 0xDEAD_BEEF_0000_0000u64;
        for i in 0..3u64 {
            let p = tt.probe(base | i);
            tt.store(p, Move::from_raw(100 + i as u16), i as i32, 0, i as i32, Bound::Lower, false);
        }
        for i in 0..3u64 {
            let q = tt.probe(base | i);
            assert!(q.hit, "entry {i} was evicted by its neighbours");
            assert_eq!(q.data.mv.raw(), 100 + i as u16);
            assert_eq!(q.data.depth, i as i32);
        }
    }

    #[test]
    fn a_missing_move_does_not_erase_the_stored_one() {
        let tt = TranspositionTable::new(1);
        let key = 42;
        let mv = Move::new(Square::A1, Square::H1);
        tt.store(tt.probe(key), mv, 10, 0, 5, Bound::Lower, false);
        // A deeper search with no best move must keep the old one for ordering.
        tt.store(tt.probe(key), Move::NONE, 20, 0, 9, Bound::Upper, false);
        let q = tt.probe(key);
        assert!(q.hit);
        assert_eq!(q.data.mv, mv);
        assert_eq!(q.data.depth, 9);
    }

    #[test]
    fn mate_scores_survive_the_ply_round_trip() {
        for ply in [0, 1, 20, 100] {
            let mate = VALUE_MATE - 10;
            assert_eq!(value_from_tt(value_to_tt(mate, ply), ply, 0), mate);
            let mated = -VALUE_MATE + 10;
            assert_eq!(value_from_tt(value_to_tt(mated, ply), ply, 0), mated);
            // An ordinary score is untouched.
            assert_eq!(value_from_tt(value_to_tt(37, ply), ply, 0), 37);
        }
    }

    #[test]
    fn clear_forgets_everything() {
        let tt = TranspositionTable::new(1);
        tt.store(tt.probe(7), Move::from_raw(9), 1, 1, 1, Bound::Exact, false);
        assert!(tt.probe(7).hit);
        tt.clear();
        assert!(!tt.probe(7).hit);
        assert_eq!(tt.hashfull(0), 0);
    }

    /// The table is shared by `&`, so this must compile and must not race. It is the
    /// property the whole atomic design exists to provide.
    #[test]
    fn the_table_is_shared_across_threads_by_reference() {
        let tt = TranspositionTable::new(4);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let tt = &tt;
                s.spawn(move || {
                    for i in 0..10_000u64 {
                        let key = t * 1_000_003 + i;
                        let p = tt.probe(key);
                        tt.store(
                            p,
                            Move::from_raw(i as u16 | 1),
                            i as i32,
                            0,
                            3,
                            Bound::Lower,
                            false,
                        );
                    }
                });
            }
        });
        assert!(tt.hashfull(0) > 0);
    }
}
