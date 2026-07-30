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
    Bound, Key, MAX_PLY, Move, VALUE_MATE_IN_MAX_PLY, VALUE_MATED_IN_MAX_PLY, VALUE_NONE, Value,
};

/// Entries per cluster.
pub const CLUSTER_SIZE: usize = 3;
/// Bytes per cluster, upstream's. The cluster COUNT derives from this, and the count is
/// what decides which positions share a slot.
pub const CLUSTER_BYTES: usize = 32;

/// Upstream's `DEPTH_ENTRY_OFFSET`: the depth stored is biased so the "empty" encoding
/// (all zero) reads back as a depth below any real one.
const DEPTH_ENTRY_OFFSET: i32 = -3;

/// How many generations the counter advances per `ucinewgame`-free search.
const GENERATION_DELTA: u8 = 8;
/// The mask isolating the generation from the bound bits.
const GENERATION_MASK: u8 = 0xF8;
/// One full turn of the 5-bit generation counter.
const GENERATION_CYCLE: i32 = 0xFF + (1 << 3);

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
        self.generation.fetch_add(u64::from(GENERATION_DELTA), Ordering::Relaxed);
    }

    /// The current generation byte.
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
                // Refresh the generation so a hit survives the next replacement sweep,
                // keeping the bound bits: upstream's `entry->save` refresh path.
                let refreshed = (self.generation8() & GENERATION_MASK) | (gen_bound & 0x7);
                c.meta.store(pack_meta(meta, slot, depth8, refreshed), Ordering::Relaxed);
                return TTProbe {
                    hit: true,
                    data: decode(main, depth8, gen_bound),
                    cluster,
                    slot,
                    key16,
                };
            }
        }

        // Miss: pick the replacement victim now, by upstream's rule -- prefer the entry
        // whose depth, discounted by how many generations old it is, is lowest.
        let mut victim = 0usize;
        let mut worst = i32::MAX;
        for slot in 0..CLUSTER_SIZE {
            let (depth8, gen_bound) = unpack_meta(meta, slot);
            let age = (GENERATION_CYCLE + i32::from(self.generation8())
                - i32::from(gen_bound & GENERATION_MASK))
                & GENERATION_MASK as i32;
            let score = i32::from(depth8) - age * 2;
            if score < worst {
                worst = score;
                victim = slot;
            }
        }

        TTProbe { hit: false, data: empty_data(), cluster, slot: victim, key16 }
    }

    /// Write an entry back into the slot `probe` selected.
    ///
    /// The three conditions under which an existing entry survives are upstream's: an
    /// exact bound always overwrites, a deeper search overwrites a shallower one, and a
    /// stale generation is replaceable regardless of depth.
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
        let occupied = (main as u16) == probe.key16 && old_depth8 != 0;

        // Keep the previous move when the new search found none: a move from a shallower
        // search still orders better than nothing.
        let mv = if !mv.is_none() || !occupied { mv } else { Move::from_raw((main >> 16) as u16) };

        if bound == Bound::Exact
            || !occupied
            || i32::from(old_depth8) - DEPTH_ENTRY_OFFSET + 2 * i32::from(is_pv)
                < depth - DEPTH_ENTRY_OFFSET + 4
            || (old_gen & GENERATION_MASK) != (self.generation8() & GENERATION_MASK)
        {
            let depth8 = (depth - DEPTH_ENTRY_OFFSET).clamp(0, 255) as u8;
            let gen_bound =
                (self.generation8() & GENERATION_MASK) | (u8::from(is_pv) << 2) | bound as u8;
            let packed = u64::from(probe.key16)
                | (u64::from(mv.raw()) << 16)
                | (u64::from(value as i16 as u16) << 32)
                | (u64::from(eval as i16 as u16) << 48);
            c.main[probe.slot].store(packed, Ordering::Relaxed);
            c.meta.store(pack_meta(meta, probe.slot, depth8, gen_bound), Ordering::Relaxed);
        }
    }

    /// Per-mille of the table written in the current generation, as UCI's `hashfull`.
    ///
    /// Sampled over the first thousand clusters, exactly as upstream does: a full sweep of
    /// a gigabyte table would cost more than the information is worth.
    #[must_use]
    pub fn hashfull(&self) -> u32 {
        let n = 1000.min(self.clusters.len());
        if n == 0 {
            return 0;
        }
        let current = self.generation8() & GENERATION_MASK;
        let mut used = 0u32;
        for c in &self.clusters[..n] {
            let meta = c.meta.load(Ordering::Relaxed);
            for slot in 0..CLUSTER_SIZE {
                let (depth8, gen_bound) = unpack_meta(meta, slot);
                if depth8 != 0 && (gen_bound & GENERATION_MASK) == current {
                    used += 1;
                }
            }
        }
        used / (n as u32 * CLUSTER_SIZE as u32 / 1000).max(1)
    }
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
        depth: i32::from(depth8) + DEPTH_ENTRY_OFFSET,
        bound: Bound::from_raw(gen_bound),
        is_pv: gen_bound & 0x4 != 0,
    }
}

fn empty_data() -> TTData {
    TTData {
        mv: Move::NONE,
        value: VALUE_NONE,
        eval: VALUE_NONE,
        depth: DEPTH_ENTRY_OFFSET,
        bound: Bound::None,
        is_pv: false,
    }
}

/// Convert a score for storage: a mate score is stored as distance from the CURRENT node,
/// not from the root, so the same entry is correct wherever it is found again.
#[inline]
#[must_use]
pub fn value_to_tt(v: Value, ply: i32) -> Value {
    debug_assert!(v != VALUE_NONE);
    if v >= VALUE_MATE_IN_MAX_PLY {
        v + ply
    } else if v <= VALUE_MATED_IN_MAX_PLY {
        v - ply
    } else {
        v
    }
}

/// The inverse of [`value_to_tt`], with upstream's fifty-move guard: a mate score found
/// under a nearly expired halfmove clock is demoted, because the rule may draw the game
/// before the mate arrives.
#[inline]
#[must_use]
pub fn value_from_tt(v: Value, ply: i32, rule50: i32) -> Value {
    if v == VALUE_NONE {
        return VALUE_NONE;
    }
    if v >= VALUE_MATE_IN_MAX_PLY {
        if v >= crate::board::types::VALUE_MATE - MAX_PLY as i32
            && crate::board::types::VALUE_MATE - v > 100 - rule50
        {
            return VALUE_MATE_IN_MAX_PLY - 1;
        }
        return v - ply;
    }
    if v <= VALUE_MATED_IN_MAX_PLY {
        if v <= -crate::board::types::VALUE_MATE + MAX_PLY as i32
            && crate::board::types::VALUE_MATE + v > 100 - rule50
        {
            return VALUE_MATED_IN_MAX_PLY + 1;
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
        assert_eq!(tt.hashfull(), 0);
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
        assert!(tt.hashfull() > 0);
    }
}
