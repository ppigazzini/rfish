//! The NUMA topology model: which processors exist, how they group into nodes, and how a
//! thread set is distributed across them.
//!
//! # Read from the filesystem, not from libc
//!
//! Upstream discovers all of this through `sched_getaffinity` and `GetNumaProcessorNodeEx`.
//! Both are FFI, and rfish has neither `unsafe` nor a dependency to reach them through. It
//! does not need to: Linux exposes exactly the same facts as text under `/sys` and `/proc`,
//! and reading a file is safe.
//!
//! | upstream | here |
//! |---|---|
//! | `sched_getaffinity(0, …)` | `/proc/self/status`, `Cpus_allowed_list` |
//! | `/sys/devices/system/node/online` | the same file |
//! | `/sys/…/node<N>/cpulist` | the same file |
//! | `/sys/…/cpu<N>/cache/index3/shared_cpu_list` | the same file |
//!
//! The kernel documents all four as stable interfaces, and the strings they hold are in the
//! very format upstream already parses.
//!
//! # What is NOT here: binding
//!
//! `sched_setaffinity` has no filesystem equivalent, so rfish cannot PIN a thread to a
//! node. Everything below — the topology, the policies, the distribution — is still real
//! and is still what decides which replica a worker reads. What rfish cannot do is stop the
//! scheduler from moving that worker somewhere else afterwards. See
//! [`NumaConfig::suggests_binding_threads`], whose answer the shell reports honestly rather
//! than claiming a binding it did not perform.
//!
//! Golden: `Stockfish/src/numa.h`.

use std::collections::{BTreeMap, BTreeSet};

/// A processor number, in the system's own numbering.
///
/// A newtype rather than an alias: `add_cpu_to_node(n, c)` took two adjacent `usize`s, and
/// two names for one type prevent exactly nothing.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CpuIndex(usize);

impl CpuIndex {
    /// Build from a processor number read out of `/sys` or a policy string.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> CpuIndex {
        CpuIndex(i)
    }

    /// The number, for printing and for the affinity arithmetic.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

impl core::fmt::Display for CpuIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// A node number in THIS config, which need not be the system's node number: the L3
/// policies subdivide a system node, and an explicit policy invents its own.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NumaIndex(usize);

impl NumaIndex {
    /// Build from a node number.
    #[inline(always)]
    #[must_use]
    pub const fn new(i: usize) -> NumaIndex {
        NumaIndex(i)
    }

    /// The number, for printing and for indexing the node list.
    #[inline(always)]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

impl core::fmt::Display for NumaIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// How `from_system` should partition the processors it finds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoPolicy {
    /// Use the system's own NUMA nodes, unmodified.
    System,
    /// Use the system-reported L3 domains, so a processor with a split L3 inside one NUMA
    /// node is treated as several nodes.
    L3Domains,
    /// Merge adjacent L3 domains until each group reaches `bundle_size` processors.
    BundledL3 { bundle_size: usize },
}

/// Upstream's default: bundle L3 domains up to 32 processors.
///
/// A modern many-core part has one L3 per core-complex, which is far finer than the memory
/// topology; replicating per complex would cost more in memory than the locality returns.
pub const DEFAULT_AUTO_POLICY: AutoPolicy = AutoPolicy::BundledL3 { bundle_size: 32 };

/// The upper bound on how many indices one `first-last` range may expand to.
///
/// A range is user input — it arrives from a `NumaPolicy` string — and `0-99999999999`
/// would otherwise ask for an allocation measured in gigabytes before anything validated
/// it. Upstream caps it at the same place and for the same reason.
const MAX_INDICES: usize = 1 << 20;

/// The upper bound on how many processors ONE policy string may name in total.
///
/// **This bound is not upstream's, and it is here deliberately.** Upstream caps each
/// `first-last` range and nothing else, so a policy naming many ranges multiplies straight
/// past the cap: forty ranges of a million is 735 bytes of UCI input and forty million
/// processors, measured here at 2.8 GB resident before the allocator gave up. A sibling
/// port lost a WSL2 VM and a CI runner to exactly this shape — an option value that
/// quietly becomes an allocation — so the value is bounded where it is parsed.
///
/// A million processors is already four orders of magnitude past any real machine, so this
/// refuses no reachable topology.
const MAX_TOTAL_CPUS: usize = 1 << 20;

