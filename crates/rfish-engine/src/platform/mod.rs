//! Platform services: threads and tablebases.
//!
//! # What is NOT here
//!
//! Upstream's `platform` layer carries a large-page allocator, a NUMA topology model and a
//! memory-mapping wrapper. None of those appear here, and their absence is the point:
//!
//! - **Large pages and aligned allocation** — the transposition table is a `Vec` of
//!   naturally aligned atomic words. Rust's allocator already honours the type's
//!   alignment, so there is nothing to hand-roll.
//! - **Memory mapping** — a mapping is `unsafe` in Rust (the file can change under the
//!   map). The Syzygy prober reads its tables with ordinary file reads instead.
//! - **A thread pool** — [`threads`] uses [`std::thread::scope`], which lends each worker a
//!   `&mut` to its own state for exactly the duration of the search. There is no pool to
//!   own, no join to forget, and no lifetime to assert by hand.
//!
//! See `docs/06-platform.md`.

pub mod syzygy;
pub mod threads;
