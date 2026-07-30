//! The benchmark: a fixed position list searched to a fixed depth.
//!
//! The total node count it prints is the engine's **signature** — one number that changes
//! whenever any search decision changes, and the anchor every gate in this repository is
//! built around.
//!
//! Four facts decide the number, and every one of them is load-bearing: the position list,
//! the depth, the `Threads` and `Hash` settings, and the fact that a SINGLE `ucinewgame`
//! precedes the whole run rather than one per position — the table and the history block
//! carry across positions.
//!
//! Golden: `Stockfish/src/benchmark.cpp`.

/// The default benchmark entry list.
///
/// Upstream's, verbatim and in upstream's order. Reordering it changes the signature just
/// as surely as changing the search does, because each position starts from the table the
/// previous one left behind.
///
/// An entry is NOT always a FEN. It is one of:
///
/// - `setoption name X value Y` — executed as a command, changing how the entries after it
///   are interpreted. `UCI_Chess960` is toggled around the 960 block this way, and dropping
///   those two lines would make the 960 positions parse under the wrong castling dialect.
/// - a FEN, optionally followed by `moves <uci>...` — the position is set up and the moves
///   played before the search starts, so the searched position is not the one written here.
///
/// Treating every entry as a bare FEN silently drops both, and the node total moves.
pub(crate) const BENCH_ENTRIES: &[&str] = &[
    "setoption name UCI_Chess960 value false",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 10",
    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 11",
    "4rrk1/pp1n3p/3q2pQ/2p1pb2/2PP4/2P3N1/P2B2PP/4RRK1 b - - 7 19",
    "rq3rk1/ppp2ppp/1bnpb3/3N2B1/3NP3/7P/PPPQ1PP1/2KR3R w - - 7 14 moves d4e6",
    "r1bq1r1k/1pp1n1pp/1p1p4/4p2Q/4Pp2/1BNP4/PPP2PPP/3R1RK1 w - - 2 14 moves g2g4",
    "r3r1k1/2p2ppp/p1p1bn2/8/1q2P3/2NPQN2/PPP3PP/R4RK1 b - - 2 15",
    "r1bbk1nr/pp3p1p/2n5/1N4p1/2Np1B2/8/PPP2PPP/2KR1B1R w kq - 0 13",
    "r1bq1rk1/ppp1nppp/4n3/3p3Q/3P4/1BP1B3/PP1N2PP/R4RK1 w - - 1 16",
    "4r1k1/r1q2ppp/ppp2n2/4P3/5Rb1/1N1BQ3/PPP3PP/R5K1 w - - 1 17",
    "2rqkb1r/ppp2p2/2npb1p1/1N1Nn2p/2P1PP2/8/PP2B1PP/R1BQK2R b KQ - 0 11",
    "r1bq1r1k/b1p1npp1/p2p3p/1p6/3PP3/1B2NN2/PP3PPP/R2Q1RK1 w - - 1 16",
    "3r1rk1/p5pp/bpp1pp2/8/q1PP1P2/b3P3/P2NQRPP/1R2B1K1 b - - 6 22",
    "r1q2rk1/2p1bppp/2Pp4/p6b/Q1PNp3/4B3/PP1R1PPP/2K4R w - - 2 18",
    "4k2r/1pb2ppp/1p2p3/1R1p4/3P4/2r1PN2/P4PPP/1R4K1 b - - 3 22",
    "3q2k1/pb3p1p/4pbp1/2r5/PpN2N2/1P2P2P/5PP1/Q2R2K1 b - - 4 26",
    "6k1/6p1/6Pp/ppp5/3pn2P/1P3K2/1PP2P2/3N4 b - - 0 1",
    "3b4/5kp1/1p1p1p1p/pP1PpP1P/P1P1P3/3KN3/8/8 w - - 0 1",
    "2K5/p7/7P/5pR1/8/5k2/r7/8 w - - 0 1 moves g5g6 f3e3 g6g5 e3f3",
    "8/6pk/1p6/8/PP3p1p/5P2/4KP1q/3Q4 w - - 0 1",
    "7k/3p2pp/4q3/8/4Q3/5Kp1/P6b/8 w - - 0 1",
    "8/2p5/8/2kPKp1p/2p4P/2P5/3P4/8 w - - 0 1",
    "8/1p3pp1/7p/5P1P/2k3P1/8/2K2P2/8 w - - 0 1",
    "8/pp2r1k1/2p1p3/3pP2p/1P1P1P1P/P5KR/8/8 w - - 0 1",
    "8/3p4/p1bk3p/Pp6/1Kp1PpPp/2P2P1P/2P5/5B2 b - - 0 1",
    "5k2/7R/4P2p/5K2/p1r2P1p/8/8/8 b - - 0 1",
    "6k1/6p1/P6p/r1N5/5p2/7P/1b3PP1/4R1K1 w - - 0 1",
    "1r3k2/4q3/2Pp3b/3Bp3/2Q2p2/1p1P2P1/1P2KP2/3N4 w - - 0 1",
    "6k1/4pp1p/3p2p1/P1pPb3/R7/1r2P1PP/3B1P2/6K1 w - - 0 1",
    "8/3p3B/5p2/5P2/p7/PP5b/k7/6K1 w - - 0 1",
    "5rk1/q6p/2p3bR/1pPp1rP1/1P1Pp3/P3B1Q1/1K3P2/R7 w - - 93 90",
    "4rrk1/1p1nq3/p7/2p1P1pp/3P2bp/3Q1Bn1/PPPB4/1K2R1NR w - - 40 21",
    "r3k2r/3nnpbp/q2pp1p1/p7/Pp1PPPP1/4BNN1/1P5P/R2Q1RK1 w kq - 0 16",
    "3Qb1k1/1r2ppb1/pN1n2q1/Pp1Pp1Pr/4P2p/4BP2/4B1R1/1R5K b - - 11 40",
    "4k3/3q1r2/1N2r1b1/3ppN2/2nPP3/1B1R2n1/2R1Q3/3K4 w - - 5 1",
    "1r6/1P4bk/3qr1p1/N6p/3pp2P/6R1/3Q1PP1/1R4K1 w - - 1 42",
    "k7/2n1n3/1nbNbn2/2NbRBn1/1nbRQR2/2NBRBN1/3N1N2/7K w - - 0 1",
    "K7/8/8/BNQNQNB1/N5N1/R1Q1q2r/n5n1/bnqnqnbk w - - 0 1",
    "8/8/8/8/5kp1/P7/8/1K1N4 w - - 0 1",
    "8/8/8/5N2/8/p7/8/2NK3k w - - 0 1",
    "8/3k4/8/8/8/4B3/4KB2/2B5 w - - 0 1",
    "8/8/1P6/5pr1/8/4R3/7k/2K5 w - - 0 1",
    "8/2p4P/8/kr6/6R1/8/8/1K6 w - - 0 1",
    "8/8/3P3k/8/1p6/8/1P6/1K3n2 b - - 0 1",
    "8/R7/2q5/8/6k1/8/1P5p/K6R w - - 0 124",
    "6k1/3b3r/1p1p4/p1n2p2/1PPNpP1q/P3Q1p1/1R1RB1P1/5K2 b - - 0 1",
    "r2r1n2/pp2bk2/2p1p2p/3q4/3PN1QP/2P3R1/P4PP1/5RK1 w - - 0 1",
    "8/8/8/8/8/6k1/6p1/6K1 w - -",
    "7k/7P/6K1/8/3B4/8/8/8 b - -",
    "setoption name UCI_Chess960 value true",
    "bbqnnrkr/pppppppp/8/8/8/8/PPPPPPPP/BBQNNRKR w HFhf - 0 1 moves g2g3 d7d5 d2d4 c8h3 c1g5 e8d6 g5e7 f7f6",
    "nqbnrkrb/pppppppp/8/8/8/8/PPPPPPPP/NQBNRKRB w KQkq - 0 1",
    "setoption name UCI_Chess960 value false",
];

