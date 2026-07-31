//! A randomised walk that drives the real search, for finding what fixed cases do not.
//!
//! # Why this is not libFuzzer
//!
//! ../mcfish gets this coverage from `tools/fuzz_search.c`, which links the engine against
//! libFuzzer's driver. rfish cannot: `libfuzzer-sys` is a dependency where the engine crate
//! has none, and its `fuzz_target!` expands to a `#[unsafe(no_mangle)]` export, which
//! `forbid(unsafe_code)` rejects outright. Neither is negotiable, so the harness is a seeded
//! PRNG instead of a coverage-guided mutator.
//!
//! What that loses is guidance — this walks at random rather than steering toward new
//! branches. What it keeps is the part that actually finds bugs here: real positions, off
//! any golden or bench list, driven through the whole spine. And it gains something
//! libFuzzer would not give: run under the `gate` profile, `debug_assert!` and
//! `overflow-checks` are both ON, so a violated search invariant or an unintended wrap is a
//! FAILURE rather than a plausible wrong number. That is this port's equivalent of the
//! sanitiser build the sibling runs its fuzzer under.
//!
//! # Why a random walk rather than a FEN table
//!
//! A table is a list someone wrote down, and a port is most wrong in the positions nobody
//! thought to write down. Walking from the start position by random legal moves reaches
//! castling rights part-expired, en passant available and declined, repetition, promotion
//! into a pinned piece, and the fifty-move counter mid-run — states no curated list covers.
//! ../mcfish's `tools/upstream_nodes.py` walks for the same reason.

use crate::board::movegen::{GenType, generate};
use crate::board::position::{Position, START_FEN};
use crate::board::types::Move;
use crate::platform::threads::ThreadPool;
use crate::search::tt::TranspositionTable;
use crate::search::worker::{SearchResult, SilentSink};
use crate::state::{Limits, SearchOptions};

/// Every legal move in `pos`.
///
/// The generators are PSEUDO-legal by design — the search filters as it goes, because most
/// generated moves are never searched — so a harness that wants the real move list has to
/// filter for itself. Evasions when in check, everything otherwise, which is the split the
/// generator itself makes.
fn legal_moves(pos: &Position) -> Vec<Move> {
    let gt = if pos.checkers().any() { GenType::Evasions } else { GenType::NonEvasions };
    generate(pos, gt).iter().copied().filter(|&m| pos.legal(m)).collect()
}

/// A seeded xorshift, so a failure reproduces from the seed the run printed.
///
/// The engine already carries one for `Skill`; this is deliberately a second, private one,
/// because a harness sharing the subject's PRNG state would couple what it tests to how it
/// tests it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One walk: play random legal moves, then search the position that results.
///
/// Returns the search result so a caller can assert on it. A position with no legal moves
/// ends the walk early — that is checkmate or stalemate, and searching it is still valid.
fn one_walk(rng: &mut Rng, plies: usize, depth: i32) -> (Position, SearchResult) {
    let mut pos = Position::from_fen(START_FEN, false).expect("the start position parses");

    for _ in 0..plies {
        let list = legal_moves(&pos);
        if list.is_empty() {
            break;
        }
        let m = list[rng.below(list.len())];
        pos.do_move(m);
    }

    let tt = TranspositionTable::new(1);
    let mut pool = ThreadPool::new(1);
    let limits = Limits { depth: Some(depth), ply: pos.game_ply(), ..Limits::default() };
    let result = pool.search(&pos, &limits, &tt, &SearchOptions::default(), &mut SilentSink);
    (pos, result)
}

/// Drive `walks` random positions through the search and check what must hold of every one.
///
/// The assertions are deliberately few and total. A search that returns an illegal move
/// would be caught by no golden, because a golden only pins the positions in it.
pub fn run(seed: u64, walks: usize, plies: usize, depth: i32) {
    let mut rng = Rng(seed.max(1));

    for i in 0..walks {
        let (pos, result) = one_walk(&mut rng, plies, depth);

        let legal = legal_moves(&pos);

        if legal.is_empty() {
            // Mate or stalemate: there is nothing to play and the search must not invent
            // one. `Move::NONE` is what upstream reports here.
            assert!(
                result.best_move == Move::NONE,
                "seed {seed} walk {i}: a move was returned in a position with none",
            );
            continue;
        }

        assert!(
            legal.contains(&result.best_move),
            "seed {seed} walk {i}: best move is not legal in the position it was found for",
        );
        assert!(result.nodes > 0, "seed {seed} walk {i}: a searched position counted no nodes");
        if let Some(ponder) = result.ponder_move {
            assert!(
                ponder != Move::NONE,
                "seed {seed} walk {i}: a null ponder move was reported as present",
            );
        }
    }
}

/// Walk until `seconds` have passed, returning how many positions were searched.
///
/// The budgeted twin of [`run`], for a scheduled job rather than a gate. A clean run means
/// "nothing failed in that budget", NOT "there is nothing to find" — the same thing the
/// sibling's fuzz step is careful to say about its own.
#[must_use]
pub fn run_for(seed: u64, seconds: u64, plies: usize, depth: i32) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut rng = Rng(seed.max(1));
    let mut walks = 0usize;
    while std::time::Instant::now() < deadline {
        let (pos, result) = one_walk(&mut rng, plies, depth);
        let legal = legal_moves(&pos);
        if legal.is_empty() {
            assert!(result.best_move == Move::NONE, "seed {seed}: a move was returned with none");
        } else {
            assert!(
                legal.contains(&result.best_move),
                "seed {seed} walk {walks}: best move is not legal in its own position",
            );
        }
        walks += 1;
    }
    walks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed seed, so this test is a gate rather than a lottery: it either always passes
    /// or always fails, and CI reports the same thing twice.
    #[test]
    fn a_random_walk_searches_legally() {
        run(0x9E37_79B9_7F4A_7C15, 12, 16, 4);
    }

    /// Deeper walks reach the states a short one cannot: expired castling rights, the
    /// fifty-move counter mid-run, repetition.
    #[test]
    fn a_long_walk_reaches_the_late_game_states() {
        run(0xD1B5_4A32_D192_ED03, 4, 90, 3);
    }

    /// The scheduled soak. `#[ignore]` because it spends a wall-clock budget rather than
    /// asserting a fact, so it must not run in the ordinary suite; `cargo xtask fuzz` runs
    /// it with `--ignored` and supplies the budget and the seed.
    ///
    /// The seed comes from the clock when nothing supplies one, so a scheduled run broadens
    /// coverage instead of re-walking the same positions — and it is PRINTED, because a
    /// failure is only actionable if it can be replayed.
    #[test]
    #[ignore = "spends a wall-clock budget; run via `cargo xtask fuzz`"]
    fn soak() {
        let seconds = env_u64("RFISH_FUZZ_SECONDS").unwrap_or(30);
        let seed = env_u64("RFISH_FUZZ_SEED").unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64)
        });
        println!("fuzz soak: seed {seed}, {seconds}s -- replay with RFISH_FUZZ_SEED={seed}");
        let walks = run_for(seed, seconds, 40, 4);
        println!("fuzz soak: {walks} positions searched, no failures");
        assert!(walks > 0, "the budget expired before a single position was searched");
    }

    fn env_u64(key: &str) -> Option<u64> {
        std::env::var(key).ok()?.parse().ok()
    }
}
