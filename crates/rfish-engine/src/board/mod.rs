//! The board zone: the value domain, the bitboards, the attack tables, the position and
//! the move generator.
//!
//! Nothing here knows about search, evaluation, UCI or threads. The dependency runs one
//! way — search and eval read the board; the board reads nothing back — and that is what
//! makes perft a complete test of this zone.
//!
//! See `docs/01-engine-board.md`.

pub mod attacks;
pub mod bitboard;
pub mod movegen;
pub mod position;
pub mod types;
pub mod zobrist;