/// The default bench depth.
pub(crate) const BENCH_DEPTH: i32 = 13;
/// The default bench hash size, in mebibytes.
pub(crate) const BENCH_HASH: usize = 16;
/// The default bench thread count.
pub(crate) const BENCH_THREADS: usize = 1;

/// How a bench run was parameterised.
#[derive(Clone, Debug)]
pub(crate) struct BenchSpec {
    pub(crate) hash_mb: usize,
    pub(crate) threads: usize,
    pub(crate) limit: i32,
    /// `depth`, `nodes` or `movetime`.
    pub(crate) limit_kind: String,
    /// The entry list, in the form [`BENCH_ENTRIES`] documents.
    pub(crate) entries: Vec<String>,
}

impl Default for BenchSpec {
    fn default() -> BenchSpec {
        BenchSpec {
            hash_mb: BENCH_HASH,
            threads: BENCH_THREADS,
            limit: BENCH_DEPTH,
            limit_kind: "depth".to_string(),
            entries: BENCH_ENTRIES.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

/// One entry, after parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BenchEntry {
    /// A command to run before the entries that follow, such as toggling `UCI_Chess960`.
    Command(String),
    /// A position to search, with the moves to play from it first.
    Position { fen: String, moves: Vec<String> },
}

/// Split an entry into what it actually is.
#[must_use]
pub(crate) fn parse_entry(entry: &str) -> BenchEntry {
    let entry = entry.trim();
    if entry.starts_with("setoption") || entry.starts_with("position") {
        return BenchEntry::Command(entry.to_string());
    }
    match entry.split_once(" moves ") {
        Some((fen, moves)) => BenchEntry::Position {
            fen: fen.trim().to_string(),
            moves: moves.split_ascii_whitespace().map(str::to_string).collect(),
        },
        None => BenchEntry::Position { fen: entry.to_string(), moves: Vec::new() },
    }
}

impl BenchSpec {
    /// Parse `bench [hash] [threads] [limit] [fen-source] [limit-kind]`.
    ///
    /// Every argument is positional and optional, and a missing one takes the default —
    /// upstream's convention, which every harness that drives the bench relies on.
    #[must_use]
    pub(crate) fn parse(args: &[&str]) -> BenchSpec {
        let mut spec = BenchSpec::default();
        if let Some(v) = args.first().and_then(|s| s.parse().ok()) {
            spec.hash_mb = v;
        }
        if let Some(v) = args.get(1).and_then(|s| s.parse().ok()) {
            spec.threads = v;
        }
        if let Some(v) = args.get(2).and_then(|s| s.parse().ok()) {
            spec.limit = v;
        }
        match args.get(3).copied() {
            None | Some("default") => {}
            Some("current") => spec.entries.clear(),
            Some(path) => {
                // Anything else names a file of FENs or EPDs, one per line. A line's first
                // six whitespace-separated fields are the FEN; an EPD's operations follow
                // and are ignored.
                if let Ok(text) = std::fs::read_to_string(path) {
                    spec.entries = text
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(|l| l.split_ascii_whitespace().take(6).collect::<Vec<_>>().join(" "))
                        .collect();
                }
            }
        }
        if let Some(kind) = args.get(4) {
            spec.limit_kind = (*kind).to_string();
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfish_engine::board::movegen::parse_uci_move;
    use rfish_engine::board::position::Position;

    /// Every bench entry must be usable: a command, or a FEN whose moves are all legal. A
    /// malformed entry would silently drop a position and move the signature.
    #[test]
    fn every_bench_entry_parses_and_its_moves_are_legal() {
        let mut chess960 = false;
        for entry in BENCH_ENTRIES {
            match parse_entry(entry) {
                BenchEntry::Command(cmd) => {
                    // The only commands upstream's list carries toggle the castling dialect,
                    // and the entries after them parse under it.
                    if cmd.contains("UCI_Chess960") {
                        chess960 = cmd.ends_with("true");
                    }
                }
                BenchEntry::Position { fen, moves } => {
                    let mut pos =
                        Position::from_fen(&fen, chess960).unwrap_or_else(|e| panic!("{fen}: {e}"));
                    for mv in moves {
                        let m = parse_uci_move(&pos, &mv)
                            .unwrap_or_else(|| panic!("{fen}: {mv} is not legal"));
                        pos.do_move(m);
                    }
                }
            }
        }
    }

    #[test]
    fn the_entry_list_carries_both_castling_dialects() {
        // Dropping the two setoption lines would parse the 960 block under standard rules,
        // where its castling field names no rook -- so the positions would be rejected and
        // the node total would silently shrink.
        let commands: Vec<_> = BENCH_ENTRIES
            .iter()
            .filter(|e| matches!(parse_entry(e), BenchEntry::Command(_)))
            .collect();
        assert_eq!(commands.len(), 3, "upstream toggles UCI_Chess960 three times");
    }

    #[test]
    fn an_entry_with_moves_is_split_from_its_fen() {
        let e = parse_entry("8/8/8/8/8/6k1/6p1/6K1 w - - moves a1a2 b1b2");
        assert_eq!(
            e,
            BenchEntry::Position {
                fen: "8/8/8/8/8/6k1/6p1/6K1 w - -".to_string(),
                moves: vec!["a1a2".to_string(), "b1b2".to_string()],
            }
        );
    }

    #[test]
    fn the_default_spec_is_the_documented_one() {
        let s = BenchSpec::default();
        assert_eq!(s.hash_mb, 16);
        assert_eq!(s.threads, 1);
        assert_eq!(s.limit, 13);
        assert_eq!(s.limit_kind, "depth");
        assert_eq!(s.entries.len(), BENCH_ENTRIES.len());
    }

    #[test]
    fn positional_arguments_fill_in_from_the_left() {
        let s = BenchSpec::parse(&["64", "4", "8"]);
        assert_eq!(s.hash_mb, 64);
        assert_eq!(s.threads, 4);
        assert_eq!(s.limit, 8);
        assert_eq!(s.limit_kind, "depth");

        let s = BenchSpec::parse(&["1", "1", "1000", "default", "nodes"]);
        assert_eq!(s.limit_kind, "nodes");
        assert_eq!(s.limit, 1000);
    }

    #[test]
    fn a_malformed_argument_falls_back_to_the_default() {
        assert_eq!(BenchSpec::parse(&["notanumber"]).hash_mb, BENCH_HASH);
    }

    #[test]
    fn current_means_the_position_the_engine_already_holds() {
        assert!(BenchSpec::parse(&["16", "1", "5", "current"]).entries.is_empty());
    }

    #[test]
    fn a_fen_file_is_read_one_position_per_line() {
        let path = std::env::temp_dir().join(format!("rfish-bench-{}.fens", std::process::id()));
        std::fs::write(
            &path,
            "# a comment\n\
             rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n\
             \n\
             8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 11 bm Kh3; id \"x\";\n",
        )
        .expect("write");
        let s = BenchSpec::parse(&["16", "1", "5", path.to_str().expect("utf-8")]);
        assert_eq!(s.entries.len(), 2);
        // The EPD operations after the sixth field are dropped.
        assert_eq!(s.entries[1], "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 11");
        std::fs::remove_file(&path).ok();
    }
}
