//! Platform services: the worker set, the NUMA topology, and the tablebase prober.
//!
//! This is the runtime that HOSTS the engine rather than a layer beneath it, which is why it
//! may read the board, the search and the evaluation and they may not read it.
//!
//! # What is NOT here
//!
//! Upstream's platform layer also carries a large-page allocator, a memory-mapping wrapper
//! and thread affinity. Those absences are decisions, and each one is a different kind:
//!
//! - **Large pages and aligned allocation** — the transposition table is a `Vec` of
//!   naturally aligned atomic words. Rust's allocator already honours the type's
//!   alignment, so there is nothing to hand-roll.
//! - **Memory mapping** — a mapping is `unsafe` in Rust (the file can change under the
//!   map). The Syzygy prober reads its tables with ordinary file reads instead.
//! - **A thread pool** — [`threads`] uses [`std::thread::scope`], which lends each worker a
//!   `&mut` to its own state for exactly the duration of the search. There is no pool to
//!   own, no join to forget, and no lifetime to assert by hand.
//! - **Thread BINDING** — and this one is blocked rather than redesigned. [`numa`] models
//!   the topology and the policies from file reads, but `sched_setaffinity` has no
//!   filesystem equivalent, so no worker can be pinned to the node it was assigned.
//!
//! See `docs/06-platform.md`.

pub mod numa;
pub mod syzygy;
pub mod threads;
