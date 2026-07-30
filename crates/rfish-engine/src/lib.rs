//! rfish — a safe-Rust port of the Stockfish chess engine.
//!
//! This crate is the ENGINE: the board, the search, the evaluation and the tablebase
//! prober. It reads no standard input, writes no standard output and parses no UCI. The
//! shell that does those things is a separate crate, and the crate boundary is what
//! enforces the split — upstream needs a link-time check for the same property, and the
//! sibling C port has to run one as a gate.
//!
//! # The one rule
//!
//! `#![forbid(unsafe_code)]` is set for the whole workspace in `Cargo.toml`. There is no
//! `unsafe` block anywhere in rfish, and no module can opt out with a local `#[allow]`.
//! Everything the C++ does with raw pointers is expressed with slices, indices, atomics
//! and scoped threads instead. See `docs/08-idiomatic-rust.md` for the pattern-by-pattern
//! translation.
//!
//! # Zones
//!
//! - [`board`] — the value domain, bitboards, attacks, position, move generation.
//! - [`state`] — the state blocks the search and the evaluation share.
//! - [`eval`] — the NNUE network and its incremental accumulator.
//! - [`search`] — the search, the move picker, the histories, the transposition table.
//! - [`platform`] — threads, timing, memory, and the Syzygy prober.
//!
//! Reading order is `docs/00-architecture.md`.

pub mod board;
pub mod eval;
pub mod platform;
pub mod search;
pub mod state;

pub use board::position::Position;
pub use board::types::{Color, Move, Piece, PieceType, Square, Value};

/// The engine version, as reported by the UCI `id name` line.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
