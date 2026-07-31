//! The search zone: the transposition table, the histories, the move picker, the time
//! manager, and the alpha-beta search itself.
//!
//! See `docs/02-engine-search.md`.

/// A randomised walk that drives the real search. See the module docs for why it is a
/// seeded PRNG rather than libFuzzer.
pub mod fuzz;
pub mod history;
pub mod movepick;
pub mod score;
pub mod skill;
pub mod timeman;
pub mod tt;
pub mod worker;

pub use worker::{SearchResult, SearchWorker};
