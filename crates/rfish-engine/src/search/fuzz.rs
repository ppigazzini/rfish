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

/// Walk a random line, checking the board's invariants at every ply, then unwind it.
///
/// Two things at once, because they need the same walk: the per-ply checks below, and the
/// whole line undone move by move at the end. A key that desyncs and resyncs would pass the
/// per-ply check and fail the unwind, which is the shape the fifty-move mixing bug had.
fn walk_and_check(rng: &mut Rng, plies: usize, seed: u64) {
    let mut pos = Position::from_fen(START_FEN, false).expect("the start position parses");
    let key0 = pos.key();
    let board0 = *pos.board();
    let mut played = Vec::with_capacity(plies);

    for ply in 0..plies {
        let list = legal_moves(&pos);
        if list.is_empty() {
            break;
        }
        let tag = format!("ply {ply}");
        check_move_list(&pos, seed, &tag);
        check_make_unmake(&mut pos, seed, &tag);

        let m = list[rng.below(list.len())];
        played.push(m);
        pos.do_move(m);
    }

    while let Some(m) = played.pop() {
        pos.undo_move(m);
    }
    assert_eq!(pos.key(), key0, "seed {seed}: the key did not survive unwinding the whole line");
    assert!(*pos.board() == board0, "seed {seed}: the board did not survive unwinding");
}

/// Parsing must reject or accept, never panic.
///
/// The FEN parser is the engine's only untrusted input besides the network file and the
/// tablebases: a GUI can send anything after `position fen`. Nothing here asserts a
/// PARTICULAR verdict -- a fuzzer has no way to know which strings are legal positions --
/// only that reaching one is not a crash, and that a position it did accept is coherent
/// enough to generate moves from.
fn check_fen_parse(rng: &mut Rng, seed: u64) {
    const ALPHABET: &[u8] = b"rnbqkpRNBQKP12345678/ -abcdefgh0123456789wKQkq";
    let len = 1 + rng.below(90);
    let text: String = (0..len).map(|_| ALPHABET[rng.below(ALPHABET.len())] as char).collect();

    for chess960 in [false, true] {
        if let Ok(pos) = Position::from_fen(&text, chess960) {
            // Accepted. Then it must behave like a position: generating moves and asking
            // for its key must not panic, and the list must still be well-formed.
            check_move_list(&pos, seed, "fen");
            let _ = pos.key();
        }
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
        // The board invariants and the parser get a share of every budget, not a separate
        // step: they are cheap next to a search, and a soak that only searched would leave
        // the classes below untested for as long as it ran.
        walk_and_check(&mut rng, plies, seed);
        check_fen_parse(&mut rng, seed);

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

/// Play every legal move, undo it, and require the position to be exactly as it was.
///
/// The strongest single invariant in the board zone, and the one whose failures this port
/// has actually paid for: AGENTS.md lists "key identity" among the four bug classes that
/// cost the most, and every one of them is invisible to perft, which only counts leaves and
/// so cannot notice a key that desynced and resynced. Checked per MOVE rather than once per
/// line, so a category-specific fault -- castling, en passant, a promotion -- is attributed
/// to the move that caused it instead of to the line that contained it.
fn check_make_unmake(pos: &mut Position, seed: u64, tag: &str) {
    let before_key = pos.key();
    let before_raw = pos.raw_key();
    let before_board = *pos.board();
    let before_ep = pos.ep_square();
    let before_rule50 = pos.rule50_count();
    let before_checkers = pos.checkers();
    let before_count = legal_moves(pos).len();

    for m in legal_moves(pos) {
        pos.do_move(m);
        pos.undo_move(m);

        assert_eq!(pos.key(), before_key, "seed {seed} {tag}: the table key desynced over {m:?}");
        assert_eq!(pos.raw_key(), before_raw, "seed {seed} {tag}: the raw key desynced over {m:?}");
        assert!(*pos.board() == before_board, "seed {seed} {tag}: the board changed over {m:?}");
        assert_eq!(pos.ep_square(), before_ep, "seed {seed} {tag}: the ep square moved over {m:?}");
        assert_eq!(
            pos.rule50_count(),
            before_rule50,
            "seed {seed} {tag}: the fifty-move counter moved over {m:?}",
        );
        assert_eq!(pos.checkers(), before_checkers, "seed {seed} {tag}: checkers moved over {m:?}");
        assert_eq!(
            legal_moves(pos).len(),
            before_count,
            "seed {seed} {tag}: the legal move count changed over {m:?}",
        );
    }
}

/// A legal move list must contain no null move and no duplicate.
///
/// A duplicate is not a crash and not a wrong perft count -- perft would count the position
/// twice and so would a naive checker. It IS a wrong search: the move picker would search
/// the same move twice and the node count would move.
fn check_move_list(pos: &Position, seed: u64, tag: &str) {
    let list = legal_moves(pos);
    for (i, &m) in list.iter().enumerate() {
        assert!(m != Move::NONE, "seed {seed} {tag}: a null move is in the legal list");
        assert!(
            !list[i + 1..].contains(&m),
            "seed {seed} {tag}: {m:?} appears twice in the legal list",
        );
    }
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

    /// Every legal move of every position along a line makes and unmakes cleanly, and the
    /// line unwinds to the position it started from.
    #[test]
    fn make_and_unmake_restore_the_position_exactly() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..6 {
            walk_and_check(&mut rng, 40, 0x2545_F491_4F6C_DD1D);
        }
    }

    /// A parser reached with nonsense rejects or accepts; it does not panic.
    #[test]
    fn the_fen_parser_survives_nonsense() {
        let mut rng = Rng(0x8A5C_D789_635D_2DFF);
        for _ in 0..4000 {
            check_fen_parse(&mut rng, 0x8A5C_D789_635D_2DFF);
        }
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