/// Parse upstream's shortened index list: `0,2,4-7` and so on.
///
/// Malformed pieces are DROPPED rather than rejected, which is upstream's behaviour: the
/// caller decides whether "nothing parsed" is an error, and for a sysfs file that is empty
/// the right answer is an empty list rather than a failure.
#[must_use]
pub fn indices_from_shortened_string(s: &str) -> Vec<usize> {
    let mut indices = Vec::new();
    if s.is_empty() {
        return indices;
    }

    for part in s.split(',') {
        if part.is_empty() {
            continue;
        }
        let bounds: Vec<&str> = part.split('-').collect();
        match bounds.len() {
            1 => {
                if let Ok(c) = bounds[0].parse::<usize>() {
                    indices.push(c);
                }
            }
            2 => {
                let (first, last) = (bounds[0].parse::<usize>(), bounds[1].parse::<usize>());
                if let (Ok(first), Ok(last)) = (first, last) {
                    // Written as a subtraction on the parsed values, so a reversed range
                    // wraps to something enormous and is refused by the same bound that
                    // refuses an honestly enormous one.
                    if last.wrapping_sub(first) < MAX_INDICES {
                        for c in first..=last {
                            indices.push(c);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    indices
}

/// Read a file, trimmed of surrounding whitespace.
///
/// A sysfs file for an EMPTY node still exists and still contains a newline, so "read
/// succeeded but there is nothing in it" is a normal answer and must be distinguishable
/// from "the file is not there".
fn read_sys_string(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// The processors this process is allowed to run on, from `/proc/self/status`.
///
/// Upstream reads the same mask through `sched_getaffinity`. Taken once at startup and
/// never re-read, exactly as upstream does: a config that changed under the engine would
/// make the engine's own behaviour depend on when it happened to look.
#[must_use]
pub fn startup_process_affinity() -> Option<BTreeSet<CpuIndex>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Cpus_allowed_list:"))?;
    let list = line.split_once(':')?.1.trim();
    let set: BTreeSet<CpuIndex> =
        indices_from_shortened_string(list).into_iter().map(CpuIndex::new).collect();
    if set.is_empty() { None } else { Some(set) }
}

/// One L3 cache domain, and which system NUMA node it belongs to.
#[derive(Clone, Debug)]
struct L3Domain {
    system_node: NumaIndex,
    cpus: BTreeSet<CpuIndex>,
}

/// A partition of the available processors into NUMA nodes.
///
/// Immutable once built. Upstream says why: there is no useful way to alter one that would
/// not require rebuilding it anyway, and the invariant that no node is empty is much easier
/// to hold at construction than to maintain.
#[derive(Clone, Debug)]
pub struct NumaConfig {
    nodes: Vec<BTreeSet<CpuIndex>>,
    node_by_cpu: BTreeMap<CpuIndex, NumaIndex>,
    /// True when this config does NOT come from the system as-is — an explicit policy
    /// string, or a deliberate refusal to respect the process affinity. It forces
    /// replication on, because the engine can no longer assume the OS agrees with it.
    custom_affinity: bool,
}

impl Default for NumaConfig {
    /// Every processor in one node, which is what a system with no NUMA support looks like.
    fn default() -> NumaConfig {
        let mut cfg = NumaConfig::empty();
        let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        for c in 0..n {
            cfg.add_cpu_to_node(NumaIndex::new(0), CpuIndex::new(c));
        }
        cfg
    }
}

impl NumaConfig {
    /// A config with no processors in it.
    #[must_use]
    pub fn empty() -> NumaConfig {
        NumaConfig { nodes: Vec::new(), node_by_cpu: BTreeMap::new(), custom_affinity: false }
    }

    /// Assign `c` to node `n`. False when `c` already belongs to a node, in which case
    /// nothing changed.
    fn add_cpu_to_node(&mut self, n: NumaIndex, c: CpuIndex) -> bool {
        if self.is_cpu_assigned(c) {
            return false;
        }
        while self.nodes.len() <= n.index() {
            self.nodes.push(BTreeSet::new());
        }
        self.nodes[n.index()].insert(c);
        self.node_by_cpu.insert(c, n);
        true
    }

    /// Drop the nodes with nothing in them, renumbering what is left.
    ///
    /// A system node with every processor masked out of the process affinity is real and
    /// empty, and the rest of the model is written assuming no node is empty.
    fn remove_empty_numa_nodes(&mut self) {
        let mut kept: Vec<BTreeSet<CpuIndex>> = Vec::new();
        for node in std::mem::take(&mut self.nodes) {
            if !node.is_empty() {
                kept.push(node);
            }
        }
        self.nodes = kept;
        self.node_by_cpu.clear();
        for (n, cpus) in self.nodes.iter().enumerate() {
            for &c in cpus {
                self.node_by_cpu.insert(c, NumaIndex::new(n));
            }
        }
    }

    /// Discover the topology from the running system.
    ///
    /// `respect_process_affinity` false means the caller wants the machine's whole topology
    /// even though this process has been restricted to part of it — which is by definition
    /// inconsistent with what the OS will actually allow, so the result is marked custom.
    #[must_use]
    pub fn from_system(policy: AutoPolicy, respect_process_affinity: bool) -> NumaConfig {
        let allowed = if respect_process_affinity { startup_process_affinity() } else { None };
        let is_allowed = |c: CpuIndex| allowed.as_ref().is_none_or(|set| set.contains(&c));

        let mut cfg = None;
        if policy != AutoPolicy::System {
            let bundle = match policy {
                AutoPolicy::BundledL3 { bundle_size } => bundle_size,
                _ => 0,
            };
            cfg = Self::try_l3_aware(respect_process_affinity, bundle, &is_allowed);
        }
        let mut cfg = cfg.unwrap_or_else(|| Self::from_system_numa(&is_allowed));

        cfg.remove_empty_numa_nodes();
        if !respect_process_affinity {
            cfg.custom_affinity = true;
        }
        cfg
    }

    /// The system's own NUMA nodes, from sysfs.
    fn from_system_numa(is_allowed: &impl Fn(CpuIndex) -> bool) -> NumaConfig {
        let mut cfg = NumaConfig::empty();
        let mut fallback = true;

        if let Some(online) = read_sys_string("/sys/devices/system/node/online") {
            fallback = false;
            for n in indices_from_shortened_string(&online) {
                let path = format!("/sys/devices/system/node/node{n}/cpulist");
                // Only a MISSING file is a reason to give up on sysfs. An empty node has a
                // file that reads as whitespace, and that is a legitimate answer.
                let Some(cpus) = read_sys_string(&path) else {
                    fallback = true;
                    break;
                };
                for c in indices_from_shortened_string(&cpus).into_iter().map(CpuIndex::new) {
                    if is_allowed(c) {
                        cfg.add_cpu_to_node(NumaIndex::new(n), c);
                    }
                }
            }
        }

        if fallback {
            cfg = NumaConfig::empty();
            let total = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            for c in (0..total).map(CpuIndex::new) {
                if is_allowed(c) {
                    cfg.add_cpu_to_node(NumaIndex::new(0), c);
                }
            }
        }
        cfg
    }

    /// Subdivide the system nodes by L3 domain, bundling up to `bundle_size` processors.
    ///
    /// `None` when the machine reports no L3 sharing information, which is the normal
    /// answer inside a container or a VM that hides the cache topology.
    fn try_l3_aware(
        respect_process_affinity: bool,
        bundle_size: usize,
        is_allowed: &impl Fn(CpuIndex) -> bool,
    ) -> Option<NumaConfig> {
        // The system view first, so each L3 domain knows which memory node it sits on.
        let system = NumaConfig::from_system(AutoPolicy::System, respect_process_affinity);

        let mut domains: Vec<L3Domain> = Vec::new();
        let mut seen: BTreeSet<CpuIndex> = BTreeSet::new();

        for (&cpu, &system_node) in &system.node_by_cpu {
            if seen.contains(&cpu) {
                continue;
            }
            let path =
                format!("/sys/devices/system/cpu/cpu{}/cache/index3/shared_cpu_list", cpu.index());
            let Some(siblings) = read_sys_string(&path).filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut domain = L3Domain { system_node, cpus: BTreeSet::new() };
            for c in indices_from_shortened_string(&siblings).into_iter().map(CpuIndex::new) {
                if is_allowed(c) {
                    domain.cpus.insert(c);
                }
                seen.insert(c);
            }
            if !domain.cpus.is_empty() {
                domains.push(domain);
            }
        }

        if domains.is_empty() {
            return None;
        }
        Some(Self::from_l3_info(domains, bundle_size))
    }

    /// Group L3 domains into nodes, merging neighbours while they fit inside `bundle_size`.
    fn from_l3_info(domains: Vec<L3Domain>, bundle_size: usize) -> NumaConfig {
        let mut by_system: BTreeMap<NumaIndex, Vec<BTreeSet<CpuIndex>>> = BTreeMap::new();
        for d in domains {
            by_system.entry(d.system_node).or_default().push(d.cpus);
        }

        let mut cfg = NumaConfig::empty();
        let mut n = 0;
        for (_, mut ds) in by_system {
            // Merge adjacent pairs repeatedly. With roughly equal domain sizes this
            // distributes evenly; each pass strictly shrinks `ds`, so it terminates.
            let mut changed = true;
            while changed {
                changed = false;
                let mut j = 0;
                while j + 1 < ds.len() {
                    if ds[j].len() + ds[j + 1].len() <= bundle_size {
                        let merged = ds.remove(j + 1);
                        ds[j].extend(merged);
                        changed = true;
                    } else {
                        j += 1;
                    }
                }
            }
            for d in ds {
                for c in d {
                    cfg.add_cpu_to_node(NumaIndex::new(n), c);
                }
                n += 1;
            }
        }
        cfg
    }

    /// Parse an explicit policy: nodes separated by `:`, processors by `,`, ranges by `-`.
    ///
    /// `None` when a processor is named twice or when nothing parsed at all. Both mean the
    /// operator asked for something the engine cannot honour, and upstream keeps the
    /// previous config rather than guessing.
    #[must_use]
    pub fn from_string(s: &str) -> Option<NumaConfig> {
        let mut cfg = NumaConfig::empty();
        let mut n = 0;
        let mut total = 0usize;
        for node_str in s.split(':') {
            let indices = indices_from_shortened_string(node_str);
            if indices.is_empty() {
                continue;
            }
            total += indices.len();
            if total > MAX_TOTAL_CPUS {
                return None;
            }
            for idx in indices {
                if !cfg.add_cpu_to_node(NumaIndex::new(n), CpuIndex::new(idx)) {
                    return None;
                }
            }
            n += 1;
        }
        if n == 0 {
            return None;
        }
        cfg.custom_affinity = true;
        Some(cfg)
    }

    /// True when `c` belongs to some node.
    #[must_use]
    pub fn is_cpu_assigned(&self, c: CpuIndex) -> bool {
        self.node_by_cpu.contains_key(&c)
    }

    /// How many nodes this config has.
    #[must_use]
    pub fn num_numa_nodes(&self) -> usize {
        self.nodes.len()
    }

    /// How many processors node `n` holds.
    #[must_use]
    pub fn num_cpus_in_numa_node(&self, n: NumaIndex) -> usize {
        self.nodes.get(n.index()).map_or(0, BTreeSet::len)
    }

    /// How many processors this config covers.
    #[must_use]
    pub fn num_cpus(&self) -> usize {
        self.node_by_cpu.len()
    }

    /// True when read-only data is worth holding once per node rather than once overall.
    #[must_use]
    pub fn requires_memory_replication(&self) -> bool {
        self.custom_affinity || self.nodes.len() > 1
    }

    /// The config in the same syntax `from_string` accepts, which is what the shell reports.
    #[must_use]
    pub fn to_string_spec(&self) -> String {
        let mut out = String::new();
        for (i, cpus) in self.nodes.iter().enumerate() {
            if i > 0 {
                out.push(':');
            }
            let list: Vec<CpuIndex> = cpus.iter().copied().collect();
            let mut first_set = true;
            let mut start = 0usize;
            for j in 0..list.len() {
                let at_end = j + 1 == list.len() || list[j + 1].index() != list[j].index() + 1;
                if !at_end {
                    continue;
                }
                if !first_set {
                    out.push(',');
                }
                if j == start {
                    out.push_str(&list[j].to_string());
                } else {
                    out.push_str(&list[start].to_string());
                    out.push('-');
                    out.push_str(&list[j].to_string());
                }
                start = j + 1;
                first_set = false;
            }
        }
        out
    }

    /// Whether spreading `num_threads` across the nodes is worth doing.
    ///
    /// The question is not "is this machine NUMA" but "will the OS keep these threads on
    /// one node anyway". A single thread never benefits. A thread set that fits comfortably
    /// inside the largest node does not either, because the scheduler will keep it there
    /// and every replica but one would be dead weight.
    #[must_use]
    pub fn suggests_binding_threads(&self, num_threads: usize) -> bool {
        // Small nodes are ignored when counting: a machine often reports a node with a
        // handful of processors that no sensible distribution would target.
        const SMALL_NODE_THRESHOLD: f64 = 0.6;

        // An affinity the operator set by hand may disagree with the one the OS reports,
        // and only binding can reconcile them.
        if self.custom_affinity {
            return true;
        }
        if num_threads <= 1 {
            return false;
        }

        let largest = self.nodes.iter().map(BTreeSet::len).max().unwrap_or(0);
        let is_small = |node: &BTreeSet<CpuIndex>| {
            largest == 0 || node.len() as f64 / largest as f64 <= SMALL_NODE_THRESHOLD
        };
        let not_small = self.nodes.iter().filter(|n| !is_small(n)).count();

        (num_threads > largest / 2 || num_threads >= not_small * 4) && self.nodes.len() > 1
    }

    /// Which node each of `num_threads` threads belongs to.
    ///
    /// Fills the node with the most spare capacity each time, measured as the fraction of
    /// its processors already taken, so an uneven machine still comes out proportional.
    #[must_use]
    pub fn distribute_threads_among_numa_nodes(&self, num_threads: usize) -> Vec<NumaIndex> {
        if self.nodes.len() <= 1 {
            return vec![NumaIndex::new(0); num_threads];
        }

        let mut occupation = vec![0usize; self.nodes.len()];
        let mut out = Vec::with_capacity(num_threads);
        for _ in 0..num_threads {
            let mut best = 0usize;
            let mut best_fill = f64::MAX;
            for (n, taken) in occupation.iter().enumerate() {
                let capacity = self.nodes[n].len().max(1);
                let fill = *taken as f64 / capacity as f64;
                if fill < best_fill {
                    best_fill = fill;
                    best = n;
                }
            }
            occupation[best] += 1;
            out.push(NumaIndex::new(best));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shortened_list_parses_singles_and_ranges() {
        assert_eq!(indices_from_shortened_string("0"), vec![0]);
        assert_eq!(indices_from_shortened_string("0,2,4"), vec![0, 2, 4]);
        assert_eq!(indices_from_shortened_string("2-5"), vec![2, 3, 4, 5]);
        assert_eq!(indices_from_shortened_string("0-1,4,6-7"), vec![0, 1, 4, 6, 7]);
        assert_eq!(indices_from_shortened_string(""), Vec::<usize>::new());
    }

    /// A range is user input. An enormous one must be refused rather than allocated, and a
    /// REVERSED one must not be read as enormous-by-wraparound either.
    #[test]
    fn an_unreasonable_range_is_refused_rather_than_allocated() {
        assert!(indices_from_shortened_string("0-99999999999").is_empty());
        assert!(indices_from_shortened_string("5-1").is_empty());
        // The boundary itself still parses, so the cap is a cap and not an off-by-one.
        assert_eq!(
            indices_from_shortened_string(&format!("0-{}", MAX_INDICES - 2)).len(),
            MAX_INDICES - 1
        );
    }

    #[test]
    fn a_malformed_piece_is_dropped_not_fatal() {
        assert_eq!(indices_from_shortened_string("0,zzz,2"), vec![0, 2]);
        assert_eq!(indices_from_shortened_string("1-2-3"), Vec::<usize>::new());
    }

    #[test]
    fn an_explicit_policy_round_trips_through_its_own_syntax() {
        let cfg = NumaConfig::from_string("0-3:4-7").expect("valid spec");
        assert_eq!(cfg.num_numa_nodes(), 2);
        assert_eq!(cfg.num_cpus(), 8);
        assert_eq!(cfg.num_cpus_in_numa_node(NumaIndex::new(0)), 4);
        assert_eq!(cfg.to_string_spec(), "0-3:4-7");
        // An explicit policy is custom by definition, so it always replicates.
        assert!(cfg.requires_memory_replication());
    }

    /// A policy whose RANGES are each legal but whose SUM is not must be refused before it
    /// allocates. Upstream accepts this shape; measured here, 735 bytes of input reached
    /// 2.8 GB resident and then aborted.
    #[test]
    fn a_policy_naming_more_processors_than_any_machine_has_is_refused() {
        let many: Vec<String> = (0..40)
            .map(|i: usize| format!("{}-{}", i * 1_100_000, i * 1_100_000 + 1_048_573))
            .collect();
        assert!(
            NumaConfig::from_string(&many.join(",")).is_none(),
            "the SUM must be bounded, not only each range"
        );
        // Split across nodes rather than within one -- the other way to write it.
        assert!(NumaConfig::from_string(&many.join(":")).is_none());

        // The bound is a bound, not a blanket refusal: a real machine still parses.
        let real = NumaConfig::from_string("0-63:64-127").expect("a 128-way machine is fine");
        assert_eq!(real.num_cpus(), 128);
    }

    #[test]
    fn a_processor_named_twice_is_refused() {
        assert!(NumaConfig::from_string("0-3:3-7").is_none());
        assert!(NumaConfig::from_string("").is_none());
        assert!(NumaConfig::from_string("nonsense").is_none());
    }

    #[test]
    fn the_spec_string_collapses_runs_but_not_gaps() {
        let cfg = NumaConfig::from_string("0,1,2,5:8").expect("valid spec");
        assert_eq!(cfg.to_string_spec(), "0-2,5:8");
    }

    /// A single SYSTEM node must never suggest binding, whatever the thread count: there
    /// is nowhere else to put the threads, and a replica per node would be one replica.
    ///
    /// An explicitly configured single node is a different question and answers yes, because
    /// a hand-set affinity may disagree with the OS's and only binding reconciles them.
    #[test]
    fn one_system_node_never_suggests_binding() {
        let mut cfg = NumaConfig::from_string("0-15").expect("valid spec");
        assert!(cfg.suggests_binding_threads(8), "an explicit config always binds");
        cfg.custom_affinity = false;
        for threads in [1usize, 2, 8, 64] {
            assert!(!cfg.suggests_binding_threads(threads), "{threads} threads");
        }
    }

    #[test]
    fn a_single_thread_never_suggests_binding() {
        let mut cfg = NumaConfig::from_string("0-7:8-15").expect("valid spec");
        cfg.custom_affinity = false;
        assert!(!cfg.suggests_binding_threads(1));
        assert!(cfg.suggests_binding_threads(8));
    }

    #[test]
    fn threads_are_distributed_proportionally_to_node_size() {
        let cfg = NumaConfig::from_string("0-7:8-11").expect("valid spec");
        let d = cfg.distribute_threads_among_numa_nodes(12);
        assert_eq!(d.len(), 12);
        let on0 = d.iter().filter(|&&n| n == NumaIndex::new(0)).count();
        let on1 = d.iter().filter(|&&n| n == NumaIndex::new(1)).count();
        // Eight processors against four, so twice as many threads land on the first node.
        assert_eq!((on0, on1), (8, 4));
    }

    #[test]
    fn one_node_puts_every_thread_on_node_zero() {
        let cfg = NumaConfig::from_string("0-3").expect("valid spec");
        assert_eq!(cfg.distribute_threads_among_numa_nodes(5), vec![NumaIndex::new(0); 5]);
    }

    /// The live system must produce a usable config on any host the tests run on,
    /// including a container that hides the topology entirely.
    #[test]
    fn the_live_system_yields_at_least_one_node_and_one_cpu() {
        for policy in [AutoPolicy::System, AutoPolicy::L3Domains, DEFAULT_AUTO_POLICY] {
            let cfg = NumaConfig::from_system(policy, true);
            assert!(cfg.num_numa_nodes() >= 1, "{policy:?} produced no nodes");
            assert!(cfg.num_cpus() >= 1, "{policy:?} produced no processors");
            // Every node the config exposes holds at least one processor.
            for n in (0..cfg.num_numa_nodes()).map(NumaIndex::new) {
                assert!(cfg.num_cpus_in_numa_node(n) >= 1, "{policy:?} node {n} is empty");
            }
            // And the spec string it reports parses back to the same partition.
            let spec = cfg.to_string_spec();
            let round = NumaConfig::from_string(&spec).expect("the reported spec must parse");
            assert_eq!(round.num_numa_nodes(), cfg.num_numa_nodes(), "{spec}");
            assert_eq!(round.num_cpus(), cfg.num_cpus(), "{spec}");
        }
    }

    /// Refusing to respect the process affinity marks the config custom, because it is
    /// then knowingly inconsistent with what the OS will allow.
    #[test]
    fn ignoring_the_process_affinity_marks_the_config_custom() {
        let cfg = NumaConfig::from_system(AutoPolicy::System, false);
        assert!(cfg.custom_affinity);
        assert!(cfg.requires_memory_replication());
    }

    #[test]
    fn the_default_config_is_one_node_covering_every_processor() {
        let cfg = NumaConfig::default();
        assert_eq!(cfg.num_numa_nodes(), 1);
        assert!(!cfg.requires_memory_replication());
    }
}
